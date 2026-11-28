//! Daemon-side sweep of the forensic session tree (`~/.clud/state/sessions`)
//! — issue #1014.
//!
//! Mirrors `session_tmp_sweep`: the periodic tick in `gc_service.rs` calls
//! [`maybe_sweep_session_state`] every cadence, and a sentinel at
//! `~/.clud/state/session-state-sweep.last` throttles the real work to
//! [`MIN_INTERVAL`]. The retention policy itself lives in
//! [`crate::gc::session_state`]; this file only owns *when* it runs and
//! supplies the liveness predicate, which is daemon-private.
//!
//! All errors are non-fatal — a missed sweep never crashes the daemon.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::gc::session_state;

/// These entries are far smaller and slower-growing than session temp, so the
/// same 6h cadence is generous; it is shared for one reason only — an operator
/// reading `gc_service` should not have to hold two numbers in their head.
pub const MIN_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

const SENTINEL_FILE: &str = "session-state-sweep.last";

/// Production entry point — called from the daemon's periodic tick.
pub fn maybe_sweep_session_state() {
    let Some(state) = state_dir() else {
        return;
    };
    if let Err(e) = maybe_sweep_at(
        &state.join(SENTINEL_FILE),
        &state.join("sessions"),
        SystemTime::now(),
    ) {
        eprintln!("[clud] session-state sweep error: {e}");
    }
}

/// Testable variant. Skips when the sentinel is newer than
/// `now - MIN_INTERVAL`; otherwise sweeps `sessions_root` and rewrites the
/// sentinel.
pub fn maybe_sweep_at(
    sentinel_path: &std::path::Path,
    sessions_root: &std::path::Path,
    now: SystemTime,
) -> std::io::Result<Option<session_state::SweepReport>> {
    if let Some(last) = read_sentinel(sentinel_path) {
        match now.duration_since(last) {
            Ok(age) if age < MIN_INTERVAL => return Ok(None),
            // A sentinel dated in the future (clock was ahead) is invalid:
            // sweep now and rewrite it, or the sweep stays off until real
            // time catches up (#1910).
            _ => {}
        }
    }
    let report = session_state::sweep_at(sessions_root, now, pid_is_alive)?;
    write_sentinel(sentinel_path, now)?;
    log_report(&report);
    Ok(Some(report))
}

/// Force an immediate sweep, ignoring the sentinel throttle. Used by the GC
/// tick's background thread under disk pressure. Still rewrites the sentinel
/// so the throttled path stays consistent.
pub fn sweep_now() {
    let Some(state) = state_dir() else {
        return;
    };
    let now = SystemTime::now();
    match session_state::sweep_at(&state.join("sessions"), now, pid_is_alive) {
        Ok(report) => {
            let _ = write_sentinel(&state.join(SENTINEL_FILE), now);
            log_report(&report);
        }
        Err(e) => eprintln!("[clud] session-state sweep error: {e}"),
    }
}

/// Liveness for invariant 1. Wraps the daemon-private helper so
/// `gc::session_state` stays free of process introspection and testable
/// without spawning anything.
fn pid_is_alive(pid: u32) -> bool {
    super::process_utils::pid_is_alive(pid)
}

fn log_report(report: &session_state::SweepReport) {
    if report.removed > 0 {
        eprintln!(
            "[clud] session-state sweep: removed {} session{}, kept {} live and {} notable, {} skipped",
            report.removed,
            if report.removed == 1 { "" } else { "s" },
            report.kept_live,
            report.kept_notable,
            report.skipped,
        );
    }
}

fn state_dir() -> Option<PathBuf> {
    Some(crate::home::user_home()?.join(".clud").join("state"))
}

fn read_sentinel(path: &std::path::Path) -> Option<SystemTime> {
    let raw = fs::read_to_string(path).ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

fn write_sentinel(path: &std::path::Path, now: SystemTime) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| std::io::Error::other("system clock before UNIX epoch"))?
        .as_secs();
    fs::write(path, secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    #[test]
    fn first_run_writes_sentinel() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let result =
            maybe_sweep_at(&sentinel, &tmp.path().join("sessions"), SystemTime::now()).unwrap();
        assert!(result.is_some(), "first run must execute the sweep");
        assert!(sentinel.exists());
    }

    #[test]
    fn future_sentinel_is_repaired_instead_of_skipping() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let sessions = tmp.path().join("sessions");
        let now = SystemTime::now();
        write_sentinel(&sentinel, now + Duration::from_secs(30 * 24 * 60 * 60)).unwrap();
        assert!(maybe_sweep_at(&sentinel, &sessions, now).unwrap().is_some());
        assert!(read_sentinel(&sentinel).unwrap() <= now);
    }

    #[test]
    fn second_run_within_interval_skips() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let sessions = tmp.path().join("sessions");
        let now = SystemTime::now();
        maybe_sweep_at(&sentinel, &sessions, now).unwrap();
        let soon = now + Duration::from_secs(60 * 60);
        assert!(maybe_sweep_at(&sentinel, &sessions, soon)
            .unwrap()
            .is_none());
    }

    #[test]
    fn run_after_interval_executes_again() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let sessions = tmp.path().join("sessions");
        let now = SystemTime::now();
        maybe_sweep_at(&sentinel, &sessions, now).unwrap();
        let later = now + Duration::from_secs(7 * 60 * 60);
        assert!(maybe_sweep_at(&sentinel, &sessions, later)
            .unwrap()
            .is_some());
    }

    /// The ambient set in `gc::session_state` mirrors the `record_ambient`
    /// call sites in `codex_bridge.rs`, which the on-disk JSONL does not mark.
    /// This is the reminder to update it: adding a `record_ambient` event
    /// without listing it there only costs extra retention, but silently
    /// keeping every session forever is still a bug worth catching.
    #[test]
    fn ambient_event_names_match_record_ambient_call_sites() {
        let bridge = include_str!("../codex_bridge.rs");
        let mut found: Vec<&str> = Vec::new();
        for (index, _) in bridge.match_indices("record_ambient(") {
            // The event name is the first `"event": "..."` after the call.
            let tail = &bridge[index..];
            let Some(key) = tail.find("\"event\":") else {
                continue;
            };
            let after = &tail[key + "\"event\":".len()..];
            let Some(open) = after.find('"') else {
                continue;
            };
            let Some(close) = after[open + 1..].find('"') else {
                continue;
            };
            found.push(&after[open + 1..open + 1 + close]);
        }
        found.sort_unstable();
        found.dedup();
        assert!(
            !found.is_empty(),
            "no record_ambient call sites found — did the helper get renamed?"
        );
        for event in found {
            assert!(
                session_state::ambient_events().contains(&event),
                "`{event}` is recorded as ambient but is missing from \
                 gc::session_state::AMBIENT_EVENTS, so those sessions will be \
                 retained for the notable window instead of the ambient one"
            );
        }
    }
}

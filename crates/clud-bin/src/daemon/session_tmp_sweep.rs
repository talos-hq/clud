//! Issue #509 — daemon-side sweep of the session temp dir (`~/.clud/tmp`).
//!
//! Mirrors `uv_cache_sweep`: the periodic tick in `gc_service.rs` calls
//! [`maybe_sweep_session_tmp`] every cadence; a sentinel at
//! `~/.clud/state/session-tmp-sweep.last` throttles the actual work to
//! [`MIN_INTERVAL`]. When due, it drops entries under `~/.clud/tmp` whose
//! mtime is older than [`crate::gc::session_tmp::STALE_THRESHOLD`] (72h).
//!
//! All errors are non-fatal — a missed sweep never crashes the daemon.

use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fs4::fs_std::FileExt;
use serde_json::json;

use super::daemon_events;
use crate::gc::{session_tmp, session_tmp_cap};

/// Temp accumulates faster than the uv cache, so sweep more often than the
/// daily uv sweep — but still cheap (one stat + age compare) between runs.
pub const MIN_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

const SENTINEL_FILE: &str = "session-tmp-sweep.last";
const WORK_FILE: &str = "session-tmp-sweep.work.json";
const LOCK_FILE: &str = "session-tmp-sweep.lock";
const TICK_ALLOWANCE: usize = 200_000;

/// Production entry point — called from the daemon's periodic tick.
pub fn maybe_sweep_session_tmp() {
    let Some(sentinel) = sentinel_path() else {
        return;
    };
    match maybe_sweep_continuously_at(&sentinel, SystemTime::now()) {
        // The age pass ran (so the cadence is due): follow it with the size cap.
        Ok(Some(_)) => enforce_size_cap(sentinel.parent()),
        Ok(None) => {}
        Err(e) => eprintln!("[clud] session-tmp sweep error: {e}"),
    }
}

/// #2397 (DD-194): evict the oldest idle units while `~/.clud/tmp` is over
/// `tmp.max_bytes`. Runs after the age pass so it sizes what survived.
fn enforce_size_cap(state_dir: Option<&Path>) {
    let report = session_tmp_cap::enforce_cap(false);
    if report.evicted == 0 && report.failed == 0 {
        return;
    }
    eprintln!("[clud] gc: {}", report.summary_line());
    if let Some(line) = report.undeletable.summary_line() {
        eprintln!("[clud] gc: tmp: {line}");
    }
    if let Some(state_dir) = state_dir {
        daemon_events::log_event(
            state_dir,
            "session_tmp_cap",
            [
                ("total_bytes", json!(report.total_bytes)),
                ("cap", json!(report.cap)),
                ("evicted", json!(report.evicted)),
                ("reclaimed_bytes", json!(report.reclaimed_bytes)),
                ("kept_live", json!(report.kept_live)),
                ("failed", json!(report.failed)),
            ],
        );
    }
}

fn maybe_sweep_continuously_at(
    sentinel_path: &Path,
    now: SystemTime,
) -> std::io::Result<Option<session_tmp::SweepReport>> {
    let Some(root) = session_tmp::session_tmp_dir() else {
        return Ok(None);
    };
    maybe_sweep_continuously_at_root(sentinel_path, &root, now, TICK_ALLOWANCE)
}

fn maybe_sweep_continuously_at_root(
    sentinel_path: &Path,
    root: &Path,
    now: SystemTime,
    allowance: usize,
) -> std::io::Result<Option<session_tmp::SweepReport>> {
    let Some(_lock) = try_sweep_lock(sentinel_path)? else {
        return Ok(None);
    };
    let Some(mut report) = maybe_sweep_at_root(sentinel_path, root, now, allowance)? else {
        return Ok(None);
    };
    report = continue_sweep(sentinel_path, root, allowance, report)?;
    Ok(Some(report))
}

fn try_sweep_lock(sentinel_path: &Path) -> std::io::Result<Option<fs::File>> {
    let lock_path = sentinel_path.with_file_name(LOCK_FILE);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    if FileExt::try_lock_exclusive(&file)? {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

fn continue_sweep(
    sentinel_path: &Path,
    root: &Path,
    allowance: usize,
    mut report: session_tmp::SweepReport,
) -> std::io::Result<session_tmp::SweepReport> {
    while report.pending > 0 && report.advanced_steps > 0 {
        report = run_sweep_at_with_quantum(
            sentinel_path,
            root,
            SystemTime::now(),
            allowance,
            allowance,
        )?;
    }
    Ok(report)
}

fn maybe_sweep_at_root(
    sentinel_path: &Path,
    root: &Path,
    now: SystemTime,
    allowance: usize,
) -> std::io::Result<Option<session_tmp::SweepReport>> {
    let work_path = sentinel_path.with_file_name(WORK_FILE);
    if !work_path.exists() {
        if let Some(last) = read_sentinel(sentinel_path) {
            match now.duration_since(last) {
                Ok(age) if age < MIN_INTERVAL => return Ok(None),
                // Clock skew (sentinel in the future) → skip, recover next tick.
                Err(_) => return Ok(None),
                _ => {}
            }
        }
    }
    let report = run_sweep_at(sentinel_path, root, now, allowance)?;
    Ok(Some(report))
}

fn run_sweep_at(
    sentinel_path: &Path,
    root: &Path,
    now: SystemTime,
    allowance: usize,
) -> std::io::Result<session_tmp::SweepReport> {
    run_sweep_at_with_quantum(sentinel_path, root, now, allowance, allowance)
}

fn run_sweep_at_with_quantum(
    sentinel_path: &Path,
    root: &Path,
    now: SystemTime,
    allowance: usize,
    candidate_quantum: usize,
) -> std::io::Result<session_tmp::SweepReport> {
    let work_path = sentinel_path.with_file_name(WORK_FILE);
    let report = session_tmp::sweep_tick_with_quantum(
        root,
        &work_path,
        now,
        false,
        allowance,
        candidate_quantum,
    )?;
    write_sentinel(sentinel_path, now)?;
    if report.pending == 0 {
        // #2486: publish the finished pass's unremovable list for `clud gc`.
        if let Some(state_dir) = sentinel_path.parent() {
            if let Err(err) = session_tmp::write_foreign_owned(state_dir, &report.foreign_owned) {
                eprintln!("[clud] session-tmp: could not record unremovable paths: {err}");
            }
        }
    }
    log_report(&report, sentinel_path.parent());
    Ok(report)
}

/// Force an immediate sweep, ignoring the sentinel throttle. Used by the GC
/// tick's background thread under disk pressure, where we want to reclaim
/// now rather than wait for the next throttle window. Still rewrites the
/// sentinel so the throttled path stays consistent.
pub fn sweep_now() {
    let now = SystemTime::now();
    let Some(sentinel) = sentinel_path() else {
        return;
    };
    let Some(root) = session_tmp::session_tmp_dir() else {
        return;
    };
    let _lock = match try_sweep_lock(&sentinel) {
        Ok(Some(lock)) => lock,
        Ok(None) => return,
        Err(err) => {
            eprintln!("[clud] session-tmp sweep lock error: {err}");
            return;
        }
    };
    match run_sweep_at(&sentinel, &root, now, TICK_ALLOWANCE)
        .and_then(|report| continue_sweep(&sentinel, &root, TICK_ALLOWANCE, report))
    {
        Ok(_) => enforce_size_cap(sentinel.parent()),
        Err(e) => eprintln!("[clud] session-tmp sweep error: {e}"),
    }
}

fn log_report(report: &session_tmp::SweepReport, state_dir: Option<&Path>) {
    if let Some(state_dir) = state_dir {
        daemon_events::log_event(
            state_dir,
            "session_tmp_sweep_progress",
            [
                ("examined", json!(report.examined)),
                ("removed_files", json!(report.removed_files)),
                ("removed_dirs", json!(report.removed_dirs)),
                ("reclaimed_bytes", json!(report.reclaimed_bytes)),
                ("pending_candidates", json!(report.pending)),
                ("advanced_steps", json!(report.advanced_steps)),
                ("current_phase", json!(report.current_phase)),
                (
                    "current_path",
                    json!(report
                        .current_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())),
                ),
                ("cursor_examined", json!(report.cursor_examined)),
                ("pending_explore", json!(report.phases.exploring)),
                ("pending_probe", json!(report.phases.probing)),
                ("pending_scan", json!(report.phases.scanning)),
                ("pending_recheck", json!(report.phases.rechecking)),
                ("pending_file_delete", json!(report.phases.deleting_files)),
                ("pending_dir_delete", json!(report.phases.deleting_dirs)),
                ("failures", json!(report.failures)),
                ("pending_age_secs", json!(report.pending_age_secs)),
                ("no_progress_secs", json!(report.no_progress_secs)),
                ("retry_count", json!(report.retry_count)),
                ("persistent_failure", json!(report.persistent_failure)),
                ("abandoned", json!(report.abandoned)),
                ("foreign_owned", json!(report.foreign_owned.len())),
                (
                    "last_error_path",
                    json!(report
                        .last_error_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())),
                ),
                ("last_error_class", json!(report.last_error_class)),
                ("last_error_message", json!(report.last_error_message)),
            ],
        );
    }
    if report.persistent_failure {
        eprintln!(
            "[clud] session-tmp persistent failure: {} pending for {}h without progress; last error at {} ({}) — retrying",
            report.pending,
            report.no_progress_secs / 3_600,
            report.last_error_path.as_ref().map_or_else(|| "unknown".to_string(), |path| path.display().to_string()),
            report.last_error_class.as_deref().unwrap_or("unknown"),
        );
    }
    if report.abandoned > 0 {
        eprintln!(
            "[clud] session-tmp: gave up on {} entr{} after repeated failures (kept, re-checked next pass); last error at {} ({})",
            report.abandoned,
            if report.abandoned == 1 { "y" } else { "ies" },
            report.last_error_path.as_ref().map_or_else(|| "unknown".to_string(), |path| path.display().to_string()),
            report.last_error_class.as_deref().unwrap_or("unknown"),
        );
    }
    if report.removed > 0 || report.skipped > 0 || report.pending > 0 {
        eprintln!(
            "[clud] session-tmp sweep: examined {}, removed {} entr{}, {} pending, {} failed",
            report.examined,
            report.removed,
            if report.removed == 1 { "y" } else { "ies" },
            report.pending,
            report.failures,
        );
    }
    // #1148: a live session holding tens of gigabytes is not swept — it is in
    // use — but it was also completely silent. One line, largest first, so the
    // 48 GB scratchpad that filled a 2 TB box appears somewhere before the
    // disk does.
    for (path, bytes) in &report.oversized {
        eprintln!(
            "[clud] session-tmp: {} holds {:.1} GB and is still in use (not swept)",
            path.display(),
            *bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        );
    }
}

fn sentinel_path() -> Option<PathBuf> {
    Some(
        crate::home::user_home()?
            .join(".clud/state")
            .join(SENTINEL_FILE),
    )
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
    use tempfile::tempdir;

    #[test]
    fn first_run_writes_sentinel() {
        let tmp = tempdir().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let root = tmp.path().join("tmp");
        let result =
            maybe_sweep_at_root(&sentinel, &root, SystemTime::now(), TICK_ALLOWANCE).unwrap();
        assert!(result.is_some(), "first run must execute the sweep");
        assert!(sentinel.exists());
    }

    #[test]
    fn second_run_within_interval_skips() {
        let tmp = tempdir().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let root = tmp.path().join("tmp");
        let now = SystemTime::now();
        maybe_sweep_at_root(&sentinel, &root, now, TICK_ALLOWANCE).unwrap();
        let soon = now + Duration::from_secs(60 * 60);
        assert!(maybe_sweep_at_root(&sentinel, &root, soon, TICK_ALLOWANCE)
            .unwrap()
            .is_none());
    }

    #[test]
    fn run_after_interval_executes_again() {
        let tmp = tempdir().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let root = tmp.path().join("tmp");
        let now = SystemTime::now();
        maybe_sweep_at_root(&sentinel, &root, now, TICK_ALLOWANCE).unwrap();
        let later = now + Duration::from_secs(7 * 60 * 60);
        assert!(maybe_sweep_at_root(&sentinel, &root, later, TICK_ALLOWANCE)
            .unwrap()
            .is_some());
    }

    #[test]
    fn min_interval_is_6h() {
        assert_eq!(MIN_INTERVAL, Duration::from_secs(6 * 60 * 60));
    }

    #[test]
    fn pending_work_bypasses_start_cadence() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("tmp");
        let candidate = root.join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        let now = SystemTime::now();
        let old = now - session_tmp::STALE_THRESHOLD - Duration::from_secs(3_600);
        for index in 0..5 {
            let path = candidate.join(format!("{index}.txt"));
            fs::write(&path, b"old").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        filetime::set_file_mtime(&candidate, filetime::FileTime::from_system_time(old)).unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let first = maybe_sweep_at_root(&sentinel, &root, now, 1)
            .unwrap()
            .unwrap();
        assert!(first.pending > 0);
        assert!(sentinel.with_file_name(WORK_FILE).exists());
        let second = maybe_sweep_at_root(&sentinel, &root, now + Duration::from_secs(60), 1)
            .unwrap()
            .expect("pending work must resume inside six hours");
        assert!(second.examined > 0 || second.removed > 0 || second.pending > 0);
    }

    #[test]
    fn background_worker_continues_without_waiting_for_another_hourly_tick() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let candidate = root.join("stale-candidate");
        fs::create_dir_all(&candidate).unwrap();
        let now = SystemTime::now();
        let old = now - session_tmp::STALE_THRESHOLD - Duration::from_secs(3_600);
        for index in 0..12 {
            let path = candidate.join(format!("{index}.txt"));
            fs::write(&path, b"old").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        filetime::set_file_mtime(&candidate, filetime::FileTime::from_system_time(old)).unwrap();
        let sentinel = temp.path().join("state").join(SENTINEL_FILE);
        let report = maybe_sweep_continuously_at_root(&sentinel, &root, now, 3)
            .unwrap()
            .unwrap();
        assert_eq!(report.pending, 0);
        assert!(!candidate.exists());
        assert!(!sentinel.with_file_name(WORK_FILE).exists());
    }

    #[test]
    fn second_daemon_does_not_overwrite_an_active_sweep() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let sentinel = temp.path().join("state").join(SENTINEL_FILE);
        let held_lock = try_sweep_lock(&sentinel).unwrap().unwrap();
        assert!(
            maybe_sweep_continuously_at_root(&sentinel, &root, SystemTime::now(), 1)
                .unwrap()
                .is_none()
        );
        assert!(!sentinel.exists(), "the second daemon must not start work");
        drop(held_lock);
        assert!(
            maybe_sweep_continuously_at_root(&sentinel, &root, SystemTime::now(), 1)
                .unwrap()
                .is_some()
        );
    }
}

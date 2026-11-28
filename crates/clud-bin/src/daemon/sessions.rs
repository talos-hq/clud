use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::io_helpers::read_json_file;
use super::paths::{session_snapshot_path, sessions_dir};
use super::process_utils::{identity_alive_in, identity_is_alive, refreshed_minimal_system};
use super::types::SessionSnapshot;

/// A crash-leftover session record is retired only once it is at least this old,
/// so a session that is merely still starting up is never touched (#549).
const RECONCILE_GRACE: Duration = Duration::from_secs(10 * 60);

/// A retired record is kept as a `.json.tombstone` for this long — cheap
/// insurance for `clud logs`-style post-mortem debugging — before deletion.
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Resolve a user-provided session identifier to the canonical session ID.
/// Tries exact match, then name match, then prefix match.
pub(super) fn resolve_session_id(state_dir: &Path, input: &str) -> Result<String, String> {
    // Exact match
    let exact_path = session_snapshot_path(state_dir, input);
    if exact_path.exists() {
        return Ok(input.to_string());
    }

    // Scan all sessions for name match or prefix match
    let Ok(entries) = fs::read_dir(sessions_dir(state_dir)) else {
        return Err(format!("session '{}' not found", input));
    };

    let mut name_matches = Vec::new();
    let mut prefix_matches = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(session) = read_json_file::<SessionSnapshot>(&path) else {
            continue;
        };
        if session.name.as_deref() == Some(input) {
            name_matches.push(session.id.clone());
        }
        if session.id.starts_with(input) {
            prefix_matches.push(session.id.clone());
        }
    }

    if name_matches.len() == 1 {
        return Ok(name_matches.into_iter().next().unwrap());
    }
    if name_matches.len() > 1 {
        return Err(format!(
            "ambiguous name '{}': matches {}",
            input,
            name_matches.join(", ")
        ));
    }
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches.into_iter().next().unwrap());
    }
    if prefix_matches.len() > 1 {
        return Err(format!(
            "ambiguous prefix '{}': matches {}",
            input,
            prefix_matches.join(", ")
        ));
    }

    Err(format!("session '{}' not found", input))
}

/// Return the most recently created active session.
pub(super) fn most_recent_session(state_dir: &Path) -> Option<SessionSnapshot> {
    let sessions = list_attachable_sessions(state_dir);
    sessions
        .into_iter()
        .max_by_key(|s| s.created_at.unwrap_or(0))
}

/// Return the most recently created session, *including exited ones*.
/// Used by `clud logs --last`: a session's log is valuable after it dies,
/// so we look at every snapshot on disk rather than only attachable ones.
pub(super) fn most_recent_session_any(state_dir: &Path) -> Option<SessionSnapshot> {
    let entries = fs::read_dir(sessions_dir(state_dir)).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .filter_map(|p| read_json_file::<SessionSnapshot>(&p).ok())
        .max_by_key(|s| s.created_at.unwrap_or(0))
}

pub(super) fn list_background_sessions(state_dir: &Path) -> Vec<SessionSnapshot> {
    let Ok(entries) = fs::read_dir(sessions_dir(state_dir)) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(session) = read_json_file::<SessionSnapshot>(&path) else {
            continue;
        };
        if !session.background {
            continue;
        }
        if !session_is_live(&session) {
            continue;
        }
        sessions.push(session);
    }
    sessions.sort_by(|left, right| left.id.cmp(&right.id));
    sessions
}

pub(super) fn list_live_session_cwds(state_dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(sessions_dir(state_dir)) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(session) = read_json_file::<SessionSnapshot>(&path) else {
            continue;
        };
        if !session_is_live(&session) {
            continue;
        }
        let Some(cwd) = session.cwd.as_deref() else {
            continue;
        };
        let Ok(canonical) = fs::canonicalize(cwd) else {
            continue;
        };
        paths.push(canonical);
    }
    paths.sort();
    paths.dedup();
    paths
}

pub(super) fn list_attachable_sessions(state_dir: &Path) -> Vec<SessionSnapshot> {
    list_background_sessions(state_dir)
        .into_iter()
        .filter(|session| session.attachable)
        .collect()
}

fn session_is_live(session: &SessionSnapshot) -> bool {
    if session.exit_code.is_some() {
        return false;
    }
    // Identity, not bare PID: a session whose worker exited long ago must not
    // read as live again because the OS reissued its number to an unrelated
    // process (issue #558). `clud kill` and attach both act on this answer.
    if !identity_is_alive(&session.worker_identity()) {
        return false;
    }
    // A repeat worker owns the long-lived job. Its `root_pid` is only the
    // currently running child and can remain stale for a moment after that
    // child exits, before the worker persists its sleeping state. Do not hide
    // the repeat job during that handoff window.
    if session.repeat_interval_secs.is_none() {
        if let Some(root) = session.root_identity() {
            if !identity_is_alive(&root) {
                return false;
            }
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordAction {
    Keep,
    Tombstone,
    DeleteTombstone,
}

/// Decide what to do with a live `<id>.json` session record.
///
/// A record is retired **only** when it is a crash leftover: no clean exit was
/// ever recorded (`exit_code` is `None`), its worker is not alive, its root (if
/// one is recorded) is not alive, and it has aged past the grace period. A
/// cleanly-exited record (`exit_code: Some`) is history owned by other
/// mechanisms and is never touched here; a record with any live process, or one
/// still inside the grace window, is kept. No process is ever terminated and no
/// executable name is ever consulted — the decision is liveness + age only.
fn session_record_action(
    exit_code_none: bool,
    worker_alive: bool,
    root_alive: Option<bool>,
    age: Duration,
    grace: Duration,
) -> RecordAction {
    let both_dead = !worker_alive && root_alive != Some(true);
    if exit_code_none && both_dead && age >= grace {
        RecordAction::Tombstone
    } else {
        RecordAction::Keep
    }
}

/// Decide whether a `.json.tombstone` file has outlived the retention window.
fn tombstone_action(tombstone_age: Duration, retention: Duration) -> RecordAction {
    if tombstone_age >= retention {
        RecordAction::DeleteTombstone
    } else {
        RecordAction::Keep
    }
}

/// Age of a record from its `created_at` (unix-ms), falling back to the file's
/// mtime when the field is absent (records written by an older clud). A missing
/// or unreadable timestamp yields age `0`, which keeps the record (fail-safe).
fn record_age(path: &Path, created_at: Option<u64>, now: SystemTime) -> Duration {
    if let Some(created_ms) = created_at {
        let now_ms = now
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        return Duration::from_millis(now_ms.saturating_sub(created_ms));
    }
    file_age(path, now).unwrap_or(Duration::ZERO)
}

/// Age of a file from its mtime, or `None` if the metadata is unreadable.
fn file_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(modified).ok()
}

/// Retire crash-leftover session records and delete long-expired tombstones.
///
/// Rides the daemon's hourly GC tick (#549). It **never terminates a process**
/// and never matches on executable name: a record is retired purely on recorded
/// liveness plus age. Retiring is a rename `<id>.json` → `<id>.json.tombstone`;
/// every session reader filters on the `.json` extension, so a tombstone is
/// invisible to listings while remaining on disk for post-mortem debugging.
/// Returns `(tombstoned, deleted)` counts. One batched process-table refresh
/// covers all records.
pub(super) fn reconcile_session_records(state_dir: &Path) -> Result<(usize, usize), String> {
    let dir = sessions_dir(state_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(err) => return Err(format!("read {}: {err}", dir.display())),
    };
    let system = refreshed_minimal_system();
    let now = SystemTime::now();

    let mut tombstoned = 0usize;
    let mut deleted = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("json") => {
                let Ok(session) = read_json_file::<SessionSnapshot>(&path) else {
                    continue;
                };
                let worker_alive = identity_alive_in(&system, &session.worker_identity());
                let root_alive = session
                    .root_identity()
                    .map(|root| identity_alive_in(&system, &root));
                let age = record_age(&path, session.created_at, now);
                if session_record_action(
                    session.exit_code.is_none(),
                    worker_alive,
                    root_alive,
                    age,
                    RECONCILE_GRACE,
                ) == RecordAction::Tombstone
                {
                    let target = path.with_extension("json.tombstone");
                    // Windows cannot rename onto an existing path; clear a stale
                    // tombstone from a prior retire of the same id first.
                    let _ = fs::remove_file(&target);
                    match fs::rename(&path, &target) {
                        Ok(()) => tombstoned += 1,
                        Err(err) => eprintln!(
                            "[clud] gc tick: sessions: retire {} failed: {err}",
                            path.display()
                        ),
                    }
                }
            }
            Some("tombstone") => {
                let age = file_age(&path, now).unwrap_or(Duration::ZERO);
                if tombstone_action(age, TOMBSTONE_RETENTION) == RecordAction::DeleteTombstone {
                    match fs::remove_file(&path) {
                        Ok(()) => deleted += 1,
                        Err(err) => eprintln!(
                            "[clud] gc tick: sessions: delete tombstone {} failed: {err}",
                            path.display()
                        ),
                    }
                }
            }
            _ => {}
        }
    }
    Ok((tombstoned, deleted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::io_helpers::write_json_file;
    use crate::daemon::types::SessionKind;
    use kernal_api::platform::fs::TemporaryDirectory;

    fn write_snapshot(state_dir: &Path, id: &str, created_at: u64, exit_code: Option<i32>) {
        write_snapshot_with_cwd_and_pid(state_dir, id, created_at, exit_code, None, 0);
    }

    fn write_snapshot_with_cwd_and_pid(
        state_dir: &Path,
        id: &str,
        created_at: u64,
        exit_code: Option<i32>,
        cwd: Option<String>,
        worker_pid: u32,
    ) {
        let snap = SessionSnapshot {
            id: id.into(),
            kind: SessionKind::Subprocess,
            backend: None,
            launch_mode: None,
            repo_root: None,
            command: Vec::new(),
            cwd,
            name: None,
            created_at: Some(created_at),
            detachable: false,
            background: true,
            attachable: true,
            repeat_interval_secs: None,
            repeat_next_run_at: None,
            repeat_running: false,
            daemon_pid: 0,
            worker_pid,
            worker_port: 0,
            root_pid: None,
            daemon_pid_start: 0,
            worker_pid_start: 0,
            root_pid_start: 0,
            exit_code,
            exited_at: exit_code.map(|_| created_at + 1000),
            ctrl_c: None,
        };
        write_json_file(&session_snapshot_path(state_dir, id), &snap).unwrap();
    }

    #[test]
    fn list_background_sessions_keeps_repeat_worker_during_stale_child_pid_window() {
        let tmp = TemporaryDirectory::new().unwrap();
        let snap = SessionSnapshot {
            id: "repeat-job".into(),
            kind: SessionKind::Subprocess,
            backend: None,
            launch_mode: None,
            repo_root: None,
            command: Vec::new(),
            cwd: None,
            name: Some("repeat background task".into()),
            created_at: Some(1),
            detachable: false,
            background: true,
            attachable: false,
            repeat_interval_secs: Some(1),
            repeat_next_run_at: None,
            repeat_running: true,
            daemon_pid: 0,
            worker_pid: std::process::id(),
            worker_port: 0,
            // The short-lived child exited before the repeat worker persisted
            // its next sleeping state. The worker remains the job's owner.
            root_pid: Some(u32::MAX),
            daemon_pid_start: 0,
            worker_pid_start: 0,
            root_pid_start: 0,
            exit_code: None,
            exited_at: None,
            ctrl_c: None,
        };
        write_json_file(&session_snapshot_path(tmp.path(), "repeat-job"), &snap).unwrap();

        let sessions = list_background_sessions(tmp.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "repeat-job");
    }

    #[test]
    fn a_recycled_worker_pid_does_not_resurrect_a_dead_session() {
        // Issue #558. The snapshot names a PID that really is running -- this
        // very test process -- but pins it to a start time that process never
        // had. That is exactly the shape left behind when a worker exits and
        // the OS hands its number to something unrelated: `pid_is_alive`
        // answers "yes", and the session would come back as attachable, with
        // `clud kill` then aimed at an innocent process.
        //
        // Synthetic on purpose: forcing the OS to actually recycle a PID is
        // not something a test can do deterministically on any platform.
        let tmp = TemporaryDirectory::new().unwrap();
        let live_pid = std::process::id();
        let real_start = crate::process_identity::self_start_time();
        assert!(
            real_start != crate::process_identity::UNKNOWN_START_TIME,
            "this platform must report a start time for the running process"
        );

        write_snapshot_with_cwd_and_pid(tmp.path(), "recycled", 1, None, None, live_pid);
        let path = session_snapshot_path(tmp.path(), "recycled");
        let mut snap = read_json_file::<SessionSnapshot>(&path).unwrap();
        snap.worker_pid_start = real_start.wrapping_add(1);
        write_json_file(&path, &snap).unwrap();
        assert!(!session_is_live(&snap));
        assert!(list_background_sessions(tmp.path()).is_empty());

        // Control: the same record with the true start time is live, so the
        // assertion above is about identity and not about some other field.
        snap.worker_pid_start = real_start;
        write_json_file(&path, &snap).unwrap();
        assert!(session_is_live(&snap));
        assert_eq!(list_background_sessions(tmp.path()).len(), 1);

        // And a record from an older clud, carrying no start time at all,
        // keeps the pre-#558 PID-only behaviour rather than reading as stale.
        snap.worker_pid_start = crate::process_identity::UNKNOWN_START_TIME;
        write_json_file(&path, &snap).unwrap();
        assert!(session_is_live(&snap));
    }

    #[test]
    fn most_recent_session_any_returns_newest_including_exited() {
        // `--last` must surface the most-recently-created session even if
        // it has already exited. `most_recent_session` (the attach helper)
        // filters exited sessions; `most_recent_session_any` does not.
        let tmp = TemporaryDirectory::new().unwrap();
        write_snapshot(tmp.path(), "sess-old", 100, Some(0));
        write_snapshot(tmp.path(), "sess-new", 200, Some(1));
        let found = most_recent_session_any(tmp.path()).expect("should find a session");
        assert_eq!(found.id, "sess-new");
        assert_eq!(found.exit_code, Some(1));
    }

    #[test]
    fn most_recent_session_any_none_when_dir_missing() {
        let tmp = TemporaryDirectory::new().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");
        assert!(most_recent_session_any(&nonexistent).is_none());
    }

    #[test]
    fn list_live_session_cwds_returns_canonical_live_cwds() {
        let tmp = TemporaryDirectory::new().unwrap();
        let live_cwd = tmp.path().join("live");
        let exited_cwd = tmp.path().join("exited");
        std::fs::create_dir_all(&live_cwd).unwrap();
        std::fs::create_dir_all(&exited_cwd).unwrap();

        write_snapshot_with_cwd_and_pid(
            tmp.path(),
            "sess-live",
            1,
            None,
            Some(live_cwd.to_string_lossy().to_string()),
            std::process::id(),
        );
        write_snapshot_with_cwd_and_pid(
            tmp.path(),
            "sess-exited",
            2,
            Some(0),
            Some(exited_cwd.to_string_lossy().to_string()),
            std::process::id(),
        );
        write_snapshot_with_cwd_and_pid(
            tmp.path(),
            "sess-dead-worker",
            3,
            None,
            Some(exited_cwd.to_string_lossy().to_string()),
            u32::MAX,
        );

        let paths = list_live_session_cwds(tmp.path());
        assert_eq!(paths, vec![std::fs::canonicalize(live_cwd).unwrap()]);
    }

    // ---- #549: session-record reconciliation -----------------------------

    const DEAD_PID: u32 = u32::MAX;
    const GRACE: Duration = Duration::from_secs(600);

    fn alive_pid() -> u32 {
        std::process::id()
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[allow(clippy::too_many_arguments)]
    fn write_record(
        state_dir: &Path,
        id: &str,
        created_at: Option<u64>,
        exit_code: Option<i32>,
        worker_pid: u32,
        root_pid: Option<u32>,
    ) {
        let snap = SessionSnapshot {
            id: id.into(),
            kind: SessionKind::Subprocess,
            backend: None,
            launch_mode: None,
            repo_root: None,
            command: Vec::new(),
            cwd: None,
            name: None,
            created_at,
            detachable: false,
            background: true,
            attachable: true,
            repeat_interval_secs: None,
            repeat_next_run_at: None,
            repeat_running: false,
            daemon_pid: 0,
            worker_pid,
            worker_port: 0,
            root_pid,
            daemon_pid_start: 0,
            worker_pid_start: 0,
            root_pid_start: 0,
            exit_code,
            exited_at: exit_code.map(|_| 0),
            ctrl_c: None,
        };
        write_json_file(&session_snapshot_path(state_dir, id), &snap).unwrap();
    }

    fn json_path(state_dir: &Path, id: &str) -> std::path::PathBuf {
        session_snapshot_path(state_dir, id)
    }

    fn tombstone_path(state_dir: &Path, id: &str) -> std::path::PathBuf {
        json_path(state_dir, id).with_extension("json.tombstone")
    }

    #[test]
    fn session_record_action_matrix() {
        let old = GRACE; // exactly at grace counts as past it (>=)
        let young = Duration::from_secs(0);
        use RecordAction::*;
        // (exit_code_none, worker_alive, root_alive, age) -> action
        let cases: &[(bool, bool, Option<bool>, Duration, RecordAction)] = &[
            // The one retirable shape: crash leftover, both dead, past grace.
            (true, false, None, old, Tombstone),
            (true, false, Some(false), old, Tombstone),
            // Any live process keeps it.
            (true, true, None, old, Keep),
            (true, true, Some(false), old, Keep),
            (true, false, Some(true), old, Keep),
            (true, true, Some(true), old, Keep),
            // Cleanly exited is never touched, regardless of liveness/age.
            (false, false, None, old, Keep),
            (false, false, Some(false), old, Keep),
            (false, false, Some(true), old, Keep),
            // Inside the grace window is always kept, even when both are dead.
            (true, false, None, young, Keep),
            (true, false, Some(false), young, Keep),
            // Cleanly exited + young + live: still kept.
            (false, true, Some(true), young, Keep),
        ];
        for (i, &(ec_none, worker, root, age, expected)) in cases.iter().enumerate() {
            assert_eq!(
                session_record_action(ec_none, worker, root, age, GRACE),
                expected,
                "row {i}: ec_none={ec_none} worker={worker} root={root:?} age={age:?}"
            );
        }
    }

    #[test]
    fn tombstone_action_deletes_only_past_retention() {
        let retention = Duration::from_secs(7 * 24 * 60 * 60);
        assert_eq!(
            tombstone_action(Duration::from_secs(0), retention),
            RecordAction::Keep
        );
        assert_eq!(
            tombstone_action(retention - Duration::from_secs(1), retention),
            RecordAction::Keep
        );
        assert_eq!(
            tombstone_action(retention, retention),
            RecordAction::DeleteTombstone
        );
        assert_eq!(
            tombstone_action(retention + Duration::from_secs(1), retention),
            RecordAction::DeleteTombstone
        );
    }

    #[test]
    fn record_with_both_pids_dead_past_grace_is_tombstoned() {
        let tmp = TemporaryDirectory::new().unwrap();
        let created = now_ms() - 20 * 60 * 1000; // 20 min ago
        write_record(
            tmp.path(),
            "crashed",
            Some(created),
            None,
            DEAD_PID,
            Some(DEAD_PID),
        );

        let (tombstoned, deleted) = reconcile_session_records(tmp.path()).unwrap();
        assert_eq!((tombstoned, deleted), (1, 0));
        assert!(!json_path(tmp.path(), "crashed").exists());
        assert!(tombstone_path(tmp.path(), "crashed").exists());
    }

    #[test]
    fn record_with_live_worker_is_kept() {
        let tmp = TemporaryDirectory::new().unwrap();
        let created = now_ms() - 20 * 60 * 1000;
        write_record(
            tmp.path(),
            "live",
            Some(created),
            None,
            alive_pid(),
            Some(DEAD_PID),
        );

        let (tombstoned, deleted) = reconcile_session_records(tmp.path()).unwrap();
        assert_eq!((tombstoned, deleted), (0, 0));
        assert!(json_path(tmp.path(), "live").exists());
    }

    #[test]
    fn record_with_dead_worker_but_live_root_is_kept() {
        let tmp = TemporaryDirectory::new().unwrap();
        let created = now_ms() - 20 * 60 * 1000;
        write_record(
            tmp.path(),
            "root-live",
            Some(created),
            None,
            DEAD_PID,
            Some(alive_pid()),
        );

        let (tombstoned, _deleted) = reconcile_session_records(tmp.path()).unwrap();
        assert_eq!(tombstoned, 0);
        assert!(json_path(tmp.path(), "root-live").exists());
    }

    #[test]
    fn record_younger_than_grace_is_kept() {
        let tmp = TemporaryDirectory::new().unwrap();
        write_record(
            tmp.path(),
            "starting",
            Some(now_ms()),
            None,
            DEAD_PID,
            Some(DEAD_PID),
        );

        let (tombstoned, _deleted) = reconcile_session_records(tmp.path()).unwrap();
        assert_eq!(tombstoned, 0);
        assert!(json_path(tmp.path(), "starting").exists());
    }

    #[test]
    fn cleanly_exited_record_is_never_touched() {
        let tmp = TemporaryDirectory::new().unwrap();
        let created = now_ms() - 20 * 60 * 1000;
        write_record(
            tmp.path(),
            "exited",
            Some(created),
            Some(0),
            DEAD_PID,
            Some(DEAD_PID),
        );

        let (tombstoned, _deleted) = reconcile_session_records(tmp.path()).unwrap();
        assert_eq!(tombstoned, 0);
        assert!(json_path(tmp.path(), "exited").exists());
    }

    #[test]
    fn missing_created_at_falls_back_to_file_mtime() {
        // No created_at, freshly written → mtime is now → inside grace → kept.
        let tmp = TemporaryDirectory::new().unwrap();
        write_record(
            tmp.path(),
            "no-created-at",
            None,
            None,
            DEAD_PID,
            Some(DEAD_PID),
        );

        let (tombstoned, _deleted) = reconcile_session_records(tmp.path()).unwrap();
        assert_eq!(tombstoned, 0);
        assert!(json_path(tmp.path(), "no-created-at").exists());
    }

    #[test]
    fn fresh_tombstone_is_kept() {
        // A tombstone younger than the 7-day retention survives the tick.
        let tmp = TemporaryDirectory::new().unwrap();
        let created = now_ms() - 20 * 60 * 1000;
        write_record(
            tmp.path(),
            "recent",
            Some(created),
            None,
            DEAD_PID,
            Some(DEAD_PID),
        );
        // First tick tombstones it; a second tick must not delete the fresh tombstone.
        assert_eq!(reconcile_session_records(tmp.path()).unwrap(), (1, 0));
        assert_eq!(reconcile_session_records(tmp.path()).unwrap(), (0, 0));
        assert!(tombstone_path(tmp.path(), "recent").exists());
    }

    #[test]
    fn listing_ignores_tombstones() {
        // A tombstoned record is invisible to every session lister.
        let tmp = TemporaryDirectory::new().unwrap();
        let created = now_ms() - 20 * 60 * 1000;
        write_record(
            tmp.path(),
            "dead",
            Some(created),
            None,
            DEAD_PID,
            Some(DEAD_PID),
        );
        write_record(tmp.path(), "alive", Some(created), None, alive_pid(), None);

        reconcile_session_records(tmp.path()).unwrap();

        let live_ids: Vec<String> = list_background_sessions(tmp.path())
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(live_ids, vec!["alive".to_string()]);
        assert_eq!(
            most_recent_session_any(tmp.path()).map(|s| s.id),
            Some("alive".to_string()),
            "the tombstone must not surface as the most recent session"
        );
    }

    #[test]
    fn reconcile_returns_counts_and_ignores_non_session_files() {
        let tmp = TemporaryDirectory::new().unwrap();
        let old = now_ms() - 20 * 60 * 1000;
        write_record(
            tmp.path(),
            "dead-1",
            Some(old),
            None,
            DEAD_PID,
            Some(DEAD_PID),
        );
        write_record(tmp.path(), "dead-2", Some(old), None, DEAD_PID, None);
        write_record(tmp.path(), "keep-live", Some(old), None, alive_pid(), None);
        write_record(
            tmp.path(),
            "keep-young",
            Some(now_ms()),
            None,
            DEAD_PID,
            None,
        );
        // A stray non-session file in the dir must be left alone.
        std::fs::write(sessions_dir(tmp.path()).join("notes.txt"), b"hi").unwrap();

        let (tombstoned, deleted) = reconcile_session_records(tmp.path()).unwrap();
        assert_eq!((tombstoned, deleted), (2, 0));
        assert!(tombstone_path(tmp.path(), "dead-1").exists());
        assert!(tombstone_path(tmp.path(), "dead-2").exists());
        assert!(json_path(tmp.path(), "keep-live").exists());
        assert!(json_path(tmp.path(), "keep-young").exists());
        assert!(sessions_dir(tmp.path()).join("notes.txt").exists());
    }

    #[test]
    fn reconcile_missing_dir_is_ok() {
        let tmp = TemporaryDirectory::new().unwrap();
        let missing = tmp.path().join("no-such-state");
        assert_eq!(reconcile_session_records(&missing).unwrap(), (0, 0));
    }
}

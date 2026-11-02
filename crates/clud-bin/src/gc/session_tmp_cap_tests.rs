use super::super::session_tmp_live::LiveSignals;
use super::*;
use std::time::Duration;

const KIB: u64 = 1024;

/// Write `len` bytes to `root/<rel>/data` and age every mtime in the session
/// dir to `age`. Returns the session dir (`root/<rel>`).
fn session(root: &Path, rel: &str, len: u64, age: Duration, now: SystemTime) -> PathBuf {
    let dir = root.join(rel);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("data");
    fs::write(&file, vec![0u8; len as usize]).unwrap();
    let when = now - age;
    for path in [&file, &dir] {
        set_mtime(path, when);
    }
    dir
}

/// Set `path`'s mtime. Not `File::open(..).set_modified(..)`: on Windows
/// that handle is read-only, so `SetFileTime` fails with "Access is denied"
/// (and a directory cannot be opened at all without
/// `FILE_FLAG_BACKUP_SEMANTICS`). `filetime` opens the handle correctly on
/// every platform.
fn set_mtime(path: &Path, when: SystemTime) {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when)).unwrap();
}

fn hours(n: u64) -> Duration {
    Duration::from_secs(n * 3600)
}

#[test]
fn under_cap_removes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let s = session(tmp.path(), "claude-1/p/old", 10 * KIB, hours(100), now);
    let report = enforce_cap_at(tmp.path(), 100 * KIB, now, false, LiveSignals::nothing_live);
    assert_eq!(report.evicted, 0);
    assert!(s.exists());
}

#[test]
fn cap_zero_disables() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let s = session(tmp.path(), "claude-1/p/old", 10 * KIB, hours(100), now);
    let report = enforce_cap_at(tmp.path(), 0, now, false, LiveSignals::nothing_live);
    assert_eq!(
        report,
        CapReport {
            cap: 0,
            ..Default::default()
        }
    );
    assert!(s.exists());
}

#[test]
fn evicts_oldest_first_until_within_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let oldest = session(tmp.path(), "claude-1/p/a", 10 * KIB, hours(30), now);
    let middle = session(tmp.path(), "claude-1/p/b", 10 * KIB, hours(20), now);
    let newer = session(tmp.path(), "claude-1/p/c", 10 * KIB, hours(10), now);
    let report = enforce_cap_at(tmp.path(), 15 * KIB, now, false, LiveSignals::nothing_live);
    assert_eq!(report.evicted, 2, "{report:?}");
    assert!(!oldest.exists() && !middle.exists());
    assert!(newer.exists(), "stops once within the cap");
}

#[test]
fn a_live_session_survives_even_when_far_over_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    // Huge and actively written: must never be evicted.
    let live = session(
        tmp.path(),
        "claude-1/p/live",
        500 * KIB,
        Duration::from_secs(30),
        now,
    );
    let idle = session(tmp.path(), "claude-1/p/idle", 10 * KIB, hours(50), now);
    let report = enforce_cap_at(tmp.path(), KIB, now, false, LiveSignals::nothing_live);
    assert!(live.join("data").exists(), "live session data deleted");
    assert!(!idle.exists());
    assert_eq!(report.kept_live, 1);
    assert!(report.total_bytes > report.cap);
}

#[test]
fn old_dir_mtime_with_a_fresh_file_inside_is_live() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let s = session(tmp.path(), "claude-1/p/s", 50 * KIB, hours(80), now);
    // The dir mtime is ancient; a nested file was just written.
    let nested = s.join("deep");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("fresh"), b"x").unwrap();
    set_mtime(&s, now - hours(80));
    enforce_cap_at(tmp.path(), KIB, now, false, LiveSignals::nothing_live);
    assert!(s.exists());
}

#[test]
fn a_future_mtime_is_treated_as_live() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let s = session(tmp.path(), "claude-1/p/s", 50 * KIB, Duration::ZERO, now);
    set_mtime(&s.join("data"), now + hours(5));
    enforce_cap_at(tmp.path(), KIB, now, false, LiveSignals::nothing_live);
    assert!(s.exists());
}

#[test]
fn dry_run_reports_without_deleting() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let s = session(tmp.path(), "claude-1/p/old", 50 * KIB, hours(50), now);
    let report = enforce_cap_at(tmp.path(), KIB, now, true, LiveSignals::nothing_live);
    assert_eq!(report.evicted, 1);
    assert!(s.exists());
    assert!(report.summary_line().contains("would evict"));
}

#[test]
fn flat_files_are_units_and_siblings_of_live_sessions_go() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let live = session(
        tmp.path(),
        "claude-1/p/live",
        5 * KIB,
        Duration::from_secs(1),
        now,
    );
    let flat = tmp.path().join("scratch.bin");
    fs::write(&flat, vec![0u8; 50 * KIB as usize]).unwrap();
    set_mtime(&flat, now - hours(40));
    enforce_cap_at(tmp.path(), 10 * KIB, now, false, LiveSignals::nothing_live);
    assert!(!flat.exists());
    assert!(live.exists());
    assert!(tmp.path().join("claude-1/p").exists(), "parents stay");
}

#[test]
fn missing_root_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let report = enforce_cap_at(
        &tmp.path().join("nope"),
        KIB,
        SystemTime::now(),
        false,
        LiveSignals::nothing_live,
    );
    assert_eq!(report.total_bytes, 0);
}

#[test]
fn idle_unit_with_a_live_process_inside_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let parked = session(tmp.path(), "claude-1/p/parked", 50 * KIB, hours(30), now);
    let dead = session(tmp.path(), "claude-1/p/dead", 50 * KIB, hours(40), now);
    let signals = || LiveSignals::new(Some(vec![parked.join("work")]), Some(Default::default()));
    let report = enforce_cap_at(tmp.path(), KIB, now, false, signals);
    assert!(parked.join("data").exists(), "parked live session evicted");
    assert!(!dead.exists(), "the pass continues past a live unit");
    assert_eq!(report.kept_live, 1);
}

#[test]
fn idle_unit_with_a_tracked_session_id_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let tracked = session(tmp.path(), "claude-1/p/sess-42", 50 * KIB, hours(30), now);
    let ids = std::collections::HashSet::from(["sess-42".to_string()]);
    let report = enforce_cap_at(tmp.path(), KIB, now, false, || {
        LiveSignals::new(Some(Vec::new()), Some(ids))
    });
    assert!(tracked.join("data").exists());
    assert_eq!(report.evicted, 0);
}

#[test]
fn unconsultable_signals_fail_safe() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let idle = session(tmp.path(), "claude-1/p/idle", 50 * KIB, hours(30), now);
    let report = enforce_cap_at(tmp.path(), KIB, now, false, LiveSignals::default);
    assert!(idle.join("data").exists());
    assert_eq!(report.kept_live, 1);
}

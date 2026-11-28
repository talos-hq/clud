use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

const DEAD: fn(u32) -> bool = |_| false;
const LIVE: fn(u32) -> bool = |_| true;

/// Build `<root>/<pid>__<epoch>/` with an optional bridge log.
///
/// Ages are expressed by moving `now` forward at the call site rather than
/// rewinding mtimes, matching `session_tmp`'s tests — no extra dependency and
/// no reliance on the filesystem's mtime granularity.
fn session(root: &std::path::Path, name: &str, bridge: Option<&str>) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("reap.jsonl"), "{}\n").unwrap();
    if let Some(body) = bridge {
        fs::write(dir.join("bridge.jsonl"), body).unwrap();
    }
    dir
}

fn ambient_log() -> String {
    [
        r#"{"ts_ms":1,"event":"catalog_advertised","count":7}"#,
        r#"{"ts_ms":2,"event":"admission_queued"}"#,
        r#"{"ts_ms":3,"event":"admission_acquired"}"#,
    ]
    .join("\n")
        + "\n"
}

fn later(offset: Duration) -> SystemTime {
    SystemTime::now() + offset
}

#[test]
fn parses_only_the_session_dir_shape() {
    assert_eq!(
        parse_session_dir_name("47180__1737390000"),
        Some((47180, 1737390000))
    );
    assert_eq!(parse_session_dir_name("47180"), None);
    assert_eq!(parse_session_dir_name("not__a_number"), None);
    assert_eq!(parse_session_dir_name("__"), None);
    assert_eq!(parse_session_dir_name("api-sessions"), None);
}

#[test]
fn a_log_of_pure_ambient_chatter_is_ambient() {
    let tmp = TemporaryDirectory::new().unwrap();
    let dir = session(tmp.path(), "1__1", Some(&ambient_log()));
    assert_eq!(classify(&dir), Retention::Ambient);
}

#[test]
fn one_non_ambient_record_makes_the_whole_log_notable() {
    let tmp = TemporaryDirectory::new().unwrap();
    let body = ambient_log() + r#"{"ts_ms":4,"event":"model_refused","model":"gpt-9"}"#;
    let dir = session(tmp.path(), "1__1", Some(&body));
    assert_eq!(classify(&dir), Retention::Notable);
}

#[test]
fn an_unreadable_or_malformed_log_is_treated_as_notable() {
    // Drift and corruption both fail toward keeping the trail: discarding a
    // forensic log on a parse guess is the one unrecoverable mistake here.
    let tmp = TemporaryDirectory::new().unwrap();
    let torn = session(
        tmp.path(),
        "1__1",
        Some("{\"event\":\"catalog_advertised\"}\n{not json"),
    );
    assert_eq!(classify(&torn), Retention::Notable);

    let no_event = session(tmp.path(), "2__2", Some("{\"ts_ms\":1}\n"));
    assert_eq!(classify(&no_event), Retention::Notable);
}

#[test]
fn a_session_without_a_bridge_log_is_ambient() {
    let tmp = TemporaryDirectory::new().unwrap();
    let dir = session(tmp.path(), "1__1", None);
    assert_eq!(classify(&dir), Retention::Ambient);
}

#[test]
fn a_live_session_is_never_swept_however_old() {
    // Invariant 1. The reaper and the bridge write into this directory for
    // the whole life of the launch; age says nothing about whether it is in
    // use.
    let tmp = TemporaryDirectory::new().unwrap();
    let dir = session(tmp.path(), "1__1", Some(&ambient_log()));

    let report = sweep_at(tmp.path(), later(NOTABLE_STALE_AFTER * 10), LIVE).unwrap();

    assert_eq!(report.removed, 0);
    assert_eq!(report.kept_live, 1);
    assert!(dir.is_dir());
}

#[test]
fn a_stale_ambient_session_is_swept() {
    let tmp = TemporaryDirectory::new().unwrap();
    let dir = session(tmp.path(), "1__1", Some(&ambient_log()));

    let report = sweep_at(
        tmp.path(),
        later(AMBIENT_STALE_AFTER + Duration::from_secs(3600)),
        DEAD,
    )
    .unwrap();

    assert_eq!(report.removed, 1);
    assert!(!dir.exists());
}

#[test]
fn a_fresh_ambient_session_survives() {
    let tmp = TemporaryDirectory::new().unwrap();
    let dir = session(tmp.path(), "1__1", Some(&ambient_log()));

    let report = sweep_at(tmp.path(), later(Duration::from_secs(60)), DEAD).unwrap();

    assert_eq!(report.removed, 0);
    assert!(dir.is_dir());
}

#[test]
fn a_failure_outlives_the_ambient_window_but_not_the_notable_one() {
    // Invariant 2, both halves in one test so the two windows cannot silently
    // collapse into a single threshold.
    let tmp = TemporaryDirectory::new().unwrap();
    let body = ambient_log() + r#"{"ts_ms":4,"event":"upstream_failed","status":500}"#;
    let failed = session(tmp.path(), "1__1", Some(&body));
    let ambient = session(tmp.path(), "2__2", Some(&ambient_log()));

    let report = sweep_at(
        tmp.path(),
        later(AMBIENT_STALE_AFTER + Duration::from_secs(3600)),
        DEAD,
    )
    .unwrap();
    assert_eq!(report.removed, 1, "the ambient neighbour goes");
    assert_eq!(report.kept_notable, 1, "the failure stays");
    assert!(failed.is_dir());
    assert!(!ambient.exists());

    let report = sweep_at(
        tmp.path(),
        later(NOTABLE_STALE_AFTER + Duration::from_secs(3600)),
        DEAD,
    )
    .unwrap();
    assert_eq!(report.removed, 1, "but the window is still bounded");
    assert!(!failed.exists());
}

#[test]
fn foreign_entries_and_files_are_left_alone() {
    // `state/` holds siblings like `api-sessions/`; a sweep that guessed at
    // unrecognized names would be reaching outside its own tree.
    let tmp = TemporaryDirectory::new().unwrap();
    let foreign = tmp.path().join("api-sessions");
    fs::create_dir_all(&foreign).unwrap();
    let loose = tmp.path().join("daemon.json");
    fs::write(&loose, "{}").unwrap();

    let report = sweep_at(tmp.path(), later(NOTABLE_STALE_AFTER * 2), DEAD).unwrap();

    assert_eq!(report.removed, 0);
    assert!(foreign.is_dir());
    assert!(loose.is_file());
}

#[test]
fn a_missing_root_is_a_successful_no_op() {
    let tmp = TemporaryDirectory::new().unwrap();
    let report = sweep_at(&tmp.path().join("nope"), SystemTime::now(), DEAD).unwrap();
    assert_eq!(report, SweepReport::default());
}

/// Issue #893: a live repo directory vanished with nothing in any log naming
/// what removed it, and #996 answered that with a pre-deletion audit line from
/// every destructive sweep. This sweep landed alongside that work and was
/// missed, so it deleted forensic session directories silently — the one thing
/// #893 exists to prevent.
///
/// Mirrors `target_sweep::sweep_writes_pre_deletion_audit_line`, including its
/// tolerance for unrelated concurrent deletions sharing the log file.
#[test]
fn sweeping_writes_a_pre_deletion_audit_line() {
    use crate::gc::delete_audit::{StateDirGuard, AUDIT_LOG_FILE};

    let tmp = TemporaryDirectory::new().unwrap();
    let state = TemporaryDirectory::new().unwrap();
    let _guard = StateDirGuard::set(state.path());
    let dir = session(tmp.path(), "1__1", Some(&ambient_log()));

    let report = sweep_at(
        tmp.path(),
        later(AMBIENT_STALE_AFTER + Duration::from_secs(3600)),
        DEAD,
    )
    .unwrap();

    assert_eq!(report.removed, 1);
    assert!(!dir.exists());

    let body = fs::read_to_string(state.path().join(AUDIT_LOG_FILE)).unwrap();
    let ours = body
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|v| {
            v["site"] == "gc.session-state"
                && v["path"].as_str().is_some_and(|p| p.ends_with("1__1"))
        })
        .expect("a gc.session-state line for this test's session must exist");
    assert_eq!(ours["rule"], "session-state ambient stale>48h");
    assert!(
        ours["path"].as_str().unwrap().ends_with("1__1"),
        "the line names the exact directory removed: {:?}",
        ours["path"]
    );
}

/// The two retention windows are separate rules, and the audit line has to say
/// which one fired — otherwise "why was my failure trail deleted?" is
/// unanswerable from the log alone.
#[test]
fn the_audit_rule_distinguishes_the_two_retention_windows() {
    use crate::gc::delete_audit::{StateDirGuard, AUDIT_LOG_FILE};

    let tmp = TemporaryDirectory::new().unwrap();
    let state = TemporaryDirectory::new().unwrap();
    let _guard = StateDirGuard::set(state.path());
    let body_with_failure = ambient_log() + r#"{"ts_ms":4,"event":"upstream_failed"}"#;
    session(tmp.path(), "2__2", Some(&body_with_failure));

    let report = sweep_at(
        tmp.path(),
        later(NOTABLE_STALE_AFTER + Duration::from_secs(3600)),
        DEAD,
    )
    .unwrap();
    assert_eq!(report.removed, 1);

    let log = fs::read_to_string(state.path().join(AUDIT_LOG_FILE)).unwrap();
    let ours = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|v| {
            v["site"] == "gc.session-state"
                && v["path"].as_str().is_some_and(|p| p.ends_with("2__2"))
        })
        .expect("a gc.session-state line for this test's session must exist");
    assert_eq!(ours["rule"], "session-state notable stale>30d");
}

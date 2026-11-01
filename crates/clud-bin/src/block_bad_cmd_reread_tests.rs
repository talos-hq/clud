use super::*;
use serde_json::json;
use tempfile::tempdir;

fn entry(len: u64, secs: u64, nanos: u32, at: u64) -> Entry {
    Entry {
        len,
        secs,
        nanos,
        at,
        name: "service.py".to_string(),
    }
}

#[test]
fn first_read_is_recorded_then_the_unchanged_repeat_is_denied() {
    let current = entry(79_497, 1_700_000_000, 5, 100);
    assert_eq!(decide(None, &current), Verdict::Record);
    assert_eq!(
        decide(Some(&entry(79_497, 1_700_000_000, 5, 40)), &current),
        Verdict::Deny {
            ago_secs: 60,
            kb: 77
        }
    );
}

#[test]
fn a_changed_file_allows_the_read_again() {
    let current = entry(79_497, 1_700_000_099, 0, 100);
    // mtime moved, so the agent edited the file: never refuse that read.
    assert_eq!(
        decide(Some(&entry(79_497, 1_700_000_000, 5, 40)), &current),
        Verdict::Record
    );
    // Same mtime but a different length is also a change.
    assert_eq!(
        decide(Some(&entry(79_400, 1_700_000_099, 0, 40)), &current),
        Verdict::Record
    );
}

#[test]
fn read_tool_spellings_are_guarded_and_shell_tools_are_not() {
    for tool in ["Read", "read", "read_file", "ReadFile"] {
        assert!(applies_to(tool), "{tool}");
    }
    for tool in ["Bash", "Edit", "Write", "Grep", "Glob", "apply_patch"] {
        assert!(!applies_to(tool), "{tool}");
    }
}

#[test]
fn kill_switch_accepts_zero_and_off_only() {
    for value in [Some("0"), Some("off"), Some("OFF"), Some(" off ")] {
        assert!(disabled_from(value), "{value:?}");
    }
    for value in [Some("1"), Some("on"), Some(""), None] {
        assert!(!disabled_from(value), "{value:?}");
    }
}

#[test]
fn a_request_needs_a_path() {
    let dir = tempdir().unwrap();
    assert!(request_for("Read", Some(&json!({"offset": 10}))).is_none());
    assert!(request_for("Read", None).is_none());
    let file = dir.path().join("a.py");
    std::fs::write(&file, "x").unwrap();
    let built =
        request_for("Read", Some(&json!({"file_path": file.to_str().unwrap()}))).expect("path");
    assert_eq!(built.path, file);
    assert_eq!(built.stamp.expect("file exists").len, 1);
}

#[test]
fn a_different_range_is_a_different_key() {
    let whole = request_for("Read", Some(&json!({"file_path": "/tmp/a.py"})));
    let ranged = request_for(
        "Read",
        Some(&json!({"file_path": "/tmp/a.py", "offset": 100, "limit": 50})),
    );
    let other = request_for(
        "Read",
        Some(&json!({"file_path": "/tmp/a.py", "offset": 300, "limit": 50})),
    );
    assert_ne!(
        whole.as_ref().map(|r| &r.key),
        ranged.as_ref().map(|r| &r.key)
    );
    assert_ne!(
        ranged.as_ref().map(|r| &r.key),
        other.as_ref().map(|r| &r.key)
    );
}

fn big_file(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("service.py");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn check_at_denies_only_the_unchanged_second_read() {
    let state = tempdir().unwrap();
    let work = tempdir().unwrap();
    let path = big_file(work.path(), &"x".repeat(9_000));
    let request =
        request_for("Read", Some(&json!({"file_path": path.to_str().unwrap()}))).expect("req");

    assert_eq!(
        check_at(state.path(), "sess", &request, 1_000),
        Verdict::Record
    );
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_060),
        Verdict::Deny {
            ago_secs: 60,
            kb: 8
        }
    );
    // Another session has its own ledger.
    assert_eq!(
        check_at(state.path(), "other", &request, 1_060),
        Verdict::Record
    );
}

#[test]
fn check_at_allows_after_an_edit_and_for_small_files() {
    let state = tempdir().unwrap();
    let work = tempdir().unwrap();
    let path = big_file(work.path(), &"x".repeat(9_000));
    let request =
        request_for("Read", Some(&json!({"file_path": path.to_str().unwrap()}))).expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_000),
        Verdict::Record
    );

    // An edit moves both the length and the mtime.
    std::fs::write(&path, "y".repeat(9_100)).unwrap();
    let edited =
        request_for("Read", Some(&json!({"file_path": path.to_str().unwrap()}))).expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &edited, 1_030),
        Verdict::Record
    );

    let small = work.path().join("small.py");
    std::fs::write(&small, "y".repeat(1_000)).unwrap();
    let small_request =
        request_for("Read", Some(&json!({"file_path": small.to_str().unwrap()}))).expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &small_request, 1_040),
        Verdict::Allow
    );
    assert_eq!(
        check_at(state.path(), "sess", &small_request, 1_050),
        Verdict::Allow
    );
}

#[test]
fn a_missing_file_fails_open() {
    let state = tempdir().unwrap();
    let request = request_for(
        "Read",
        Some(&json!({"file_path": "/nonexistent/does-not-exist.py"})),
    )
    .expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_000),
        Verdict::Allow
    );
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_001),
        Verdict::Allow
    );
}

#[test]
fn the_ledger_is_capped() {
    let mut ledger = Ledger::default();
    for index in 0..(MAX_ENTRIES + 10) {
        ledger
            .entries
            .insert(format!("key-{index}"), entry(9_000, 1, 0, index as u64));
    }
    trim_entries(&mut ledger);
    assert_eq!(ledger.entries.len(), MAX_ENTRIES);
    assert!(!ledger.entries.contains_key("key-0"));
    assert!(ledger
        .entries
        .contains_key(&format!("key-{}", MAX_ENTRIES + 9)));
}

#[test]
fn the_refusal_names_the_remedies_and_stays_small() {
    let request = Request {
        key: "k".to_string(),
        path: PathBuf::from("/dev/zach/src/booking/service.py"),
        stamp: None,
    };
    let message = deny_message(&request, 90, 77);
    assert!(message.starts_with("[clud reread-guard] service.py (77 kB)"));
    assert!(message.contains("1min ago"));
    assert!(message.contains("offset"));
    assert!(message.contains("code/outline.py"));
    assert!(message.contains("CLUD_REREAD_GUARD=0"));
    // The refusal is the whole cost of the call: keep it under 600 bytes.
    assert!(message.len() < 600, "{} bytes", message.len());
}

#[test]
fn ago_text_is_compact() {
    assert_eq!(ago_text(5), "just now");
    assert_eq!(ago_text(30), "30s");
    assert_eq!(ago_text(90), "1min");
    assert_eq!(ago_text(7_200), "2h");
    assert_eq!(ago_text(172_800), "2d");
}

#[test]
fn the_hook_path_denies_through_the_state_dir_env() {
    let state = tempdir().unwrap();
    let work = tempdir().unwrap();
    let path = big_file(work.path(), &"x".repeat(9_000));
    let input = json!({"file_path": path.to_str().unwrap()});
    // SAFETY: single-threaded assertion window; the guard reads the env once
    // per call and this test owns both calls.
    unsafe { std::env::set_var("CLUD_DAEMON_STATE_DIR", state.path()) };
    let first = reason("Read", Some(&input), Some("sess-hook"));
    assert_eq!(first, None, "first read must pass");
    let second = reason("Read", Some(&input), Some("sess-hook"));
    unsafe { std::env::remove_var("CLUD_DAEMON_STATE_DIR") };
    let second = second.expect("unchanged second read must be refused");
    assert!(second.contains("reread-guard"), "{second}");
}

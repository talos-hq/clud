use super::*;
use serde_json::json;
use tempfile::tempdir;

fn spill(home: &str) -> PathBuf {
    PathBuf::from(format!(
        "{home}/.claude/projects/-home-niteris-dev-cludbot/9c77d7d7-0e9e-4015-aa92-dbb478c4ef86/\
         tool-results/b3w53ef92.txt"
    ))
}

#[test]
fn a_claude_spill_file_is_a_persisted_output() {
    assert!(is_persisted_output(&spill("/home/niteris")));
    assert!(is_persisted_output(Path::new(
        "/root/.claude/projects/-p/abc/tool-results/b1m.txt"
    )));
}

#[test]
fn a_projects_own_tool_results_directory_is_not() {
    // A repository may keep a directory with the same name; only the path
    // under `.claude` is a harness artifact.
    assert!(!is_persisted_output(Path::new(
        "/home/niteris/dev/clud/tool-results/x.txt"
    )));
    assert!(!is_persisted_output(Path::new(
        "/home/niteris/dev/foo/tool-results"
    )));
    // The session directory itself is not a spill file.
    assert!(!is_persisted_output(Path::new(
        "/home/niteris/.claude/projects/-p/abc/x.txt"
    )));
}

#[test]
fn only_a_whole_file_read_is_guarded() {
    assert!(!is_ranged(Some(&json!({"file_path": "/a/b.txt"}))));
    assert!(is_ranged(Some(
        &json!({"file_path": "/a/b.txt", "offset": 40})
    )));
    assert!(is_ranged(Some(
        &json!({"file_path": "/a/b.txt", "limit": 150})
    )));
    assert!(is_ranged(Some(
        &json!({"file_path": "/a/b.txt", "max_lines": 80})
    )));
    // A null value is not a range.
    assert!(!is_ranged(Some(
        &json!({"file_path": "/a/b.txt", "limit": null})
    )));
    assert!(!is_ranged(None));
}

#[test]
fn the_first_whole_read_is_refused_and_the_retry_passes() {
    let mut ledger = Ledger::default();
    assert_eq!(decide(&mut ledger, "abc", 1_000), Verdict::Deny);
    assert_eq!(decide(&mut ledger, "abc", 1_001), Verdict::Retry);
    // A different artifact is its own read.
    assert_eq!(decide(&mut ledger, "def", 1_002), Verdict::Deny);
}

#[test]
fn a_refusal_expires_so_the_file_can_be_read_whole_later() {
    let mut ledger = Ledger::default();
    assert_eq!(decide(&mut ledger, "abc", 1_000), Verdict::Deny);
    assert_eq!(
        decide(&mut ledger, "abc", 1_000 + MAX_AGE_SECS + 1),
        Verdict::Deny
    );
}

#[test]
fn the_ledger_stays_bounded() {
    let mut ledger = Ledger::default();
    for index in 0..(MAX_ENTRIES + 10) {
        decide(&mut ledger, &format!("key-{index}"), 1_000 + index as u64);
    }
    assert!(ledger.refused.len() <= MAX_ENTRIES);
}

#[test]
fn refusals_are_recorded_per_session() {
    let dir = tempdir().expect("tempdir");
    let path = spill("/home/niteris");
    assert_eq!(check_at(dir.path(), "sess-a", &path, 1_000), Verdict::Deny);
    assert_eq!(check_at(dir.path(), "sess-a", &path, 1_001), Verdict::Retry);
    // Another session has not been refused this file yet.
    assert_eq!(check_at(dir.path(), "sess-b", &path, 1_002), Verdict::Deny);
    // An empty session id allows and writes nothing.
    assert_eq!(check_at(dir.path(), "", &path, 1_003), Verdict::Allow);
}

#[test]
fn an_unwritable_state_dir_allows() {
    let dir = tempdir().expect("tempdir");
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, b"not a directory").expect("write file");
    // `state_dir/blocked/...` cannot be created under a file: fail open.
    assert_eq!(
        check_at(&blocked, "sess", &spill("/home/niteris"), 1_000),
        Verdict::Allow
    );
}

#[test]
fn the_message_names_a_bounded_read() {
    let message = deny_message(&spill("/home/niteris"), 35_988);
    assert!(message.contains("persisted-read-guard"), "{message}");
    assert!(message.contains("35 KB"), "{message}");
    assert!(message.contains("offset"), "{message}");
    assert!(message.contains("code/slice.py"), "{message}");
    assert!(message.contains(DISABLE_ENV), "{message}");
}

#[test]
fn the_guard_is_off_by_request() {
    assert!(disabled_from(Some("0")));
    assert!(disabled_from(Some("off")));
    assert!(disabled_from(Some(" OFF ")));
    assert!(!disabled_from(Some("1")));
    assert!(!disabled_from(None));
}

#[test]
fn read_spellings_are_guarded() {
    for tool in ["Read", "read", "read_file", "ReadFile"] {
        assert!(applies_to(tool), "{tool}");
    }
    for tool in ["Bash", "Write", "Edit", "Grep"] {
        assert!(!applies_to(tool), "{tool}");
    }
}

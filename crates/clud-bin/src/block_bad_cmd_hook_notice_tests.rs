use super::*;
use kernal_api::platform::fs::TemporaryDirectory;

fn lines(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_string()).collect()
}

#[test]
fn identical_failures_collapse_into_one_counted_line() {
    let collapsed = collapse(&lines(&["a failed", "b failed", "a failed", "a failed"]));
    assert_eq!(collapsed, lines(&["a failed (x3)", "b failed"]));
}

#[test]
fn a_session_sees_each_distinct_message_once() {
    let state = TemporaryDirectory::new().unwrap();
    let first = unseen_at(state.path(), "s1", lines(&["a failed", "b failed"]));
    assert_eq!(first, lines(&["a failed", "b failed"]));
    let second = unseen_at(state.path(), "s1", lines(&["a failed", "c failed"]));
    assert_eq!(second, lines(&["c failed"]));
    let other = unseen_at(state.path(), "s2", lines(&["a failed"]));
    assert_eq!(
        other,
        lines(&["a failed"]),
        "sessions do not share a ledger"
    );
}

#[test]
fn unreadable_state_shows_the_notice_rather_than_hiding_it() {
    let state = TemporaryDirectory::new().unwrap();
    let blocker = state.path().join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let shown = unseen_at(&blocker, "s1", lines(&["a failed"]));
    assert_eq!(shown, lines(&["a failed"]));
    let again = unseen_at(&blocker, "s1", lines(&["a failed"]));
    assert_eq!(
        again,
        lines(&["a failed"]),
        "nothing was recorded, so it shows again"
    );
}

#[test]
fn merge_adds_context_beside_an_existing_decision() {
    let allow = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "additionalContext": "earlier note",
        }
    });
    let merged = merge(allow, "PreToolUse", "hook note");
    let specific = &merged["hookSpecificOutput"];
    assert_eq!(specific["permissionDecision"], "allow");
    assert_eq!(specific["additionalContext"], "earlier note\n\nhook note");
}

#[test]
fn merge_into_nothing_builds_a_context_document_for_the_event() {
    let merged = merge(Value::Null, "PostToolUse", "hook note");
    assert_eq!(merged["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert_eq!(
        merged["hookSpecificOutput"]["additionalContext"],
        "hook note"
    );
}

#[test]
fn take_drains_what_was_stashed() {
    stash(lines(&["x failed", "x failed"]));
    let note = take(None).expect("a note");
    assert!(note.contains("x failed (x2)"), "{note}");
    assert_eq!(take(None), None, "drained");
}

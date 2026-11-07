use super::*;

fn stop(active: bool, message: &str) -> Value {
    serde_json::json!({
        "session_id": "s-1",
        "hook_event_name": "Stop",
        "stop_hook_active": active,
        "last_assistant_message": message,
    })
}

#[test]
fn identical_hook_driven_continuations_tell_the_user_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = "Blocked: the user must run rm -rf on that path.";
    assert_eq!(observe_in(dir.path(), &stop(true, blocked)), None);
    let told = observe_in(dir.path(), &stop(true, blocked)).unwrap();
    assert!(
        told.contains("systemMessage") && told.contains("/goal clear"),
        "{told}"
    );
    for _ in 0..5 {
        assert_eq!(observe_in(dir.path(), &stop(true, blocked)), None);
    }
}

#[test]
fn a_different_answer_keeps_re_prompting_normally() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..6 {
        let text = format!("progress step {n}");
        assert_eq!(observe_in(dir.path(), &stop(true, &text)), None);
    }
}

#[test]
fn a_user_driven_stop_resets_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let same = "same";
    assert_eq!(observe_in(dir.path(), &stop(true, same)), None);
    assert_eq!(observe_in(dir.path(), &stop(false, same)), None);
    assert_eq!(observe_in(dir.path(), &stop(true, same)), None);
    assert!(observe_in(dir.path(), &stop(true, same)).is_some());
}

#[test]
fn unsafe_session_ids_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let mut payload = stop(true, "x");
    payload["session_id"] = "../escape".into();
    assert_eq!(observe_in(dir.path(), &payload), None);
    assert_eq!(observe_in(dir.path(), &payload), None);
    assert!(!dir.path().join("stop-repeat").exists());
}

#[test]
fn step_is_pure() {
    let (one, tell) = step(State::default(), Some("a"));
    assert!(!tell);
    let (two, tell) = step(one, Some("a"));
    assert!(tell);
    assert_eq!(two.count, 2);
    assert_eq!(step(two, None), (State::default(), false));
}

#[test]
fn the_budget_ends_the_turn_once_and_names_every_way_out() {
    let dir = tempfile::tempdir().unwrap();
    let differ = |n: u32| format!("progress step {n}");
    for n in 0..3 {
        assert_eq!(
            observe_in_with_budget(dir.path(), &stop(true, &differ(n)), 4),
            None
        );
    }
    let halted = observe_in_with_budget(dir.path(), &stop(true, &differ(3)), 4).unwrap();
    let decision: Value = serde_json::from_str(&halted).unwrap();
    assert_eq!(decision["continue"], serde_json::json!(false));
    let reason = decision["stopReason"].as_str().unwrap();
    assert!(reason.contains("4 Stop-hook continuations"), "{reason}");
    assert!(reason.contains("CLUD_STOP_LOOP_BUDGET"), "{reason}");
    assert!(reason.contains("/goal clear"), "{reason}");
    // Emitted exactly once: a harness that ignores `continue: false` must not
    // get a second line, and must not be re-halted on every later stop.
    for n in 4..8 {
        assert_eq!(
            observe_in_with_budget(dir.path(), &stop(true, &differ(n)), 4),
            None
        );
    }
}

#[test]
fn a_budget_of_zero_leaves_the_harness_in_charge() {
    let dir = tempfile::tempdir().unwrap();
    let same = "the same blocked answer";
    assert_eq!(
        observe_in_with_budget(dir.path(), &stop(true, same), 0),
        None
    );
    // The repeat advisory still fires; the halt never does.
    assert!(observe_in_with_budget(dir.path(), &stop(true, same), 0).is_some());
    for _ in 0..40 {
        let line = observe_in_with_budget(dir.path(), &stop(true, same), 0);
        assert_eq!(line, None);
    }
}

#[test]
fn a_user_driven_stop_reopens_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let differ = |n: u32| format!("round {n}");
    for n in 0..2 {
        assert_eq!(
            observe_in_with_budget(dir.path(), &stop(true, &differ(n)), 3),
            None
        );
    }
    let halted = observe_in_with_budget(dir.path(), &stop(true, &differ(2)), 3).unwrap();
    assert!(halted.contains("\"continue\":false"));
    // The human typed something: the loop is theirs again.
    assert_eq!(
        observe_in_with_budget(dir.path(), &stop(false, ""), 3),
        None
    );
    for n in 0..2 {
        assert_eq!(
            observe_in_with_budget(dir.path(), &stop(true, &differ(10 + n)), 3),
            None
        );
    }
    assert!(
        observe_in_with_budget(dir.path(), &stop(true, &differ(12)), 3)
            .unwrap()
            .contains("\"continue\":false")
    );
}

#[test]
fn a_legacy_state_file_loads_as_an_unspent_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stop-repeat").join("s-1.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"hash":123,"count":3}"#).unwrap();
    let same = "unchanged";
    // The legacy file carries no `total`: the budget starts unspent, so the
    // first two continuations stay advisory-only and the third halts.
    assert_eq!(
        observe_in_with_budget(dir.path(), &stop(true, same), 3),
        None
    );
    // The legacy file's `count` belongs to a different message hash, so the
    // advisory restarts at its second round rather than carrying the old count.
    assert!(observe_in_with_budget(dir.path(), &stop(true, same), 3)
        .unwrap()
        .contains("systemMessage"));
    let decision: Value =
        serde_json::from_str(&observe_in_with_budget(dir.path(), &stop(true, same), 3).unwrap())
            .unwrap();
    assert_eq!(decision["continue"], serde_json::json!(false));
}

#[test]
fn the_budget_env_parses_defensively() {
    assert_eq!(budget_from(None), DEFAULT_BUDGET);
    assert_eq!(budget_from(Some("0")), 0);
    assert_eq!(budget_from(Some(" 3 ")), 3);
    assert_eq!(budget_from(Some("many")), DEFAULT_BUDGET);
    assert_eq!(budget_from(Some("-1")), DEFAULT_BUDGET);
}

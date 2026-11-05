use super::*;
use serde_json::json;

fn observe(state: &mut State, command: &str, now: u64) -> Verdict {
    match detect(command) {
        Some((key, lane)) => state.observe(key, lane, now),
        None => Verdict::Silent,
    }
}

#[test]
fn first_direct_call_advises_and_identical_shape_retries_pass() {
    let mut state = State::default();
    assert_eq!(
        observe(&mut state, "cargo test -p clud filter", 100),
        Verdict::Advisory("prefer: clud gate test".into())
    );
    assert_eq!(
        observe(&mut state, "cargo test -p clud filter", 101),
        Verdict::Silent
    );
    assert_eq!(
        observe(&mut state, "cargo test -p other other_filter", 102),
        Verdict::Silent
    );
}

#[test]
fn second_distinct_call_denies_once_with_exact_lane_then_stays_silent() {
    for (command, lane) in [
        ("cargo clippy", "lint"),
        ("pytest", "test"),
        ("ruff check . && pytest", "both"),
    ] {
        let mut state = State::default();
        observe(&mut state, "cargo test", 100);
        assert_eq!(
            observe(&mut state, command, 101),
            Verdict::Deny(format!("clud gate {lane}"))
        );
        assert_eq!(observe(&mut state, command, 102), Verdict::Silent);
        assert_eq!(
            observe(&mut state, "cargo nextest run", 103),
            Verdict::Silent
        );
    }
}

#[test]
fn gate_is_never_matched() {
    for command in [
        "clud gate both",
        "cargo test && clud gate test",
        "echo 'clud gate' && pytest",
    ] {
        assert_eq!(detect(command), None, "{command}");
    }
}

#[test]
fn heredoc_and_quoted_arguments_are_never_matched() {
    for command in [
        "cat <<'EOF'\ncargo test\nEOF",
        "bash <<EOF\ncargo test\nEOF",
        "echo 'cargo test'",
        "printf '%s' \"cargo test\"",
        "bash -c 'cargo test'",
        "echo \"$(cargo test)\"",
        "'cargo' test",
        "cargo 'test'",
        "cargo test && '",
    ] {
        assert_eq!(detect(command), None, "{command}");
    }
}

#[test]
fn bypass_allows_silently_without_consuming_a_signature() {
    let mut state = State::default();
    for command in [
        "CLUD_GATE_BYPASS=1 cargo test",
        "env CLUD_GATE_BYPASS=1 pytest",
        "CLUD_READ=1 cargo clippy",
        "env CLUD_READ=1 pytest",
        "CLUD_GATE_BYPASS='1' pytest",
    ] {
        assert_eq!(
            observe(&mut state, command, 100),
            Verdict::Silent,
            "{command}"
        );
    }
    assert_eq!(
        observe(&mut state, "pytest", 101),
        Verdict::Advisory("prefer: clud gate test".into())
    );
}

#[test]
fn signatures_reset_after_session_window() {
    let mut state = State::default();
    observe(&mut state, "cargo test", 100);
    observe(&mut state, "pytest", 101);
    assert_eq!(
        observe(&mut state, "cargo test", 102 + RESET_SECS),
        Verdict::Advisory("prefer: clud gate test".into())
    );
}

#[test]
fn bash_registration_reaches_the_gate_guard() {
    let fragment = crate::clud_hooks_compile::deletion_safety_fragment();
    let entry = &fragment["hooks"]["PreToolUse"][0];
    assert!(entry["matcher"]
        .as_str()
        .unwrap()
        .split('|')
        .any(|name| name == "Bash"));
    assert_eq!(entry["hooks"][0]["command"], "clud-cmd-scan");
    assert!(include_str!("block_bad_cmd.rs")
        .contains("block_bad_cmd_gate_guard::verdict(&payload, raw_payload)"));
}

#[test]
fn required_runners_infer_lanes() {
    for (command, lane) in [
        ("cargo test", "test"),
        ("soldr cargo nextest run", "test"),
        ("cargo clippy", "lint"),
        ("cargo fmt --check", "lint"),
        ("ruff check .", "lint"),
        ("pytest", "test"),
        ("npm test", "test"),
        ("pnpm test", "test"),
        ("yarn test", "test"),
        ("./lint", "lint"),
        ("cargo xtask lint", "lint"),
        ("eslint .", "lint"),
        ("vitest", "test"),
        ("soldr cargo clippy && soldr cargo nextest run", "both"),
    ] {
        assert_eq!(
            detect(command).map(|(_, lane)| lane),
            Some(lane),
            "{command}"
        );
    }
    for command in [
        "cargo build",
        "cargo fmt",
        "ruff format .",
        "echo pytest",
        "npm run build",
    ] {
        assert_eq!(detect(command), None, "{command}");
    }
}

fn record(id: &str, command: &str) -> String {
    json!({"type":"assistant", "sessionId":"s", "timestamp":"2026-11-03T00:00:00.123Z",
        "message":{"content":[{"type":"tool_use", "name":"Bash", "id":id,
            "input":{"command":command}}]}})
    .to_string()
}

#[test]
fn read_only_replay_recovers_signatures_across_hook_invocations() {
    let now = timestamp(&json!("2026-11-03T00:00:01Z")).unwrap();
    let first = record("one", "cargo test");
    let current = record("two", "cargo clippy");
    let history = format!("{first}\n{first}\n{current}\n");
    let mut state = replay(&history, "s", Some("two"), now).unwrap();
    assert_eq!(
        observe(&mut state, "cargo clippy", now),
        Verdict::Deny("clud gate lint".into())
    );
    let mut state = replay(&history, "s", None, now).unwrap();
    assert_eq!(observe(&mut state, "pytest", now), Verdict::Silent);
    let mut other = replay(&history, "other", None, now).unwrap();
    assert!(matches!(
        observe(&mut other, "pytest", now),
        Verdict::Advisory(_)
    ));
    assert!(replay("broken", "s", None, now).is_none());
    assert!(replay(&first, "s", None, 1).is_none());
}

#[test]
fn sidechain_calls_do_not_consume_main_session_advisory() {
    let now = timestamp(&json!("2026-11-03T00:00:01Z")).unwrap();
    let mut sidechain: Value = serde_json::from_str(&record("child", "cargo test")).unwrap();
    sidechain["isSidechain"] = json!(true);
    let mut state = replay(&sidechain.to_string(), "s", None, now).unwrap();
    assert_eq!(
        observe(&mut state, "pytest", now),
        Verdict::Advisory("prefer: clud gate test".into())
    );
}

#[test]
fn production_entry_fails_open_and_replays_without_writing_state() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir
        .path()
        .join(format!("clud{}", std::env::consts::EXE_SUFFIX));
    let executable = std::env::current_exe().unwrap();
    if std::fs::hard_link(&executable, &binary).is_err() {
        std::fs::copy(&executable, &binary).unwrap();
    }
    let transcript = dir.path().join("session.jsonl");
    std::fs::write(&transcript, record("one", "cargo test")).unwrap();
    let raw = json!({"tool_name":"Bash", "session_id":"s", "cwd":dir.path(),
        "transcript_path":transcript, "tool_use_id":"two",
        "tool_input":{"command":"cargo clippy"}})
    .to_string();
    let payload = super::super::parse_payload(&raw, dir.path()).unwrap();
    let raw: Value = serde_json::from_str(&raw).unwrap();
    let now = timestamp(&json!("2026-11-03T00:00:01Z")).unwrap();
    let path_env = dir.path().to_string_lossy().into_owned();
    let env = |name: &str| (name == "PATH").then(|| path_env.clone());
    let before = std::fs::read(&transcript).unwrap();
    assert_eq!(
        check(&payload, &raw, env, now),
        Some(Verdict::Deny("clud gate lint".into()))
    );
    assert_eq!(std::fs::read(&transcript).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    assert_eq!(check(&payload, &raw, |_| None, now), None); // missing PATH/binary
    assert_eq!(check(&payload, &raw, |_| Some(String::new()), now), None);
    for flag in ["CLUD_READ", "CLUD_GATE_BYPASS"] {
        assert_eq!(
            check(
                &payload,
                &raw,
                |name| {
                    if name == flag {
                        Some("1".into())
                    } else {
                        env(name)
                    }
                },
                now
            ),
            None
        );
    }
    std::fs::write(&transcript, "malformed").unwrap();
    assert_eq!(check(&payload, &raw, env, now), None);
    std::fs::write(&transcript, " ".repeat(MAX_HISTORY_BYTES as usize + 1)).unwrap();
    assert_eq!(check(&payload, &raw, env, now), None);
    let missing = dir.path().join("missing.jsonl");
    let payload = HookPayloadView {
        transcript_path: Some(missing),
        ..payload
    };
    assert_eq!(check(&payload, &raw, env, now), None);
}

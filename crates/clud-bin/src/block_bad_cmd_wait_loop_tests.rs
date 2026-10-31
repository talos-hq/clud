//! zackees/clud#1880/#2004: hand-written waits are refused; use a verified
//! process-session wait or background completion notification. Fixtures are the exact commands from the
//! issue's session and its transcript analysis.

use super::tests::temp_env;
use super::*;

/// The words every refusal must carry so the agent learns the replacement.
fn assert_names_replacement(command: &str, reason: &str) {
    for needle in [
        "run_in_background",
        "completion notification",
        "clud ci",
        "bosn ci run --wait",
        "pr_merge_watch",
    ] {
        assert!(
            reason.contains(needle),
            "{command}: refusal must name {needle:?}: {reason}"
        );
    }
}

fn denial(command: &str, dialect: ShellDialect) -> String {
    let mut reason = None;
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        reason = evaluate_command_with_policy_and_dialect(command, None, false, &[], &[], dialect)
            .reason;
    });
    reason.unwrap_or_else(|| panic!("{command} should be denied"))
}

fn allowed(command: &str) -> bool {
    let mut reason = Some(String::new());
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        reason = evaluate_command(command, None, false, &[]).reason;
    });
    reason.is_none()
}

/// The four loops from the 2026-10-06 session (issue table rows 1-4).
const SESSION_LOOPS: &[&str] = &[
    // 1: main agent, hit the 600 s tool cap.
    r#"until grep -qE "Finished|error" build.output; do sleep 5; done"#,
    // 2: main agent, already blocked by the PR-polling rule.
    r#"until [ "$(gh run view 38054481154 --json status -q .status)" = completed ]; do sleep 20; done"#,
    // 3: sub-agent, matched its own command line for 7 h.
    r#"while pgrep -f "pr_merge_watch.py -- --no-cancel 1867" >/dev/null; do sleep 30; done"#,
    // 4: sub-agent, the verdict was cut off by `tail -30` for 7 h.
    r#"until grep -qE "PASS|FAIL|rror" /tmp/test-output.txt; do sleep 15; done"#,
];

#[test]
fn session_loops_are_refused_and_name_the_replacement() {
    for command in SESSION_LOOPS {
        let reason = denial(command, ShellDialect::Posix);
        assert_names_replacement(command, &reason);
    }
}

#[test]
fn other_waiting_constructs_are_refused_and_name_the_replacement() {
    for (command, dialect) in [
        ("while true; do echo tick; done", ShellDialect::Posix),
        ("for ((;;)); do date; done", ShellDialect::Posix),
        (
            "for ((i=0;i<10;i++)); do test -f done.flag && break; sleep 6; done",
            ShellDialect::Posix,
        ),
        (
            "for i in $(seq 1 20); do test -f out.txt && break; sleep 10; done",
            ShellDialect::Posix,
        ),
        ("for f in a b c; do sleep 1; done", ShellDialect::Posix),
        ("tail -f build.log", ShellDialect::Posix),
        ("tail -n 30 -F build.log", ShellDialect::Posix),
        ("tail --follow=name build.log", ShellDialect::Posix),
        ("watch -n 5 gh pr checks 1867", ShellDialect::Posix),
        (
            r#"while ps aux | grep -q "[p]r_merge_watch"; do sleep 10; done"#,
            ShellDialect::Posix,
        ),
        ("sleep 120", ShellDialect::Posix),
        ("sleep 2m", ShellDialect::Posix),
        ("sleep infinity", ShellDialect::Posix),
        ("sleep $DELAY", ShellDialect::Posix),
        (
            "foreach ($i in 1..10) { Test-Path x; Start-Sleep 5 }",
            ShellDialect::PowerShell,
        ),
        (
            "while ($true) { Get-Content x.log; Start-Sleep 5 }",
            ShellDialect::PowerShell,
        ),
    ] {
        let reason = denial(command, dialect);
        assert_names_replacement(command, &reason);
    }
}

#[test]
fn literal_list_loops_and_one_shot_commands_stay_allowed() {
    for command in [
        // The issue's sanctioned shape: a literal job list, no sleep.
        r#"for j in static-checks lint-linux-x64 test-linux-x64-unit; do bosn ci run --workspace . --trigger pr --job "$j" --wait; done"#,
        "for pr in 101 102; do gh pr checks \"$pr\"; done",
        "gh pr checks 528",
        "gh run view 38054481154 --json status,conclusion",
        "gh api repos/zackees/clud/actions/runs/123/jobs",
        "gh pr view 515 --json mergeStateStatus -q .mergeStateStatus",
        "git status",
        "grep -qE 'PASS|FAIL' /tmp/test-output.txt",
        "pgrep -f pr_merge_watch.py",
        "tail -n 30 build.log",
        "timeout 60 tail -f build.log",
        // DD: one short sleep outside a loop, not chained to a poll, is allowed.
        "sleep 5",
        "sleep 60",
        "sleep 2 && curl -s localhost:8080/health",
        // Data, not commands.
        "git commit -m 'never write: while true; do sleep 5; done'",
        "rg 'tail -f' docs/",
        // #2489: a `while read` loop ends at EOF.
        "ls ~/.claude/skills/*/SKILL.md | while read f; do grep -L x \"$f\"; done",
        "python - <<'PY'\nprint('until grep -q x f; do sleep 5; done')\nPY",
    ] {
        assert!(allowed(command), "{command} should be allowed");
    }
}

#[test]
fn hand_written_wait_override_is_accepted_with_a_reason() {
    let command = SESSION_LOOPS[0];
    let mut reason = Some(String::new());
    temp_env(
        BAD_CMD_OVERRIDE_ENV,
        "hand-written-wait:reproducing #1880 in a scratch shell",
        || {
            reason = evaluate_command(command, None, false, &[]).reason;
        },
    );
    assert!(reason.is_none(), "override should allow: {reason:?}");

    let mut reason = None;
    temp_env(BAD_CMD_OVERRIDE_ENV, "hand-written-wait:", || {
        reason = evaluate_command(command, None, false, &[]).reason;
    });
    let reason = reason.expect("an override without a reason is rejected");
    assert!(reason.contains("hand-written-wait"), "{reason}");
    assert!(reason.contains(BAD_CMD_OVERRIDE_ENV), "{reason}");
}

/// The hook judges `payload.command` whatever agent issued it: a sub-agent's
/// Bash call (the harness sets `agent_id` / `agent_type`) is refused exactly
/// like the main agent's. Loops 3 and 4 came from sub-agents.
#[test]
fn sub_agent_bash_calls_are_refused_too() {
    for agent in ["", r#","agent_id":"a1","agent_type":"general-purpose""#] {
        for command in SESSION_LOOPS {
            let raw = format!(
                r#"{{"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":"/tmp","tool_input":{{"command":{}}}{agent}}}"#,
                serde_json::to_string(command).unwrap()
            );
            let payload = parse_payload(&raw, Path::new("/tmp")).expect("payload parses");
            let mut decision = None;
            temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
                decision = Some(decision_from_payload(&payload, &[]));
            });
            match decision.unwrap() {
                Decision::Deny { reason } => assert_names_replacement(command, &reason),
                Decision::Allow => panic!("{command} from agent {agent:?} should be denied"),
            }
        }
    }
}

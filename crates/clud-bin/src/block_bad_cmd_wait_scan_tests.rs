//! zackees/clud#1972: every hand-rolled PR/CI watch in the audit corpus is
//! refused, and the legitimate commands beside them are not. The corpus is
//! `tests/fixtures/wait_loop/corpus.json`: real transcript commands, one per
//! distinct shape, each with the tool that ran it.

use super::block_bad_cmd_wait_scan::{
    heredoc_scripts, inline_script, peel_wrappers, python_sleep_seconds, python_wait, script_file,
    HeredocUse, ScriptKind,
};
use super::tests::temp_env;
use super::*;

const CORPUS: &str = include_str!("../tests/fixtures/wait_loop/corpus.json");

/// The first command's words, as the evaluated syntax tree reads them.
fn words(text: &str) -> Vec<String> {
    parse_command(text, ShellDialect::Posix, None)
        .invocations
        .first()
        .map(|invocation| invocation.argv())
        .unwrap_or_default()
}

/// The decision the hook reaches for one corpus entry, with its files
/// written to a scratch cwd and no gh-shim session.
fn corpus_reason(entry: &Value, dir: &Path) -> Option<String> {
    if let Some(files) = entry.get("files").and_then(Value::as_object) {
        for (name, body) in files {
            std::fs::write(dir.join(name), body.as_str().unwrap_or_default())
                .expect("write corpus file");
        }
    }
    let command = entry["command"].as_str().expect("command");
    let mut reason = None;
    temp_env(crate::shim_registry::GH_ACTIVE_KEY, "0", || {
        reason = evaluate_command_with_policy_and_dialect(
            command,
            Some(dir),
            false,
            &[],
            &[],
            ShellDialect::Posix,
        )
        .reason;
    });
    reason
}

#[test]
fn corpus_positives_are_refused_and_negatives_allowed() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("corpus is JSON");
    let mut failures = Vec::new();
    for (kind, refuse) in [("positives", true), ("negatives", false)] {
        let entries = corpus[kind].as_array().expect("entry list");
        assert!(entries.len() >= 30, "{kind}: the corpus shrank");
        for entry in entries {
            let tool = entry["tool"].as_str().expect("tool");
            assert!(
                crate::clud_hooks_compile::GUARDED_TOOLS.contains(&tool),
                "{tool} never reaches the scanner"
            );
            let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
            let reason = corpus_reason(entry, dir.path());
            let shape = entry["shape"].as_str().unwrap_or_default();
            match (refuse, reason) {
                (true, None) => failures.push(format!("allowed positive: {shape}")),
                (false, Some(reason)) => {
                    failures.push(format!("refused negative: {shape}: {reason}"));
                }
                (true, Some(reason)) => {
                    for needle in ["pr_merge_watch", "run_in_background", "clud ci"] {
                        if !reason.contains(needle) {
                            failures.push(format!("{shape}: refusal lacks {needle:?}"));
                        }
                    }
                }
                (false, None) => {}
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn pr_refusals_say_how_to_run_pr_merge_watch() {
    let mut reason = None;
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        reason = evaluate_command(
            "timeout 590 bash -c 'while gh pr checks 5 | grep -q pending; do sleep 20; done'",
            None,
            false,
            &[],
        )
        .reason;
    });
    let reason = reason.expect("refused");
    assert!(
        reason.contains("clud tool run github/pr_merge_watch.py -- <PR>"),
        "{reason}"
    );
    assert!(reason.contains("hand-written-wait"), "{reason}");
}

#[test]
fn override_still_accepts_a_wrapped_loop() {
    let mut reason = Some(String::new());
    temp_env(
        BAD_CMD_OVERRIDE_ENV,
        "hand-written-wait:reproducing #1972",
        || {
            reason = evaluate_command(
                "timeout 60 bash -c 'until gh run view 1 | grep -q completed; do sleep 5; done'",
                None,
                false,
                &[],
            )
            .reason;
        },
    );
    assert!(reason.is_none(), "{reason:?}");
}

#[test]
fn peel_wrappers_finds_the_real_program() {
    for (command, program, timed) in [
        ("timeout 590 gh run watch 1", "gh", true),
        (
            "timeout -s INT --kill-after=5 900 gh run watch 1",
            "gh",
            true,
        ),
        ("timeout -k 5 60 gh run watch 1", "gh", true),
        ("nohup gh run watch 1", "gh", false),
        ("GH_PAGER=cat gh run watch 1", "gh", false),
        ("env -u GH_TOKEN A=1 gh run watch 1", "gh", false),
        ("stdbuf -oL gh run watch 1", "gh", false),
        ("stdbuf -o L gh run watch 1", "gh", false),
        ("nice -n 10 gh run watch 1", "gh", false),
        ("ionice -c 3 gh run watch 1", "gh", false),
        ("setsid -f sh -c 'x'", "sh", false),
        ("xargs -r -I{} gh run watch {}", "gh", false),
        ("xargs -n 1 gh run cancel", "gh", false),
        ("command gh run watch 1", "gh", false),
        ("exec gh run watch 1", "gh", false),
        ("time -p gh run watch 1", "gh", false),
        ("nohup timeout 60 env A=1 bash -c 'x'", "bash", true),
        ("cargo build", "cargo", false),
    ] {
        let peeled = peel_wrappers(&words(command));
        assert_eq!(program_name(&peeled.words[0]), program, "{command}");
        assert_eq!(peeled.timed, timed, "{command}");
    }
}

#[test]
fn inline_script_extracts_c_payloads() {
    for (command, kind, needle) in [
        (
            "bash -c 'while true; do sleep 1; done'",
            ScriptKind::Shell(ShellDialect::Posix),
            "while",
        ),
        (
            "sh -lc 'until x; do sleep 1; done'",
            ScriptKind::Shell(ShellDialect::Posix),
            "until",
        ),
        (
            "zsh -c 'sleep 5'",
            ScriptKind::Shell(ShellDialect::Posix),
            "sleep",
        ),
        (
            "eval 'sleep 5'",
            ScriptKind::Shell(ShellDialect::Posix),
            "sleep",
        ),
        (
            "python3 -c 'import time;time.sleep(2)'", // python-name-lint: allow
            ScriptKind::Python,
            "time.sleep",
        ),
    ] {
        let (found, script) = inline_script(&words(command), ShellDialect::Posix).expect(command);
        assert_eq!(found, kind, "{command}");
        assert!(script.contains(needle), "{command}: {script}");
    }
    // A script path is not a `-c` payload, even though `watch` holds a `c`.
    assert!(inline_script(&words("bash ./watch.sh 1650"), ShellDialect::Posix).is_none());
    assert!(inline_script(&words("python wait.py"), ShellDialect::Posix).is_none());
}

#[test]
fn python_sleep_counts_as_a_sleep_only_without_its_own_loop() {
    assert_eq!(
        python_sleep_seconds(&words("python -c 'import time;time.sleep(20)'")),
        Some(Some(20.0))
    );
    assert_eq!(
        python_sleep_seconds(&words("python -c 'import time;time.sleep(n)'")),
        Some(None)
    );
    assert_eq!(python_sleep_seconds(&words("python -c 'print(1)'")), None);
    assert_eq!(python_sleep_seconds(&words("sleep 5")), None);
}

#[test]
fn python_wait_needs_a_loop_and_a_sleep_and_flags_gh_reads() {
    let poll = "import subprocess,time\nwhile True:\n    subprocess.run(['gh','pr','checks','1'])\n    time.sleep(30)\n";
    assert_eq!(python_wait(poll), Some(true));
    // DD-189: a run read is a wait, but not a PR poll.
    let run = "import subprocess,time\nwhile True:\n    subprocess.run(['gh','run','view','1'])\n    time.sleep(30)\n";
    assert_eq!(python_wait(run), Some(false));
    let local = "import time\nfor i in range(3):\n    time.sleep(1)\n";
    assert_eq!(python_wait(local), Some(false));
    assert_eq!(python_wait("import time\ntime.sleep(1)\n"), None);
    // Replacement code inside triple quotes is data.
    let edit = "s=s.replace('''a''','''\nwhile True:\n    gh run view 1\n    time.sleep(5)\n''')\n";
    assert_eq!(python_wait(edit), None);
    // A loop written inside an ordinary string is not a line of code.
    assert_eq!(python_wait("print('while True: time.sleep(1)')\n"), None);
}

#[test]
fn heredoc_scripts_classify_fed_and_written_bodies() {
    let cases = [
        (
            "bash <<'EOF'\nsleep 1\nEOF",
            Some((HeredocUse::Fed, ScriptKind::Shell(ShellDialect::Posix))),
        ),
        (
            "python3 - <<'PY'\nprint(1)\nPY", // python-name-lint: allow
            Some((HeredocUse::Fed, ScriptKind::Python)),
        ),
        (
            "cat > /tmp/s/w.sh <<'EOF'\nsleep 1\nEOF",
            Some((HeredocUse::Written, ScriptKind::Shell(ShellDialect::Posix))),
        ),
        (
            "S=/tmp; cat > \"$S/w.py\" <<'EOF'\nimport time\nEOF",
            Some((HeredocUse::Written, ScriptKind::Python)),
        ),
        (
            "cat <<'EOF' > run.sh\nsleep 1\nEOF",
            Some((HeredocUse::Written, ScriptKind::Shell(ShellDialect::Posix))),
        ),
        (
            "tee /tmp/x <<'EOF'\n#!/bin/bash\nsleep 1\nEOF",
            Some((HeredocUse::Written, ScriptKind::Shell(ShellDialect::Posix))),
        ),
        (
            "git commit -F - <<'EOF'\nwhile true; do sleep 1; done\nEOF",
            None,
        ),
        (
            "cat > notes.md <<'EOF'\nwhile true; do sleep 1; done\nEOF",
            None,
        ),
        ("python x.py <<'EOF'\nstdin data\nEOF", None),
    ];
    for (command, expected) in cases {
        let found = heredoc_scripts(command)
            .into_iter()
            .next()
            .map(|(usage, kind, _)| (usage, kind));
        assert_eq!(found, expected, "{command}");
    }
}

#[test]
fn script_file_reads_only_literal_existing_scripts() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    std::fs::write(dir.path().join("w.sh"), "sleep 1\n").expect("write");
    std::fs::write(dir.path().join("w.py"), "import time\n").expect("write");
    let cwd = Some(dir.path());
    let shell = ScriptKind::Shell(ShellDialect::Posix);
    assert_eq!(
        script_file(&words("bash w.sh"), cwd).map(|f| f.0),
        Some(shell)
    );
    assert_eq!(
        script_file(&words("./w.sh 5"), cwd).map(|f| f.0),
        Some(shell)
    );
    assert_eq!(
        script_file(&words("python w.py"), cwd).map(|f| f.0),
        Some(ScriptKind::Python)
    );
    assert!(script_file(&words("bash missing.sh"), cwd).is_none());
    assert!(script_file(&words("bash $S/w.sh"), cwd).is_none());
    assert!(script_file(&words("bash w.sh"), None).is_none());
}

#[test]
fn shimmed_canonical_watch_is_still_allowed_outside_a_loop() {
    let mut reason = Some(String::new());
    temp_env(crate::shim_registry::GH_ACTIVE_KEY, "1", || {
        reason = evaluate_command("timeout 1500 gh pr checks 5 --watch", None, false, &[]).reason;
    });
    assert!(reason.is_none(), "{reason:?}");
    let mut reason = None;
    temp_env(crate::shim_registry::GH_ACTIVE_KEY, "1", || {
        reason = evaluate_command(
            "for p in 1 2; do gh pr checks $p --watch; done",
            None,
            false,
            &[],
        )
        .reason;
    });
    assert!(reason.is_some_and(|r| r.contains("retried in a loop")));
}

#[test]
fn codex_watch_payload_keeps_command_and_recovery_capabilities() {
    for tool in ["exec_command", "functions.exec_command"] {
        let payload = serde_json::json!({
            "tool_name": tool,
            "tool_input": {"cmd": "gh run watch 123 --exit-status"}
        });
        let view = parse_payload_value(&payload, Path::new(".")).expect("payload");
        assert_eq!(view.command, "gh run watch 123 --exit-status");
        assert!(block_bad_cmd_gate::gates_tool(tool));
    }
    let reason = evaluate_command("while gh pr checks 123; do sleep 5; done", None, false, &[])
        .reason
        .expect("native watch refused");
    assert!(reason.contains("write_stdin"), "{reason}");
    assert!(reason.contains("run_in_background"), "{reason}");
}

#[test]
fn execution_payload_uses_declared_fields_without_guessing_the_harness() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    for tool in ["Bash", "functions.exec_command"] {
        let raw = serde_json::json!({
            "tool_name": tool,
            "tool_input": {"command": "echo ignored", "cmd": "gh run watch 123",
                "workdir": "nested", "shell": "pwsh"}
        });
        let view = parse_payload_value(&raw, dir.path()).expect("payload");
        if tool == "Bash" {
            assert_eq!(view.command, "echo ignored");
            assert_eq!(view.cwd, dir.path());
            assert_eq!(shell_dialect_for_payload(&view), ShellDialect::Posix);
        } else {
            assert_eq!(view.command, "gh run watch 123");
            assert_eq!(view.cwd, dir.path().join("nested"));
            assert_eq!(shell_dialect_for_payload(&view), ShellDialect::PowerShell);
        }
    }
}

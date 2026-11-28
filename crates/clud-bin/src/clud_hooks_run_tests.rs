//! Tier-B execution: rooting, the exit-code contract, and the stdin payload.
//!
//! These spawn real child processes, because the properties under test —
//! inherited env, cwd, a closed stdin pipe delivering EOF — only exist in a
//! real process.

use super::*;
use crate::clud_hooks::HookEntry;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs;
use std::path::Path;

/// Write `body` as a Python script under `dir` and return the hook command
/// that runs it.
///
/// A real file rather than `python -c "..."`: the inline form has to survive
/// two levels of quoting (Rust, then cmd or sh) plus Windows backslashes,
/// which turns a quoting slip into a mystery `SyntaxError` rather than a
/// clear assertion failure.
fn python_hook(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    fs::write(&path, body).expect("write hook script");
    format!("{} \"{}\"", python_exe(), slashed(&path))
}

/// Python rather than a `.cmd`/`.sh` pair: divergent scripts would make a
/// platform-specific failure look like a logic bug. Every CI exec lane
/// installs Python as `python`, the one name clud uses everywhere (see
/// `ci/banned_python3.py`).
fn python_exe() -> String {
    if let Ok(path) = which::which("python") {
        return slashed(&path);
    }
    panic!("no `python` on PATH; the hook-execution tests need an interpreter");
}

/// Forward slashes throughout: Python accepts them on every platform, and
/// they keep Windows separators from being read as escapes by either the
/// shell or the Python literal they end up inside.
fn slashed(path: &Path) -> String {
    crate::path_norm::display_slash(path)
}

fn same_dir(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn entry(command: String, timeout_secs: u64) -> HookEntry {
    HookEntry {
        matcher: None,
        command,
        timeout_secs,
    }
}

fn run(entries: &[HookEntry], root: &Path, payload: &str) -> HookRunOutcome {
    let refs: Vec<&HookEntry> = entries.iter().collect();
    run_hooks(&refs, root, payload)
}

#[test]
fn a_hook_runs_rooted_at_the_declaring_repo_whatever_the_session_cwd_is() {
    // The contract that makes the whole dispatcher worth building: the hook
    // sees the repo root, not wherever the agent wandered off to.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let out = root.join("observed.txt");
    let body = format!(
        "import os\nopen(\"{}\", \"w\").write(os.getcwd() + \"|\" + os.environ.get(\"CLUD_PROJECT_DIR\", \"\"))\n",
        slashed(&out)
    );

    let outcome = run(
        &[entry(python_hook(root, "observe.py", &body), 60)],
        root,
        "{}",
    );
    assert_eq!(outcome.deny_reason, None, "{outcome:?}");

    let observed = fs::read_to_string(&out).expect("hook wrote its observation");
    let (cwd, project_dir) = observed.split_once('|').expect("both fields");
    assert!(same_dir(Path::new(cwd), root), "cwd was {cwd}");
    assert!(
        same_dir(Path::new(project_dir), root),
        "CLUD_PROJECT_DIR was {project_dir}"
    );
}

#[test]
fn the_hook_payload_arrives_on_stdin_and_the_pipe_closes() {
    // A hook blocking in `json.load(sys.stdin)` needs EOF, not just bytes —
    // the upstream Windows hook-stdin bug is exactly this failure.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let out = root.join("payload.txt");
    let body = format!(
        "import sys, json\npayload = json.load(sys.stdin)\nopen(\"{}\", \"w\").write(payload[\"tool_name\"])\n",
        slashed(&out)
    );

    let outcome = run(
        &[entry(python_hook(root, "payload.py", &body), 60)],
        root,
        r#"{"tool_name":"Bash","cwd":"/somewhere/else"}"#,
    );

    assert_eq!(outcome.deny_reason, None, "{outcome:?}");
    assert_eq!(fs::read_to_string(&out).unwrap(), "Bash");
}

#[test]
fn the_inherited_environment_survives_so_path_still_works() {
    // `ProcessConfig.env` replaces rather than overlays; forgetting to seed
    // it from the real environment would leave hooks with no PATH.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let out = root.join("env.txt");
    let body = format!(
        "import os\nopen(\"{}\", \"w\").write(str(len(os.environ.get(\"PATH\", \"\"))))\n",
        slashed(&out)
    );

    let outcome = run(&[entry(python_hook(root, "env.py", &body), 60)], root, "{}");
    assert_eq!(outcome.deny_reason, None, "{outcome:?}");

    let length: usize = fs::read_to_string(&out).unwrap().trim().parse().unwrap();
    assert!(length > 0, "PATH reached the hook");
}

// -----------------------------------------------------------------
// The exit-code contract.
// -----------------------------------------------------------------

#[test]
fn exit_two_blocks_and_relays_the_hooks_own_words() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let body = "import sys\nprint(\"run bash lint first\", file=sys.stderr)\nsys.exit(2)\n";

    let outcome = run(
        &[entry(python_hook(root, "block.py", body), 60)],
        root,
        "{}",
    );

    assert_eq!(
        outcome.deny_reason.as_deref(),
        Some("run bash lint first"),
        "the hook's message reaches the caller verbatim"
    );
}

#[test]
fn a_blocking_hook_without_a_message_still_says_something_actionable() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let body = "import sys\nsys.exit(2)\n";

    let outcome = run(
        &[entry(python_hook(root, "silent.py", body), 60)],
        root,
        "{}",
    );

    let reason = outcome.deny_reason.expect("blocked");
    assert!(reason.contains(".clud/hooks.json"), "{reason}");
    assert!(reason.contains("without explaining why"), "{reason}");
}

#[test]
fn a_blocking_hooks_stdout_is_kept_for_verbatim_relay() {
    // A hook may speak the harness's JSON protocol; re-wrapping it would
    // corrupt the decision it is trying to express.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let body = "import json, sys\nprint(json.dumps({\"decision\": \"block\"}))\nsys.exit(2)\n";

    let outcome = run(
        &[entry(python_hook(root, "json_block.py", body), 60)],
        root,
        "{}",
    );

    assert!(outcome.deny_reason.is_some());
    assert!(
        outcome.deny_stdout.unwrap_or_default().contains("decision"),
        "stdout preserved"
    );
}

#[test]
fn a_nonzero_exit_that_is_not_two_does_not_block() {
    // Only exit 2 blocks, same as the harness. A hook that is merely broken
    // must not become a wall in front of every tool call.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let body = "import sys\nsys.exit(1)\n";

    let outcome = run(
        &[entry(python_hook(root, "broken.py", body), 60)],
        root,
        "{}",
    );

    assert_eq!(outcome.deny_reason, None);
}

#[test]
fn a_non_blocking_failure_becomes_a_notice_quoting_the_root_cause() {
    // #1739: uv prints a generic headline first and the cause last; the
    // harness shows only the first line, and never to the agent.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let body = "import sys\n\
        print('x No solution found when resolving dependencies', file=sys.stderr)\n\
        print('  middle detail', file=sys.stderr)\n\
        print('there is no version of fbuild==2.5.31', file=sys.stderr)\n\
        sys.exit(1)\n";
    let command = python_hook(root, "uv_like.py", body);

    let outcome = run(&[entry(command.clone(), 60)], root, "{}");

    assert_eq!(outcome.deny_reason, None);
    let [notice] = outcome.notices.as_slice() else {
        panic!("one notice expected: {:?}", outcome.notices);
    };
    assert!(notice.contains(&format!("{command:?}")), "{notice}");
    assert!(notice.contains("exited 1"), "{notice}");
    assert!(notice.contains("No solution found"), "{notice}");
    assert!(
        notice.contains("there is no version of fbuild==2.5.31"),
        "{notice}"
    );
    assert!(!notice.contains("middle detail"), "{notice}");
}

#[test]
fn passing_and_blocking_hooks_leave_no_notice() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let ok = python_hook(root, "ok.py", "import sys\nsys.exit(0)\n");
    let stop = python_hook(root, "stop.py", "import sys\nsys.exit(2)\n");

    let outcome = run(&[entry(ok, 60), entry(stop, 60)], root, "{}");

    assert!(outcome.deny_reason.is_some());
    assert!(outcome.notices.is_empty(), "{:?}", outcome.notices);
}

#[test]
fn a_hook_that_cannot_start_is_survivable() {
    let tmp = TemporaryDirectory::new().unwrap();

    let outcome = run(
        &[entry("definitely-not-a-real-command-xyzzy".to_string(), 30)],
        tmp.path(),
        "{}",
    );

    assert_eq!(outcome.deny_reason, None, "fails open");
}

#[test]
fn hooks_run_in_declaration_order_and_stop_at_the_first_block() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let marker = root.join("third.txt");

    let first = python_hook(root, "ok.py", "import sys\nsys.exit(0)\n");
    let second = python_hook(
        root,
        "stop.py",
        "import sys\nprint(\"stop here\", file=sys.stderr)\nsys.exit(2)\n",
    );
    let third_body = format!("open(\"{}\", \"w\").write(\"ran\")\n", slashed(&marker));
    let third = python_hook(root, "third.py", &third_body);

    let outcome = run(
        &[entry(first, 60), entry(second, 60), entry(third, 60)],
        root,
        "{}",
    );

    assert_eq!(outcome.deny_reason.as_deref(), Some("stop here"));
    assert!(
        !marker.exists(),
        "the hook after the blocking one must not run"
    );
}

#[test]
fn a_hook_that_overruns_its_timeout_fails_open() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let body = "import time\ntime.sleep(30)\n";

    let started = std::time::Instant::now();
    let outcome = run(&[entry(python_hook(root, "slow.py", body), 2)], root, "{}");
    let elapsed = started.elapsed();

    assert_eq!(outcome.deny_reason, None, "a slow guard is not a wall");
    // #1908: the timeout is one budget, not one for draining plus one for waiting.
    assert!(
        elapsed < std::time::Duration::from_millis(3500),
        "{elapsed:?}"
    );
    assert!(
        outcome
            .log_messages
            .iter()
            .any(|message| message.contains("clud_hook_timeout")),
        "{:?}",
        outcome.log_messages
    );
    assert!(
        outcome
            .notices
            .iter()
            .any(|notice| notice.contains("timed out after 2s")),
        "{:?}",
        outcome.notices
    );
}

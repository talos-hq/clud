//! Unit tests for the `loop` contract surface of `command::builder`.
//!
//! Split out of `tests.rs` by the LOC guard; the tests are unchanged apart
//! from `use super::*` for the shared helpers.

use super::*;

#[test]
fn test_loop_command() {
    let p = plan(&["clud", "loop", "--loop-count", "5", "do stuff"]);
    assert_eq!(p.iterations, 5);
    assert!(p.command.contains(&"-p".to_string()));
    let prompt = prompt_from_plan(&p);
    assert!(prompt.starts_with("do stuff"));
    // Issue #95: contract now embeds absolute paths; the relative
    // suffix is still present, but the separator is platform-native.
    assert!(
        prompt.contains(".clud/loop/DONE") || prompt.contains(".clud\\loop\\DONE"),
        "prompt missing DONE marker path: {prompt}"
    );
    assert!(
        prompt.contains(".clud/loop/BLOCKED") || prompt.contains(".clud\\loop\\BLOCKED"),
        "prompt missing BLOCKED marker path: {prompt}"
    );
    assert!(p.loop_markers.is_some());
}

#[test]
fn test_loop_default_count() {
    let p = plan(&["clud", "loop", "task"]);
    assert_eq!(p.iterations, 50);
}

#[test]
fn test_loop_no_done_omits_contract() {
    let p = plan(&["clud", "loop", "--no-done", "task"]);
    let prompt = prompt_from_plan(&p);
    assert_eq!(prompt, "task");
    assert!(p.loop_markers.is_none());
}

#[test]
fn test_loop_repeat_implies_no_done_contract() {
    let p = plan(&["clud", "loop", "--repeat", "1h", "task"]);
    let prompt = prompt_from_plan(&p);
    assert_eq!(prompt, "task");
    assert!(p.loop_markers.is_none());
    assert_eq!(
        p.repeat_schedule.as_ref().map(|s| s.interval_secs),
        Some(3600)
    );
}

#[test]
fn test_loop_repeat_with_done_override_restores_contract() {
    let p = plan(&[
        "clud", "loop", "--repeat", "1h", "--done", "DONE.md", "task",
    ]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("DONE.md"));
    assert!(prompt.contains("BLOCKED.md"));
    assert!(p.loop_markers.is_some());
    let markers = p.loop_markers.unwrap();
    assert!(markers.done_path.ends_with("DONE.md"));
    assert!(markers.blocked_path.ends_with("BLOCKED.md"));
}

// ---- Issue #48: `clud --codex loop "..."` must drive codex the same ----
// way `clud loop` drives claude: exec subcommand, positional prompt,
// DONE/BLOCKED contract appended, loop_markers populated, and the
// non-interactive launch mode (subprocess) selected.

#[test]
fn test_codex_loop_routes_through_exec() {
    let p = plan(&["clud", "--codex", "loop", "--loop-count", "5", "do stuff"]);
    assert_eq!(p.command[0], "codex");
    assert!(codex_exec_index(&p) > 0);
    assert!(p
        .command
        .contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    assert_eq!(p.iterations, 5);
    assert_eq!(p.backend, Backend::Codex);
}

#[test]
fn test_codex_loop_prompt_is_positional_not_dash_p() {
    // Codex's `-p` is `--profile`; the prompt must be the final positional.
    let p = plan(&["clud", "--codex", "loop", "do stuff"]);
    assert!(
        p.command.iter().all(|a| a != "-p"),
        "codex must not emit -p for the prompt; cmd={:?}",
        p.command
    );
    let last = last_arg(&p);
    assert!(
        last.starts_with("do stuff"),
        "codex prompt must be the last positional arg; got: {last:?}"
    );
}

#[test]
fn test_codex_loop_appends_done_marker_contract() {
    let p = plan(&["clud", "--codex", "loop", "do stuff"]);
    let prompt = last_arg(&p);
    // Issue #95: absolute paths in contract; assert on the relative
    // suffix using platform-native separators.
    assert!(
        prompt.contains(".clud/loop/DONE") || prompt.contains(".clud\\loop\\DONE"),
        "prompt missing DONE marker path: {prompt}"
    );
    assert!(
        prompt.contains(".clud/loop/BLOCKED") || prompt.contains(".clud\\loop\\BLOCKED"),
        "prompt missing BLOCKED marker path: {prompt}"
    );
    assert!(p.loop_markers.is_some());
}

#[test]
fn test_codex_loop_default_count() {
    let p = plan(&["clud", "--codex", "loop", "task"]);
    assert_eq!(p.iterations, 50);
}

#[test]
fn test_codex_loop_no_done_omits_contract() {
    let p = plan(&["clud", "--codex", "loop", "--no-done", "task"]);
    let prompt = last_arg(&p);
    assert!(prompt.starts_with("task"));
    assert!(!prompt.contains(".clud/loop/DONE"));
    assert!(p.loop_markers.is_none());
}

/// The bounded-diagnostics routing used to ride on the loop prompt and only
/// when the backend was Codex, so a Claude agent reading a busy issue still
/// reached for `gh issue view --json body,comments`. It is now part of the
/// instruction surface every agent-mode launch gets (#2413).
fn codex_instructions(p: &LaunchPlan) -> String {
    p.command
        .iter()
        .filter(|arg| arg.starts_with("developer_instructions="))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

fn claude_system_prompt(p: &LaunchPlan) -> String {
    p.command
        .iter()
        .position(|arg| arg == "--append-system-prompt")
        .map(|at| p.command[at + 1].clone())
        .unwrap_or_default()
}

fn assert_routes_bounded_diagnostics(text: &str, what: &str) {
    assert!(
        text.contains("github/issue_context.py"),
        "{what} missing issue_context routing: {text}"
    );
    assert!(
        text.contains("github/pr_context.py"),
        "{what} missing pr_context routing: {text}"
    );
    assert!(
        text.contains("diagnostics/test_report.py"),
        "{what} missing test_report routing: {text}"
    );
    assert!(
        text.contains("instead of `gh pr view --json reviews,comments`")
            && text.contains("gh issue view --json body,comments"),
        "{what} lost the negative guidance: {text}"
    );
}

#[test]
fn test_codex_loop_routes_bounded_diagnostics() {
    let p = plan(&["clud", "--codex", "loop", "task"]);
    assert_routes_bounded_diagnostics(&codex_instructions(&p), "codex loop instructions");
}

#[test]
fn test_codex_one_shot_routes_bounded_diagnostics() {
    let p = plan(&["clud", "--codex", "-p", "task"]);
    assert_routes_bounded_diagnostics(&codex_instructions(&p), "codex -p instructions");
}

#[test]
fn test_claude_loop_routes_bounded_diagnostics() {
    let p = plan(&["clud", "loop", "task"]);
    assert_routes_bounded_diagnostics(&claude_system_prompt(&p), "claude loop prompt");
}

#[test]
fn test_claude_one_shot_routes_bounded_diagnostics() {
    let p = plan(&["clud", "-p", "task"]);
    assert_routes_bounded_diagnostics(&claude_system_prompt(&p), "claude -p prompt");
}

fn assert_routes_bounded_reads(text: &str, what: &str) {
    assert!(
        text.contains("code/outline.py"),
        "{what} missing outline routing: {text}"
    );
    assert!(
        text.contains("code/ast_grep.py"),
        "{what} missing ast_grep routing: {text}"
    );
    assert!(
        text.contains("sed -n") && text.contains("`cat` a large file whole"),
        "{what} lost the negative read guidance: {text}"
    );
}

#[test]
fn test_claude_loop_routes_bounded_reads() {
    let p = plan(&["clud", "loop", "task"]);
    assert_routes_bounded_reads(&claude_system_prompt(&p), "claude loop prompt");
}

#[test]
fn test_claude_one_shot_routes_bounded_reads() {
    let p = plan(&["clud", "-p", "task"]);
    assert_routes_bounded_reads(&claude_system_prompt(&p), "claude -p prompt");
}

#[test]
fn test_codex_loop_routes_bounded_reads() {
    let p = plan(&["clud", "--codex", "loop", "task"]);
    assert_routes_bounded_reads(&codex_instructions(&p), "codex loop instructions");
}

#[test]
fn test_codex_one_shot_routes_bounded_reads() {
    let p = plan(&["clud", "--codex", "-p", "task"]);
    assert_routes_bounded_reads(&codex_instructions(&p), "codex -p instructions");
}

#[test]
fn test_interactive_session_keeps_the_short_prompt() {
    // A human in a terminal decides for themselves; only agent launches are
    // steered toward the bounded readers.
    let p = plan(&["clud"]);
    let appended = claude_system_prompt(&p);
    assert!(!appended.contains("github/issue_context.py"));
    assert!(!appended.contains("diagnostics/test_report.py"));
    assert!(!appended.contains("code/outline.py"));
    let codex = codex_instructions(&plan(&["clud", "--codex"]));
    assert!(!codex.contains("code/outline.py"));
    assert!(!codex.contains("diagnostics/test_report.py"));
}

#[test]
fn test_codex_loop_uses_subprocess_launch_mode() {
    // `codex exec` is non-interactive → subprocess (pipe-friendly),
    // just like `clud --codex -p "..."`.
    let p = plan(&["clud", "--codex", "loop", "task"]);
    assert_eq!(p.launch_mode, LaunchMode::Subprocess);
}

#[test]
fn test_codex_loop_safe_mode_omits_bypass_flag() {
    let p = plan(&["clud", "--codex", "--safe", "loop", "task"]);
    assert!(!p
        .command
        .contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    assert_eq!(p.command[0], "codex");
    assert!(codex_exec_index(&p) > 0);
}

#[test]
fn test_codex_loop_forwards_passthrough_flags() {
    // `clud --codex loop "task" -- --verbose` must keep the passthrough
    // flag so the test harness can inject mock-agent flags the same way
    // it does for the claude path.
    let p = plan(&["clud", "--codex", "loop", "task", "--", "--verbose"]);
    assert!(p.command.contains(&"--verbose".to_string()));
}

// ---- Stream-JSON progress injection ----
//
// `clud loop` against claude in *subprocess* launch mode (Windows default,
// or anywhere `--subprocess` is forced) used to go silent for the whole
// iteration because `claude -p` buffers its final response. The fix is to
// append `--output-format stream-json --verbose` so claude streams its
// turn events live, and let the runtime render each event as a one-line
// progress update. PTY-mode loops already show the live TUI, so no
// injection is needed there.

/// Helper: locate the index of `needle` in `cmd`, panicking with a
/// readable message if missing.
fn expect_arg(cmd: &[String], needle: &str) -> usize {
    cmd.iter().position(|a| a == needle).unwrap_or_else(|| {
        panic!("expected `{needle}` in command; got {cmd:?}");
    })
}

#[test]
fn test_claude_loop_subprocess_injects_stream_json() {
    let p = plan(&["clud", "--subprocess", "loop", "task"]);
    assert_eq!(p.launch_mode, LaunchMode::Subprocess);
    let idx = expect_arg(&p.command, "stream-json");
    assert_eq!(
        p.command[idx - 1],
        "--output-format",
        "stream-json must follow --output-format; cmd={:?}",
        p.command
    );
    assert!(
        p.command.iter().any(|a| a == "--verbose"),
        "stream-json requires --verbose per claude's CLI contract; cmd={:?}",
        p.command
    );
    assert!(
        p.stream_json_progress,
        "LaunchPlan must signal the runtime to parse stream-json"
    );
}

#[test]
fn grind_never_enables_external_loop_stream_json() {
    let p = plan(&[
        "clud",
        "--subprocess",
        "grind",
        "https://github.com/zackees/clud/issues",
    ]);
    assert_eq!(p.launch_mode, console_launch_mode());
    assert!(!p.command.iter().any(|arg| arg == "stream-json"));
    assert!(!p.command.iter().any(|arg| arg == "--verbose"));
    assert!(!p.stream_json_progress);
    assert!(p
        .command
        .last()
        .is_some_and(|arg| arg.starts_with("/grind")));
}

#[test]
fn test_claude_loop_stream_json_flags_emitted_before_prompt() {
    // Regression guard for PR #91 / commit 8c0818a: the stream-json flags
    // must be inserted BEFORE `-p <prompt>` so that `command[-1]` is the
    // prompt body. Dry-run consumers, the Python integration tests in
    // tests/test_hello.py, and downstream tooling all rely on the
    // "prompt is the last arg" contract.
    let p = plan(&["clud", "--subprocess", "loop", "do stuff"]);
    assert!(p.stream_json_progress);

    // Prompt body must still be the last positional.
    let last = p.command.last().expect("cmd is non-empty");
    assert!(
        last.starts_with("do stuff"),
        "command[-1] must be the prompt body, got: {last:?} (full cmd: {:?})",
        p.command
    );

    // Each stream-json flag must appear strictly before `-p`.
    let p_idx = expect_arg(&p.command, "-p");
    for flag in ["--output-format", "stream-json", "--verbose"] {
        let flag_idx = expect_arg(&p.command, flag);
        assert!(
            flag_idx < p_idx,
            "{flag} (idx {flag_idx}) must come before -p (idx {p_idx}); cmd={:?}",
            p.command
        );
    }
}

#[test]
fn test_claude_loop_pty_does_not_inject_stream_json() {
    // PTY mode runs the live claude TUI; switching it into the
    // non-interactive stream-json wire format would *remove* the
    // streaming UX we already have.
    let p = plan(&["clud", "--pty", "loop", "task"]);
    assert_eq!(p.launch_mode, LaunchMode::Pty);
    assert!(
        !p.command.iter().any(|a| a == "stream-json"),
        "pty-mode loop must NOT inject stream-json; cmd={:?}",
        p.command
    );
    assert!(
        !p.stream_json_progress,
        "pty mode does not need the stream-json renderer"
    );
}

#[test]
fn test_codex_loop_does_not_inject_stream_json() {
    // codex does not accept `--output-format stream-json` — the flag is
    // claude-only. Forcing subprocess to be explicit so the test is
    // platform-independent.
    let p = plan(&["clud", "--codex", "--subprocess", "loop", "task"]);
    assert!(
        !p.command.iter().any(|a| a == "stream-json"),
        "codex must NOT receive --output-format stream-json; cmd={:?}",
        p.command
    );
    assert!(!p.stream_json_progress);
}

#[test]
fn test_claude_plain_prompt_does_not_inject_stream_json() {
    // Single-shot `clud -p` is short-lived and not a loop, so we keep
    // the existing UX untouched. Stream-json injection is loop-only.
    let p = plan(&["clud", "--subprocess", "-p", "hello"]);
    assert!(
        !p.command.iter().any(|a| a == "stream-json"),
        "plain -p must NOT receive stream-json injection; cmd={:?}",
        p.command
    );
    assert!(!p.stream_json_progress);
}

#[test]
fn test_claude_do_launches_interactively_without_print_flag() {
    // Regression for the `clud do` "nothing happened / halts" report: `do`
    // runs the `/goal` contract, which drives a long interactive Stop-hook
    // loop. Headless `-p` mode buffers its single final response and shows
    // nothing until the whole goal completes, so it must seed an *interactive*
    // session (bare positional prompt, no `-p`) instead.
    let p = plan(&["clud", "do", "https://github.com/o/r/issues/1"]);
    assert!(
        !p.command.iter().any(|a| a == "-p"),
        "clud do must NOT run headless `-p`; cmd={:?}",
        p.command
    );
    assert!(
        !p.stream_json_progress,
        "interactive do renders the live TUI; it must not use the stream-json renderer"
    );
    // The `/goal` prompt body is still the trailing positional and is intact.
    let last = p.command.last().expect("cmd is non-empty");
    assert!(
        last.starts_with("/goal ") && last.contains("https://github.com/o/r/issues/1"),
        "command[-1] must be the /goal prompt seed; got {last:?} (cmd={:?})",
        p.command
    );
}

#[test]
fn test_claude_loop_safe_mode_still_injects_stream_json() {
    // `--safe` only drops the YOLO permissions flag; it must not also
    // suppress progress streaming, which is orthogonal.
    let p = plan(&["clud", "--subprocess", "--safe", "loop", "task"]);
    assert!(p.command.iter().any(|a| a == "stream-json"));
    assert!(p.stream_json_progress);
    // Sanity: --safe removed the permissions bypass.
    assert!(!p
        .command
        .iter()
        .any(|a| a == "--dangerously-skip-permissions"));
}

#[test]
fn test_pty_override() {
    let p = plan(&["clud", "--pty", "-p", "hello"]);
    assert_eq!(p.launch_mode, LaunchMode::Pty);
}

#[test]
fn test_graphics_config_threads_into_launch_plan() {
    let p = plan(&[
        "clud",
        "--graphics=sixel",
        "--graphics-image",
        "banner.png",
        "--pty",
        "-p",
        "hello",
    ]);
    assert_eq!(p.graphics.mode, crate::graphics::GraphicsMode::Sixel);
    assert_eq!(
        p.graphics.image_path.as_ref().map(|path| path.as_os_str()),
        Some(std::ffi::OsStr::new("banner.png"))
    );
    assert!(!p.command.iter().any(|arg| arg.starts_with("--graphics")));
}

#[test]
fn test_passthrough_flags() {
    let p = plan(&["clud", "--some-flag", "-p", "hello"]);
    assert!(p.command.contains(&"--some-flag".to_string()));
}

#[test]
fn test_passthrough_after_separator() {
    let p = plan(&["clud", "-p", "hello", "--", "--verbose"]);
    assert!(p.command.contains(&"--verbose".to_string()));
}

#[test]
fn test_is_github_url() {
    assert!(is_github_url("https://github.com/user/repo"));
    assert!(is_github_url("http://github.com/user/repo"));
    assert!(!is_github_url("https://gitlab.com/user/repo"));
    assert!(!is_github_url("not a url"));
}

#[test]
fn test_build_fix_prompt_no_url() {
    let prompt = build_fix_prompt(None);
    assert_eq!(prompt, FIX_PROMPT);
}

#[test]
fn test_build_fix_prompt_github_url() {
    let prompt = build_fix_prompt(Some("https://github.com/user/repo/actions/runs/999"));
    assert!(prompt.contains("runs/999"));
    assert!(prompt.contains("gh run view"));
}

#[test]
fn test_build_up_prompt_default() {
    let prompt = build_up_prompt(None, false);
    assert!(prompt.contains("<your one-line summary>"));
    assert!(!prompt.contains(" -p"));
}

#[test]
fn test_build_up_prompt_custom_message() {
    let prompt = build_up_prompt(Some("my msg"), false);
    assert!(prompt.contains("codeup -m \"my msg\""));
    assert!(!prompt.contains("<your one-line summary>"));
}

#[test]
fn test_build_up_prompt_publish() {
    let prompt = build_up_prompt(None, true);
    assert!(prompt.contains("-p"));
}

//! Unit tests for the builtin-command prompt builders: `up`, `rebase`,
//! `fix`, and `do`, plus the `/grind` seed prompt.
//!
//! Split out of `tests.rs` by the LOC guard; the tests are unchanged apart
//! from `use super::*` for the shared helpers.

use super::*;

#[test]
fn test_model_flag() {
    let p = plan(&["clud", "--model", "opus", "-p", "hello"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        vec![
            "claude",
            "--dangerously-skip-permissions",
            "--model",
            "opus",
            "-p",
            "hello"
        ]
    );
}

#[test]
fn test_continue_session() {
    let p = plan(&["clud", "-c"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        vec!["claude", "--dangerously-skip-permissions", "--continue"]
    );
}

#[test]
fn test_message_flag() {
    let p = plan(&["clud", "-m", "fix bug"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        vec!["claude", "--dangerously-skip-permissions", "-m", "fix bug"]
    );
}

#[test]
fn test_up_default() {
    let p = plan(&["clud", "up"]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("lint"));
    assert!(prompt.contains("codeup"));
    assert!(prompt.contains("<your one-line summary>"));
    assert!(!prompt.contains("-p\n"));
}

#[test]
fn test_up_with_message() {
    let p = plan(&["clud", "up", "-m", "bump version"]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("codeup -m \"bump version\""));
    assert!(!prompt.contains("<your one-line summary>"));
}

#[test]
fn test_up_with_publish() {
    let p = plan(&["clud", "up", "--publish"]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("codeup -m \"<your one-line summary>\" -p"));
}

#[test]
fn test_up_with_message_and_publish() {
    let p = plan(&["clud", "up", "-m", "release v2", "--publish"]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("codeup -m \"release v2\" -p"));
}

#[test]
fn test_rebase_command() {
    let p = plan(&["clud", "rebase"]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("git fetch"));
    assert!(prompt.contains("rebase"));
}

#[test]
fn test_fix_default() {
    let p = plan(&["clud", "fix"]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("linting"));
    assert!(prompt.contains("unit tests"));
}

#[test]
fn test_fix_with_github_url() {
    let p = plan(&[
        "clud",
        "fix",
        "https://github.com/user/repo/actions/runs/123",
    ]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("https://github.com/user/repo/actions/runs/123"));
    assert!(prompt.contains("gh run view"));
    assert!(prompt.contains("lint-test"));
}

#[test]
fn test_fix_with_non_github_url() {
    let p = plan(&["clud", "fix", "https://example.com/logs"]);
    let prompt = prompt_from_plan(&p);
    assert!(prompt.contains("linting"));
    assert!(!prompt.contains("example.com"));
}

#[test]
fn test_do_command_resolves_goal_prompt() {
    let p = plan(&["clud", "do", "https://github.com/zackees/clud/issues/866"]);
    // `do` seeds an interactive session, so the prompt is the trailing
    // positional (no `-p`). Claude caps the /goal condition (#2372), so the
    // seed is short and the live contract rides in the appended system prompt.
    let seed = last_arg(&p);
    assert!(seed.starts_with("/goal Complete `clud do`"));
    assert!(seed.contains("https://github.com/zackees/clud/issues/866"));
    let system = appended_system_prompt(&p);
    assert!(system.contains("# /do: rendered"));
    assert!(system.contains("Verdict: `default`"));
    assert!(system.contains("Read the issue and implement it"));
}

#[test]
fn test_do_command_on_a_meta_issue_seeds_grind() {
    let mut args = parse(&["clud", "do", "https://github.com/zackees/clud/issues/900"]);
    args.do_meta = true;
    args.do_rendered_prompt = Some(crate::command::do_prompt::render(
        "https://github.com/zackees/clud/issues/900",
        &Ok(crate::command::do_kind::DoKind::Meta),
        &Ok(crate::command::do_prompt::BranchFacts {
            branch: Some("main".into()),
            default_branch: "main".into(),
            ..Default::default()
        }),
    ));
    let backend = crate::backend::resolve_backend(args.claude, args.codex);
    let p = build_launch_plan(&args, backend, backend.executable_name());
    assert!(last_arg(&p).starts_with("/goal Complete `clud do`"));
    let seed = appended_system_prompt(&p);
    assert!(seed.contains("# /do: rendered"));
    assert!(seed.contains("This is a meta issue"));
    assert!(seed.contains("Workflow tool"));
    assert!(seed.contains("use Read to read the complete grind router"));
    assert!(!seed.contains("Read the issue and implement it"));
}

#[test]
fn test_build_do_prompt_preserves_the_whole_rendered_contract() {
    let procedure = "Verdict: stale_merged\nrepair \"quotes\"; $(touch sentinel) &\nFinish";
    assert_eq!(
        build_do_prompt(Some(procedure)),
        format!("/goal {procedure}")
    );
}

fn appended_system_prompt(p: &LaunchPlan) -> &str {
    let at = p
        .command
        .iter()
        .position(|arg| arg == "--append-system-prompt")
        .expect("claude plan appends a system prompt");
    &p.command[at + 1]
}

#[test]
fn test_claude_do_goal_stays_under_the_goal_condition_cap() {
    // #2372: Claude Code rejects a longer condition without sending a request.
    let target = "x".repeat(10_000);
    let mut args = parse(&["clud", "do", &target]);
    args.do_rendered_prompt = Some(format!("# /do: rendered\n{}", "y".repeat(8_000)));
    let backend = crate::backend::resolve_backend(args.claude, args.codex);
    let p = build_launch_plan(&args, backend, backend.executable_name());
    let seed = last_arg(&p);
    assert!(seed.starts_with("/goal "));
    assert!(seed.chars().count() - "/goal ".len() < CLAUDE_GOAL_CONDITION_LIMIT);
    assert!(appended_system_prompt(&p).contains(&"y".repeat(8_000)));
    let unprepared = build_claude_do_goal(&target, false);
    assert!(unprepared.starts_with("/goal Stop:"));
}

#[test]
fn test_do_plan_without_a_prepared_contract_fails_closed() {
    for rendered in [None, Some(""), Some("  ")] {
        let seed = build_do_prompt(rendered);
        assert!(seed.starts_with("/goal Stop:"));
        assert!(seed.contains("do not start work"));
    }
}

#[test]
fn test_build_grind_prompt_substitutes_url() {
    let prompt = build_grind_prompt("https://example.com/repo/issues");
    assert_eq!(prompt, "/grind https://example.com/repo/issues");
    assert!(prompt.contains("https://example.com/repo/issues"));
    assert!(!prompt.contains("{url}"));
}

#[test]
fn test_claude_do_goal_keeps_constraints_from_a_long_target() {
    // #2522: the old seed cut the target at 1000 chars, silently dropping a
    // late "never push" or "do not touch" clause.
    let target = format!(
        "Fix crates/clud-bin/src/args.rs. {} Do not touch ~/dev/clud4. Never push to main.",
        "Background prose that explains the history at length. ".repeat(90)
    );
    let mut args = parse(&["clud", "do", &target]);
    args.do_rendered_prompt = Some("# /do: rendered".into());
    let backend = crate::backend::resolve_backend(args.claude, args.codex);
    let p = build_launch_plan(&args, backend, backend.executable_name());
    let seed = last_arg(&p);
    assert!(seed.chars().count() - "/goal ".len() <= CLAUDE_GOAL_CONDITION_LIMIT);
    for clause in [
        "crates/clud-bin/src/args.rs",
        "Do not touch ~/dev/clud4.",
        "Never push to main.",
    ] {
        assert!(seed.contains(clause), "{clause:?} lost from {seed}");
    }
}

#[test]
fn test_claude_do_goal_prefers_the_prepared_compacted_goal() {
    let mut args = parse(&["clud", "do", "anything"]);
    args.do_rendered_prompt = Some("# /do: rendered".into());
    args.do_goal = Some("Execute /x/brief.md exactly. Never push.".into());
    let backend = crate::backend::resolve_backend(args.claude, args.codex);
    let p = build_launch_plan(&args, backend, backend.executable_name());
    assert_eq!(
        last_arg(&p),
        "/goal Execute /x/brief.md exactly. Never push."
    );
}

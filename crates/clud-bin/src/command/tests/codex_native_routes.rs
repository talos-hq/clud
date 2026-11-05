//! Unit tests for the Codex native-route surface of `command::builder`.
//!
//! Split out of `tests.rs` by the LOC guard; the tests are unchanged apart
//! from `use super::*` for the shared helpers.

use super::*;

#[test]
fn test_codex_prompt_goes_through_exec_subcommand() {
    // Codex's `-p` is `--profile`, not a prompt flag. Non-interactive
    // runs must use `codex exec <prompt>` with the prompt as positional.
    let p = plan(&["clud", "--codex", "-p", "hello"]);
    assert!(!p
        .command
        .iter()
        .any(|arg| arg == "--dangerously-bypass-hook-trust"));
    assert_eq!(
        command_without_deletion_policy(&p),
        [
            codex_prefix(),
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "hello".to_string(),
            ],
        ]
        .concat()
    );
    // `codex exec` is non-interactive; subprocess mode is fine.
    assert_eq!(p.launch_mode, LaunchMode::Subprocess);
}

/// An interactive launch is PTY from a console and subprocess without one
#[test]
fn test_codex_interactive_follows_console_rule() {
    let p = plan(&["clud", "--codex"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        [
            codex_prefix(),
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()],
        ]
        .concat()
    );
    assert_eq!(p.launch_mode, console_launch_mode());
}

#[test]
fn test_codex_keeps_native_agents_when_agents_md_exists() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("AGENTS.md"), "native agents").unwrap();
    std::fs::write(repo.path().join("CODEX.md"), "codex fallback").unwrap();
    std::fs::write(repo.path().join("CLAUDE.md"), "claude fallback").unwrap();

    let p = plan_at(&["clud", "--codex"], repo.path());

    assert!(!codex_config_values(&p)
        .iter()
        .any(|value| value.starts_with("project_doc_fallback_filenames=")));
}

#[test]
fn test_codex_uses_codex_md_as_project_doc_fallback_before_claude_md() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("CODEX.md"), "codex fallback").unwrap();
    std::fs::write(repo.path().join("CLAUDE.md"), "claude fallback").unwrap();

    let p = plan_at(&["clud", "--codex"], repo.path());

    assert!(codex_config_values(&p).contains(&r#"project_doc_fallback_filenames=["CODEX.md"]"#));
    assert!(!codex_config_values(&p)
        .iter()
        .any(|value| value.contains("CLAUDE.md")));
}

#[test]
fn test_codex_uses_claude_md_when_agents_and_codex_are_absent() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("CLAUDE.md"), "claude fallback").unwrap();

    let p = plan_at(&["clud", "--codex"], repo.path());

    assert!(codex_config_values(&p).contains(&r#"project_doc_fallback_filenames=["CLAUDE.md"]"#));
}

#[test]
fn test_codex_project_doc_fallback_noops_when_no_instruction_file_exists() {
    let repo = tempfile::tempdir().unwrap();

    let p = plan_at(&["clud", "--codex"], repo.path());

    assert!(!codex_config_values(&p)
        .iter()
        .any(|value| value.starts_with("project_doc_fallback_filenames=")));
}

#[test]
fn test_codex_continue_uses_resume_last() {
    // `-c` on codex maps to `codex resume --last`, not `--continue`.
    let p = plan(&["clud", "--codex", "-c"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        [
            codex_prefix(),
            vec![
                "resume".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "--last".to_string(),
            ],
        ]
        .concat()
    );
    assert_eq!(p.launch_mode, console_launch_mode());
}

#[test]
fn test_codex_resume_with_session_id() {
    let p = plan(&["clud", "--codex", "-r", "sess-123"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        [
            codex_prefix(),
            vec![
                "resume".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "sess-123".to_string(),
            ],
        ]
        .concat()
    );
}

#[test]
fn test_codex_model_uses_short_m() {
    // Codex's model flag is `-m/--model`; Claude's is `--model`.
    let p = plan(&["clud", "--codex", "--model", "gpt-5"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        [
            codex_prefix(),
            vec![
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "-m".to_string(),
                "gpt-5".to_string(),
            ],
        ]
        .concat()
    );
}

#[test]
fn test_codex_builtin_verbs_seed_interactive_sessions() {
    for argv in [
        vec!["clud", "--codex", "up"],
        vec!["clud", "--codex", "rebase"],
        vec!["clud", "--codex", "fix"],
        vec![
            "clud",
            "--codex",
            "do",
            "https://github.com/zackees/clud/issues/1036",
        ],
        vec![
            "clud",
            "--codex",
            "grind",
            "https://github.com/zackees/clud/issues",
        ],
    ] {
        let p = plan(&argv);
        assert_eq!(p.command[0], "codex", "argv={argv:?}");
        assert!(
            !p.command.iter().any(|arg| arg == "exec"),
            "one-shot built-in must not use codex exec; argv={argv:?}, cmd={:?}",
            p.command
        );
        assert!(
            p.command.iter().all(|arg| arg != "-p"),
            "Codex prompt must stay positional; argv={argv:?}, cmd={:?}",
            p.command
        );
        assert_eq!(
            p.launch_mode,
            console_launch_mode(),
            "an interactive Codex TUI follows the console rule (DD-086); argv={argv:?}"
        );
    }
}

#[test]
fn codex_resumed_builtins_put_the_session_selector_before_the_prompt() {
    for mut argv in [
        vec!["clud", "--codex", "--resume=sess-123", "up"],
        vec!["clud", "--codex", "--resume=sess-123", "rebase"],
        vec!["clud", "--codex", "--resume=sess-123", "fix"],
        vec![
            "clud",
            "--codex",
            "--resume=sess-123",
            "do",
            "https://github.com/zackees/clud/issues/1036",
        ],
        vec![
            "clud",
            "--codex",
            "--resume=sess-123",
            "grind",
            "https://github.com/zackees/clud/issues",
        ],
    ] {
        let resumed_plan = plan(&argv);
        let resume = resumed_plan
            .command
            .iter()
            .position(|arg| arg == "resume")
            .unwrap();
        let session = resumed_plan
            .command
            .iter()
            .position(|arg| arg == "sess-123")
            .unwrap();
        assert!(resume < session && session + 1 == resumed_plan.command.len() - 1);
        assert!(!resumed_plan.command.iter().any(|arg| arg == "exec"));

        argv.retain(|arg| *arg != "--resume=sess-123");
        argv.insert(2, "--continue");
        let continued_plan = plan(&argv);
        let resume = continued_plan
            .command
            .iter()
            .position(|arg| arg == "resume")
            .unwrap();
        let last = continued_plan
            .command
            .iter()
            .position(|arg| arg == "--last")
            .unwrap();
        assert!(resume < last && last + 1 == continued_plan.command.len() - 1);
        assert!(!continued_plan.command.iter().any(|arg| arg == "exec"));
    }
}

#[test]
fn bare_resume_with_interactive_codex_builtin_is_rejected_and_plan_safe() {
    let args = parse(&[
        "clud",
        "--codex",
        "--resume",
        "do",
        "https://github.com/zackees/clud/issues/1036",
    ]);
    assert!(interactive_builtin_resume_error(&args, Backend::Codex)
        .unwrap()
        .contains("--resume=<session>"));
    assert!(interactive_builtin_resume_error(&args, Backend::Claude).is_none());

    let plan = build_launch_plan(&args, Backend::Codex, "codex");
    assert!(plan.command.iter().any(|arg| arg == "resume"));
    assert!(!plan.command.iter().any(|arg| arg.starts_with("/goal")));
}

/// #1173: `grind` is one interactive Claude-harness session. On the Codex
/// harness it is refused outright by `grind_launch_error` -- not by the
/// headless-builtin `--resume` check, which no longer applies to it -- and
/// the plan carries none of clud's own loop machinery.
#[test]
fn bare_resume_with_codex_grind_is_refused_by_the_harness_check() {
    let args = parse(&[
        "clud",
        "--codex",
        "--resume",
        "grind",
        "https://github.com/zackees/clud/issues",
    ]);
    assert!(interactive_builtin_resume_error(&args, Backend::Codex).is_none());
    let codex_harness = ResolvedLaunchTarget {
        routing_mode: RoutingMode::Direct,
        model_provider: ModelProvider::Codex,
        requested_harness: HarnessSelection::Codex,
        effective_harness: Backend::Codex,
        provider_source: PreferenceSource::Cli,
        harness_source: PreferenceSource::Cli,
    };
    let error = grind_launch_error(&args, codex_harness).expect("grind needs the Claude harness");
    assert!(error.contains("requires the Claude harness"), "{error}");
    let plan = build_launch_plan(&args, Backend::Codex, "codex");
    assert_eq!(plan.iterations, 1);
    assert!(plan.loop_markers.is_none());
    assert!(plan.repeat_schedule.is_none());
}

#[test]
fn unresolved_do_target_has_a_safe_interactive_plan_fallback() {
    for (argv, expected_backend_token) in [
        (vec!["clud", "--codex", "do"], "codex"),
        (vec!["clud", "do"], "claude"),
        (vec!["clud", "--harness", "deepseek", "do"], "dsh"),
    ] {
        let plan = if expected_backend_token == "dsh" {
            let args = parse(&argv);
            build_launch_plan_for_target(&args, deepseek_harness_target(), "dsh")
        } else {
            plan(&argv)
        };
        assert_eq!(plan.command[0], expected_backend_token, "argv={argv:?}");
        assert!(
            !plan.command.iter().any(|arg| arg == "exec" || arg == "headless"),
            "an unresolved target must fall back to the harness's interactive entry; argv={argv:?}, cmd={:?}",
            plan.command
        );
    }
}

const AUTO_REVIEW: &str = r#"approvals_reviewer="auto_review""#;

fn reviewer_values(p: &LaunchPlan) -> Vec<&str> {
    codex_config_values(p)
        .into_iter()
        .filter(|value| value.starts_with("approvals_reviewer"))
        .collect()
}

/// #1847: every Codex launch routes approvals to Codex auto review, and the
/// override precedes the subcommand so `exec` and `resume` inherit it.
#[test]
fn test_codex_enables_auto_review_on_interactive_exec_and_resume() {
    for raw in [
        &["clud", "--codex"][..],
        &["clud", "--codex", "-p", "hello"][..],
        &["clud", "--codex", "-c"][..],
        &["clud", "--codex", "--safe"][..],
    ] {
        let p = plan(raw);
        assert_eq!(reviewer_values(&p), [AUTO_REVIEW], "{raw:?}");
        let reviewer = p.command.iter().position(|arg| arg == AUTO_REVIEW).unwrap();
        if let Some(sub) = p
            .command
            .iter()
            .position(|arg| arg == "exec" || arg == "resume")
        {
            assert!(reviewer < sub, "{raw:?}");
        }
    }
}

#[test]
fn test_claude_harness_does_not_get_codex_auto_review() {
    let p = plan(&["clud", "--claude", "-p", "hello"]);
    assert!(!p
        .command
        .iter()
        .any(|arg| arg.contains("approvals_reviewer")));
}

#[test]
fn test_codex_auto_review_respects_settings_override() {
    let mut args = parse(&["clud", "--codex"]);
    args.codex_config_overrides = vec![r#"approvals_reviewer="user""#.to_string()];
    let p = build_launch_plan(&args, Backend::Codex, "codex");
    assert_eq!(reviewer_values(&p), [r#"approvals_reviewer="user""#]);
}

#[test]
fn test_codex_auto_review_respects_passthrough_choice() {
    for raw in [
        &[
            "clud",
            "--codex",
            "--",
            "-c",
            r#"approvals_reviewer="user""#,
        ][..],
        &[
            "clud",
            "--codex",
            "--",
            "--config",
            r#"approvals_reviewer="user""#,
        ][..],
        &[
            "clud",
            "--codex",
            "--",
            r#"--config=approvals_reviewer="user""#,
        ][..],
        &["clud", "--codex", "--", "--approve-for-me"][..],
    ] {
        let p = plan(raw);
        assert!(
            !p.command.iter().any(|arg| arg == AUTO_REVIEW),
            "{raw:?}: {:?}",
            p.command
        );
    }
}

use super::builder::{
    build_launch_plan, build_launch_plan_at, build_launch_plan_for_target, grind_launch_error,
    interactive_builtin_resume_error, next_run_at_millis, parse_repeat_interval,
    plan_mode_suppression_notice, repeat_implies_no_done_warning, video_launch_error,
};
use super::prompts::{
    build_claude_do_goal, build_do_prompt, build_fix_prompt, build_grind_prompt, build_up_prompt,
    is_github_url, CLAUDE_GOAL_CONDITION_LIMIT, FIX_PROMPT,
};
use super::types::LaunchPlan;
use crate::args::Args;
use crate::backend::{
    Backend, HarnessSelection, LaunchMode, ModelProvider, PreferenceSource, ResolvedLaunchTarget,
    RoutingMode,
};
use crate::clud_settings::DEFAULT_CODEX_GITHUB_PLUGIN_CONFIG_OVERRIDE;

fn parse(raw: &[&str]) -> Args {
    let raw: Vec<String> = raw.iter().map(|s| s.to_string()).collect();
    let mut args = Args::parse_from_raw(raw);
    // Production resolves live facts in main before building a plan. Keep
    // these argv tests pure instead of querying this checkout or GitHub.
    if let Some(crate::args::Command::Do {
        target: Some(target),
    }) = &args.command
    {
        args.do_rendered_prompt = Some(super::do_prompt::render(
            target,
            &Ok(super::do_kind::DoKind::Single),
            &Ok(super::do_prompt::BranchFacts {
                branch: Some("main".into()),
                default_branch: "main".into(),
                ..Default::default()
            }),
        ));
    }
    args
}

fn plan(raw: &[&str]) -> LaunchPlan {
    let mut args = parse(raw);
    let backend = crate::backend::resolve_backend(args.claude, args.codex);
    if matches!(backend, Backend::Codex) {
        args.codex_config_overrides = vec![DEFAULT_CODEX_GITHUB_PLUGIN_CONFIG_OVERRIDE.to_string()];
    }
    build_launch_plan(&args, backend, backend.executable_name())
}

fn plan_at(raw: &[&str], cwd: &std::path::Path) -> LaunchPlan {
    let mut args = parse(raw);
    let backend = crate::backend::resolve_backend(args.claude, args.codex);
    if matches!(backend, Backend::Codex) {
        args.codex_config_overrides = vec![DEFAULT_CODEX_GITHUB_PLUGIN_CONFIG_OVERRIDE.to_string()];
    }
    build_launch_plan_at(&args, backend, backend.executable_name(), cwd)
}

fn prompt_from_plan(p: &LaunchPlan) -> &str {
    let idx = p.command.iter().position(|a| a == "-p").unwrap();
    &p.command[idx + 1]
}

/// Find the last positional (non-flag, non-subcommand) argument of the plan.
/// For codex we emit the prompt positionally, so this picks it up.
fn last_arg(p: &LaunchPlan) -> &str {
    p.command.last().map(String::as_str).unwrap_or("")
}

fn codex_prefix() -> Vec<String> {
    vec![
        "codex".to_string(),
        "-c".to_string(),
        DEFAULT_CODEX_GITHUB_PLUGIN_CONFIG_OVERRIDE.to_string(),
        "-c".to_string(),
        r#"approvals_reviewer="auto_review""#.to_string(),
    ]
}

fn codex_exec_index(p: &LaunchPlan) -> usize {
    p.command.iter().position(|arg| arg == "exec").unwrap()
}

fn codex_config_values(p: &LaunchPlan) -> Vec<&str> {
    p.command
        .windows(2)
        .filter_map(|pair| (pair[0] == "-c").then_some(pair[1].as_str()))
        .collect()
}

/// Preserve the historical argv assertions while checking that the deletion
/// policy is present exactly once and precedes any positional prompt.
fn command_without_deletion_policy(p: &LaunchPlan) -> Vec<String> {
    let mut command = p.command.clone();
    if command[0] == "claude" {
        let index = command
            .iter()
            .position(|arg| arg == "--append-system-prompt")
            .unwrap();
        assert_eq!(index, 1);
        assert!(command[index + 1].contains(&crate::deletion_rules::generated().instructions));
        command.drain(index..index + 2);
    } else if command[0] == "codex" {
        for prefix in [
            "hooks.PreToolUse=",
            "hooks.state=",
            "developer_instructions=",
        ] {
            let indexes: Vec<_> = command
                .iter()
                .enumerate()
                .filter_map(|(i, arg)| arg.starts_with(prefix).then_some(i))
                .collect();
            assert_eq!(indexes.len(), 1, "missing or duplicate {prefix}");
            let index = indexes[0];
            assert_eq!(command[index - 1], "-c");
            if let Some(exec_index) = command
                .iter()
                .position(|arg| arg == "exec" || arg == "resume")
            {
                assert!(index > exec_index, "hook config must follow subcommand");
            }
            command.drain(index - 1..index + 1);
        }
    }
    command
}

#[test]
fn test_prompt_with_yolo() {
    let p = plan(&["clud", "-p", "hello"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        vec![
            "claude",
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "-p",
            "hello"
        ]
    );
    assert_eq!(p.iterations, 1);
    assert_eq!(p.launch_mode, LaunchMode::Subprocess);
}

#[test]
fn explicit_run_is_the_same_launch_as_bare_clud() {
    for (bare_argv, run_argv) in [
        (vec!["clud"], vec!["clud", "run"]),
        (
            vec!["clud", "--prompt", "hello"],
            vec!["clud", "--prompt", "hello", "run"],
        ),
        (
            vec!["clud", "--message", "hello"],
            vec!["clud", "--message", "hello", "run"],
        ),
        (
            vec!["clud", "--continue"],
            vec!["clud", "--continue", "run"],
        ),
        (
            vec!["clud", "--resume=session-id"],
            vec!["clud", "--resume=session-id", "run"],
        ),
    ] {
        let bare = plan(&bare_argv);
        let explicit = plan(&run_argv);
        assert_eq!(explicit.command, bare.command, "{run_argv:?}");
        assert_eq!(explicit.iterations, bare.iterations, "{run_argv:?}");
        assert_eq!(explicit.routing_mode, bare.routing_mode, "{run_argv:?}");
    }
}

#[test]
fn test_loop_automatically_disallows_interactive_tools() {
    let p = plan(&["clud", "loop", "fix the build"]);
    assert!(p
        .command
        .iter()
        .any(|a| a == "--disallowedTools=EnterPlanMode,AskUserQuestion"));
}

#[test]
fn test_repeat_loop_automatically_disallows_interactive_tools() {
    let p = plan(&["clud", "loop", "fix the build", "--repeat", "1m"]);
    assert!(p
        .command
        .iter()
        .any(|a| a == "--disallowedTools=EnterPlanMode,AskUserQuestion"));
    assert_eq!(
        p.repeat_schedule.as_ref().map(|s| s.interval_secs),
        Some(60)
    );
}

#[test]
fn test_loop_under_codex_provider_claude_harness_disallows_interactive_tools() {
    let args = parse(&["clud", "loop", "fix the build"]);
    let target = ResolvedLaunchTarget {
        routing_mode: RoutingMode::Direct,
        model_provider: ModelProvider::Codex,
        requested_harness: HarnessSelection::Claude,
        effective_harness: Backend::Claude,
        provider_source: PreferenceSource::Cli,
        harness_source: PreferenceSource::Cli,
    };
    let p = build_launch_plan_for_target(&args, target, "claude");
    assert!(p
        .command
        .iter()
        .any(|a| a == "--disallowedTools=EnterPlanMode,Task,AskUserQuestion"));
}

/// Issue #955 acceptance path: Sol stays selected when `loop` turns the
/// Claude harness launch into a subprocess/repeat plan.
#[test]
fn sol_through_claude_loop_keeps_the_discovery_model_and_wire_selection() {
    let args = parse(&[
        "clud",
        "--model",
        "codex-sol",
        "--effort",
        "low",
        "loop",
        "--loop-count",
        "1",
        "--no-done",
        "reply once",
    ]);
    let plan = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert_eq!(plan.iterations, 1);
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["--model", "clud-claude-codex-sol"]));
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["--effort", "low"]));
    assert_eq!(
        plan.model_selection
            .as_ref()
            .and_then(|selection| selection.wire_model.as_deref()),
        Some("gpt-6-sol")
    );
}

/// `clud --codex --harness claude`, interactive, no `--unattended`.
fn bridge_target() -> ResolvedLaunchTarget {
    ResolvedLaunchTarget {
        routing_mode: RoutingMode::Direct,
        model_provider: ModelProvider::Codex,
        requested_harness: HarnessSelection::Claude,
        effective_harness: Backend::Claude,
        provider_source: PreferenceSource::Cli,
        harness_source: PreferenceSource::Cli,
    }
}

fn deepseek_bridge_target() -> ResolvedLaunchTarget {
    ResolvedLaunchTarget {
        routing_mode: RoutingMode::Direct,
        model_provider: ModelProvider::DeepSeek,
        requested_harness: HarnessSelection::Claude,
        effective_harness: Backend::Claude,
        provider_source: PreferenceSource::Cli,
        harness_source: PreferenceSource::Cli,
    }
}

fn args_with_codex_catalog_default() -> Args {
    let mut args = parse(&["clud", "--codex"]);
    args.resolved_model_selection = crate::provider_catalog::resolve_for_launch(
        ModelProvider::Codex,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap();
    args
}

#[test]
fn native_codex_catalog_default_is_sol_at_low_effort() {
    let args = args_with_codex_catalog_default();
    let target =
        crate::backend::resolve_launch_target(false, true, false, None, None, None).unwrap();
    let plan = build_launch_plan_for_target(&args, target, "codex");
    assert_eq!(
        plan.command
            .windows(2)
            .filter(|pair| pair == &["-m", "gpt-6-sol"])
            .count(),
        1,
        "{}",
        plan.command.join(" ")
    );
    assert_eq!(
        plan.command
            .windows(2)
            .filter(|pair| pair == &["-c", "model_reasoning_effort=\"low\""])
            .count(),
        1,
        "{}",
        plan.command.join(" ")
    );
    assert!(!plan
        .command
        .iter()
        .any(|arg| arg.contains("terra") || arg == "medium"));
}

#[test]
fn codex_through_claude_catalog_default_is_sol_at_low_effort() {
    let args = args_with_codex_catalog_default();
    let plan = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert_eq!(
        plan.command
            .windows(2)
            .filter(|pair| pair == &["--model", "clud-claude-codex-sol"])
            .count(),
        1,
        "{}",
        plan.command.join(" ")
    );
    assert_eq!(
        plan.command
            .windows(2)
            .filter(|pair| pair == &["--effort", "low"])
            .count(),
        1,
        "{}",
        plan.command.join(" ")
    );
    assert!(!plan
        .command
        .iter()
        .any(|arg| arg.contains("terra") || arg == "medium"));
}

fn deepseek_harness_target() -> ResolvedLaunchTarget {
    ResolvedLaunchTarget {
        routing_mode: RoutingMode::Direct,
        model_provider: ModelProvider::DeepSeek,
        requested_harness: HarnessSelection::DeepSeek,
        effective_harness: Backend::DeepSeek,
        provider_source: PreferenceSource::Cli,
        harness_source: PreferenceSource::Cli,
    }
}

#[test]
fn deepseek_harness_bare_launch_uses_web_profile() {
    let args = parse(&["clud", "--harness", "deepseek"]);
    let plan = build_launch_plan_for_target(&args, deepseek_harness_target(), "dsh");
    assert_eq!(plan.command, vec!["dsh", "web"]);
    assert_eq!(plan.effective_harness(), Backend::DeepSeek);
}

#[test]
fn deepseek_harness_prompt_uses_headless_profile_without_yolo_flags() {
    let args = parse(&["clud", "--harness", "deepseek", "-p", "hello"]);
    let plan = build_launch_plan_for_target(&args, deepseek_harness_target(), "dsh");
    assert_eq!(plan.command, vec!["dsh", "--profile", "headless", "hello"]);
}

#[test]
fn deepseek_harness_one_shot_builtins_keep_headless_profile() {
    for argv in [
        vec!["clud", "--harness", "deepseek", "up"],
        vec!["clud", "--harness", "deepseek", "rebase"],
        vec!["clud", "--harness", "deepseek", "fix"],
        vec![
            "clud",
            "--harness",
            "deepseek",
            "do",
            "https://github.com/zackees/clud/issues/1036",
        ],
    ] {
        let args = parse(&argv);
        let plan = build_launch_plan_for_target(&args, deepseek_harness_target(), "dsh");
        assert_eq!(
            &plan.command[..3],
            ["dsh", "--profile", "headless"],
            "argv={argv:?}, cmd={:?}",
            plan.command
        );
        assert_eq!(plan.launch_mode, LaunchMode::Subprocess);
    }
}

#[test]
fn deepseek_harness_native_passthrough_does_not_inject_web() {
    let args = parse(&[
        "clud",
        "--harness",
        "deepseek",
        "--",
        "--profile",
        "headless",
        "hello",
    ]);
    let plan = build_launch_plan_for_target(&args, deepseek_harness_target(), "dsh");
    assert_eq!(plan.command, ["dsh", "--profile", "headless", "hello"]);
}

fn kimi_bridge_target() -> ResolvedLaunchTarget {
    ResolvedLaunchTarget {
        routing_mode: RoutingMode::Direct,
        model_provider: ModelProvider::Kimi,
        requested_harness: HarnessSelection::Claude,
        effective_harness: Backend::Claude,
        provider_source: PreferenceSource::Cli,
        harness_source: PreferenceSource::Cli,
    }
}

fn unified_target(provider: ModelProvider) -> ResolvedLaunchTarget {
    ResolvedLaunchTarget {
        routing_mode: RoutingMode::Unified,
        model_provider: provider,
        requested_harness: HarnessSelection::Claude,
        effective_harness: Backend::Claude,
        provider_source: PreferenceSource::Cli,
        harness_source: PreferenceSource::Cli,
    }
}

#[test]
fn test_bridge_disallows_plan_mode_even_when_interactive() {
    // The reported bug: an ordinary interactive question on this bridge turned
    // into an unprompted planning session. Suppression must not be gated on
    // `--unattended`.
    let args = parse(&["clud"]);
    let p = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert!(p
        .command
        .iter()
        .any(|a| a == "--disallowedTools=EnterPlanMode,Task"));
}

#[test]
fn test_bridge_permanently_disallows_task_subagents() {
    let args = parse(&["clud", "--allow-plan-mode"]);
    let p = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert!(p.command.iter().any(|a| a == "--disallowedTools=Task"));
}

#[test]
fn test_bridge_leaves_ask_user_question_alone() {
    // Only plan mode is the complaint; multiple-choice questions stay usable
    // in an interactive session.
    let args = parse(&["clud"]);
    let p = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert!(!p.command.iter().any(|a| a.contains("AskUserQuestion")));
}

#[test]
fn test_allow_plan_mode_restores_plan_mode_on_the_bridge() {
    let args = parse(&["clud", "--allow-plan-mode"]);
    let p = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert!(p.allow_plan_mode);
    assert!(p.command.iter().any(|a| a == "--disallowedTools=Task"));
    // And the flag itself must not leak to the backend as passthrough.
    assert!(!p.command.iter().any(|a| a == "--allow-plan-mode"));
}

#[test]
fn test_allow_plan_mode_does_not_re_enable_it_for_unattended_runs() {
    // `--unattended` has its own, older reason to strip plan mode (a loop that
    // parks on a human never finishes). Opting into plan mode must not defeat
    // that; the bridge rule is the only thing `--allow-plan-mode` turns off.
    let args = parse(&["clud", "--allow-plan-mode", "--unattended", "-p", "hi"]);
    let p = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert!(p
        .command
        .iter()
        .any(|a| a == "--disallowedTools=EnterPlanMode,Task,AskUserQuestion"));
}

#[test]
fn test_plain_claude_keeps_plan_mode_interactively() {
    // Narrow rule: only non-Claude providers routing through the Claude harness
    // (Codex, DeepSeek) are affected.
    let p = plan(&["clud"]);
    assert!(!p.command.iter().any(|a| a.starts_with("--disallowedTools")));
}

#[test]
fn test_deepseek_bridge_disallows_plan_mode() {
    // DeepSeek models driving the Claude harness self-invoke EnterPlanMode
    // the same way Codex models do (zackees/clud#841 follow-up).
    // Unlike Codex, DeepSeek keeps `Task` — only `EnterPlanMode` is stripped.
    let args = parse(&["clud"]);
    let p = build_launch_plan_for_target(&args, deepseek_bridge_target(), "claude");
    let disallowed = p
        .command
        .iter()
        .find(|a| a.starts_with("--disallowedTools"))
        .map(|a| a.as_str())
        .unwrap_or("");
    assert!(
        disallowed.contains("EnterPlanMode"),
        "expected EnterPlanMode disallowed, got: {disallowed:?}"
    );
    assert!(
        !disallowed.contains("Task"),
        "Task should not be disallowed for DeepSeek, got: {disallowed:?}"
    );
}

#[test]
fn test_allow_plan_mode_restores_plan_mode_on_deepseek_bridge() {
    let args = parse(&["clud", "--allow-plan-mode"]);
    let p = build_launch_plan_for_target(&args, deepseek_bridge_target(), "claude");
    assert!(!p
        .command
        .iter()
        .any(|a| a.contains("EnterPlanMode") && a.starts_with("--disallowedTools")));
}

/// #937 Phase 3 / #936 "Decisions": "Do not copy DeepSeek-specific plan-mode
/// suppression without Kimi-specific failing evidence." `is_non_claude_
/// claude_harness_bridge`'s `matches!` is not compiler-exhaustive, so the
/// absence of a `ModelProvider::Kimi` arm there is a silent, deliberate
/// choice rather than something the compiler forces a reviewer to notice.
/// This test makes that choice visible: an interactive Kimi-via-Claude
/// launch (exercised through a real interactive `parse`, not `--unattended`
/// or `loop`, which strip plan mode for an unrelated reason) must keep
/// `EnterPlanMode` available.
#[test]
fn test_kimi_bridge_does_not_suppress_plan_mode() {
    let args = parse(&["clud"]);
    let p = build_launch_plan_for_target(&args, kimi_bridge_target(), "claude");
    assert!(
        !p.command
            .iter()
            .any(|a| a.starts_with("--disallowedTools") && a.contains("EnterPlanMode")),
        "Kimi must not suppress EnterPlanMode without Kimi-specific evidence (#936): {:?}",
        p.command
    );
}

#[test]
fn test_plain_codex_harness_keeps_getting_no_claude_only_flag() {
    // Codex harness has no EnterPlanMode surface and rejects the flag.
    let p = plan(&["clud", "--codex"]);
    assert!(!p.command.iter().any(|a| a.starts_with("--disallowedTools")));
}

/// The Claude harness receives the registered discovery ID while the plan
/// retains the provider wire selection for bridge/default compatibility.
#[test]
fn test_bridge_uses_a_discovery_id_and_separate_effort() {
    let args = parse(&["clud", "--model", "terra@high"]);
    let p = build_launch_plan_for_target(&args, bridge_target(), "claude");
    let model_index = p.command.iter().position(|a| a == "--model").unwrap();
    assert_eq!(p.command[model_index + 1], "clud-claude-codex-terra");
    assert_eq!(p.codex_model.as_deref(), Some("gpt-5.6-terra@high"));
    assert!(p
        .command
        .windows(2)
        .any(|pair| pair == ["--effort", "high"]));
}

/// DD-059: a plain `clud --deepseek` launch carries the catalog default
/// effort on the harness's own `--effort` session flag (an initial value, so
/// `/effort` stays the live session control) instead of a pinned
/// `CLAUDE_CODE_EFFORT_LEVEL` env var. `main` populates
/// `resolved_model_selection` from `resolve_for_launch` with the catalog
/// default enabled for direct launches; the test mirrors that seam.
#[test]
fn test_deepseek_bridge_catalog_default_effort_rides_the_session_flag() {
    let mut args = parse(&["clud", "--deepseek"]);
    args.resolved_model_selection = crate::provider_catalog::resolve_for_launch(
        ModelProvider::DeepSeek,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap();
    let p = build_launch_plan_for_target(&args, deepseek_bridge_target(), "claude");
    assert!(
        p.command.windows(2).any(|pair| pair == ["--effort", "low"]),
        "expected --effort low from the catalog default, got: {}",
        p.command.join(" ")
    );
}

/// Issue #955: the shared catalog is the extension seam for both the native
/// Codex harness and Claude's discovery namespace. This deliberately iterates
/// rows instead of restating Sol/Terra/Luna in the adapter test.
#[test]
fn every_registered_codex_model_has_native_and_claude_harness_addresses() {
    for model in crate::provider_catalog::models_for_provider(ModelProvider::Codex) {
        let args = parse(&["clud", "--model", model.cli_id, "--effort", "high"]);
        let claude = build_launch_plan_for_target(&args, bridge_target(), "claude");
        let discovery_id = model.discovery_id.expect("Codex discovery id");
        assert!(
            claude
                .command
                .windows(2)
                .any(|pair| pair == ["--model", discovery_id]),
            "{}",
            claude.command.join(" ")
        );
        assert!(claude
            .command
            .windows(2)
            .any(|pair| pair == ["--effort", "high"]));

        let native_target =
            crate::backend::resolve_launch_target(false, true, false, None, None, None).unwrap();
        let native = build_launch_plan_for_target(&args, native_target, "codex");
        assert!(
            native
                .command
                .windows(2)
                .any(|pair| pair == ["-m", model.wire_id]),
            "{}",
            native.command.join(" ")
        );
    }
}

#[test]
fn unified_initial_routes_use_discovery_ids_and_keep_plan_mode() {
    for (provider, model, discovery_id) in [
        (ModelProvider::Codex, "codex-luna", "clud-claude-codex-luna"),
        (
            ModelProvider::DeepSeek,
            "deepseek-v4-pro",
            "clud-claude-deepseek-v4-pro-0813",
        ),
    ] {
        let args = parse(&["clud", "--model", model, "--effort", "high"]);
        let plan = build_launch_plan_for_target(&args, unified_target(provider), "claude");
        assert!(
            plan.command
                .windows(2)
                .any(|pair| pair == ["--model", discovery_id]),
            "{}",
            plan.command.join(" ")
        );
        assert!(
            plan.command
                .windows(2)
                .any(|pair| pair == ["--effort", "high"]),
            "{}",
            plan.command.join(" ")
        );
        assert!(!plan
            .command
            .iter()
            .any(|arg| arg == "--disallowedTools=EnterPlanMode"));
        assert_eq!(plan.codex_model, None);
    }
}

#[test]
fn model_less_bridge_effort_pins_the_reviewed_default_model() {
    let args = parse(&["clud", "--effort", "high"]);
    let plan = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["--model", "clud-claude-codex-sol"]));
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["--effort", "high"]));
    assert_eq!(plan.codex_model.as_deref(), Some("gpt-6-sol@high"));
}

#[test]
fn bridge_keeps_none_effort_on_the_discovery_id() {
    let args = parse(&["clud", "--model", "luna", "--effort", "none"]);
    let plan = build_launch_plan_for_target(&args, bridge_target(), "claude");
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["--model", "clud-claude-codex-luna@none"]));
    assert!(!plan.command.iter().any(|arg| arg == "--effort"));
}

#[test]
fn native_codex_receives_normalized_model_and_effort_as_separate_settings() {
    let args = parse(&[
        "clud",
        "--codex",
        "--model",
        "terra@high",
        "--effort",
        "high",
    ]);
    let target =
        crate::backend::resolve_launch_target(false, true, false, None, None, None).unwrap();
    let plan = build_launch_plan_for_target(&args, target, "codex");
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["-m", "gpt-5.6-terra"]));
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["-c", "model_reasoning_effort=\"high\""]));
}

#[test]
fn native_claude_receives_effort_as_a_session_flag() {
    let args = parse(&["clud", "--claude", "--model", "opus", "--effort", "high"]);
    let target =
        crate::backend::resolve_launch_target(true, false, false, None, None, None).unwrap();
    let plan = build_launch_plan_for_target(&args, target, "claude");
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["--model", "opus"]));
    assert!(plan
        .command
        .windows(2)
        .any(|pair| pair == ["--effort", "high"]));
}

/// An id we do not know is forwarded untouched — the alias table gives short
/// names, it does not gate which models are reachable.
#[test]
fn test_bridge_forwards_an_unknown_full_model_id_untouched() {
    let args = parse(&["clud", "--model", "gpt-5.7-nova"]);
    let p = build_launch_plan_for_target(&args, bridge_target(), "claude");
    let model_index = p.command.iter().position(|a| a == "--model").unwrap();
    assert_eq!(p.command[model_index + 1], "gpt-5.7-nova");
    assert_eq!(p.codex_model.as_deref(), Some("gpt-5.7-nova"));
}

/// A typo is left alone here and rejected by the bridge, which owns the
/// message. What must not happen is a silent substitution of the default.
#[test]
fn test_bridge_does_not_substitute_a_default_for_an_unknown_alias() {
    let args = parse(&["clud", "--model", "tera"]);
    let p = build_launch_plan_for_target(&args, bridge_target(), "claude");
    let model_index = p.command.iter().position(|a| a == "--model").unwrap();
    assert_eq!(p.command[model_index + 1], "tera");
    assert_eq!(p.codex_model, None);
}

/// Off the bridge, `--model` is the harness's own flag and must not be
/// rewritten: `clud --model sonnet` selects a Claude model, not a Codex one.
#[test]
fn test_native_routes_never_rewrite_the_model_flag() {
    let p = plan(&["clud", "--model", "sonnet"]);
    let model_index = p.command.iter().position(|a| a == "--model").unwrap();
    assert_eq!(p.command[model_index + 1], "sonnet");
    assert_eq!(p.codex_model, None);
}

#[test]
fn test_plan_mode_suppression_notice_is_green_and_tty_only() {
    let args = parse(&["clud"]);
    let notice = plan_mode_suppression_notice(&args, bridge_target(), true, false).unwrap();
    assert!(notice.starts_with("\x1b[32m"));
    assert!(notice.ends_with("\x1b[0m"));
    assert!(notice.contains("--allow-plan-mode"));

    // Not a terminal, or structured output wanted: stay silent.
    assert_eq!(
        plan_mode_suppression_notice(&args, bridge_target(), false, false),
        None
    );
    assert_eq!(
        plan_mode_suppression_notice(&args, bridge_target(), true, true),
        None
    );

    // Nothing suppressed => nothing announced.
    let allowed = parse(&["clud", "--allow-plan-mode"]);
    assert_eq!(
        plan_mode_suppression_notice(&allowed, bridge_target(), true, false),
        None
    );
}

#[test]
fn test_unattended_disallows_interactive_tools() {
    let p = plan(&["clud", "--unattended", "-p", "hello"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        vec![
            "claude",
            "--dangerously-skip-permissions",
            "--disallowedTools=EnterPlanMode,AskUserQuestion",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "-p",
            "hello"
        ]
    );
}

#[test]
fn test_unattended_emits_a_single_argv_token() {
    // `claude` declares `--disallowedTools <tools...>` as variadic: the
    // space-separated spelling swallows the following token, so a later
    // `-p <prompt>` silently vanishes and claude exits 0 producing nothing.
    // Keeping this one `=`-bound token is what makes the flag order-safe.
    let p = plan(&["clud", "--unattended", "-p", "hello"]);
    assert!(!p.command.iter().any(|a| a == "--disallowedTools"));
    assert!(p
        .command
        .iter()
        .any(|a| a == "--disallowedTools=EnterPlanMode,AskUserQuestion"));
    // The prompt must survive intact directly after its own flag.
    let idx = p.command.iter().position(|a| a == "-p").unwrap();
    assert_eq!(p.command[idx + 1], "hello");
}

#[test]
fn test_unattended_is_a_noop_for_codex_harness() {
    // Codex has no EnterPlanMode/AskUserQuestion surface, and it would reject
    // a claude-only flag.
    let p = plan(&["clud", "--codex", "--unattended", "-p", "hello"]);
    assert!(!p.command.iter().any(|a| a.starts_with("--disallowedTools")));
}

#[test]
fn test_unattended_is_not_forwarded_as_passthrough() {
    // `--unattended` must be in `bool_flags` in `split_known_unknown`, or the
    // splitter routes it to the backend, which errors on an unknown flag.
    let p = plan(&["clud", "--unattended", "-p", "hello"]);
    assert!(!p.command.iter().any(|a| a == "--unattended"));
}

/// #1317: Claude attribution is hidden by default and opt-in with
/// `--coauthor[=TAG]`. The choice rides on the plan, so the argv is unchanged
/// and the flag is never forwarded to the backend as passthrough.
#[test]
fn test_coauthor_is_hidden_by_default() {
    if std::env::var_os(crate::attribution::COAUTHOR_ENV).is_some() {
        return;
    }
    let p = plan(&["clud", "-p", "hello"]);
    assert_eq!(p.coauthor, crate::attribution::Coauthor::Hidden);
    assert!(!p.command.iter().any(|a| a == "--settings"));
}

#[test]
fn test_coauthor_flag_opts_in_to_the_harness_attribution() {
    let p = plan(&["clud", "--coauthor", "-p", "hello"]);
    assert_eq!(p.coauthor, crate::attribution::Coauthor::Harness);
    assert!(!p.command.iter().any(|a| a.starts_with("--coauthor")));
    assert_eq!(p.command.last().map(String::as_str), Some("hello"));
}

#[test]
fn test_coauthor_flag_carries_a_tag() {
    let p = plan(&[
        "clud",
        "--coauthor=Co-Authored-By: Bot <b@x>",
        "-p",
        "hello",
    ]);
    assert_eq!(
        p.coauthor,
        crate::attribution::Coauthor::Tag("Co-Authored-By: Bot <b@x>".to_string())
    );
    assert!(!p.command.iter().any(|a| a.starts_with("--coauthor")));
}

/// A bare `--coauthor` must not swallow the next word as its tag: the value
/// is `=`-joined only.
#[test]
fn test_bare_coauthor_does_not_eat_the_next_argument() {
    let args = parse(&["clud", "--coauthor", "loop", "task"]);
    assert_eq!(args.coauthor.as_deref(), Some(""));
    assert!(matches!(
        args.command,
        Some(crate::args::Command::Loop { .. })
    ));
}

#[test]
fn test_safe_mode_no_yolo() {
    let p = plan(&["clud", "--safe", "-p", "hello"]);
    assert_eq!(
        command_without_deletion_policy(&p),
        vec![
            "claude",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "-p",
            "hello"
        ]
    );
}

#[test]
fn unsafe_launch_omits_clud_deletion_instructions_for_both_harnesses() {
    let claude = plan(&["clud", "--unsafe", "-p", "hello"]);
    assert!(claude.unsafe_mode);
    assert!(claude.command.iter().all(|arg| !arg.contains("safe-rm")));
    assert!(claude
        .command
        .iter()
        .any(|arg| arg == "--dangerously-skip-permissions"));

    let codex = plan(&["clud", "--codex", "--unsafe", "-p", "hello"]);
    assert!(codex.unsafe_mode);
    assert!(codex_config_values(&codex)
        .iter()
        .all(|value| !value.contains("safe-rm")));
    assert!(codex
        .command
        .iter()
        .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));

    let user_instructions = plan(&[
        "clud",
        "--codex",
        "--unsafe",
        "--",
        "-c",
        "developer_instructions=\"keep my instructions\"",
    ]);
    assert!(codex_config_values(&user_instructions)
        .iter()
        .any(|value| value.contains("keep my instructions")));
}

#[test]
fn codex_config_long_form_developer_instructions_are_merged() {
    for passthrough in [
        &["--config", "developer_instructions=\"be terse\""][..],
        &["--config=developer_instructions=\"be terse\""][..],
    ] {
        let mut raw = vec!["clud", "--codex", "--"];
        raw.extend_from_slice(passthrough);
        let p = plan(&raw);
        let overrides: Vec<&String> = p
            .command
            .iter()
            .filter(|arg| arg.contains("developer_instructions"))
            .collect();
        assert_eq!(overrides.len(), 1, "{passthrough:?}: {overrides:?}");
        let merged = codex_config_values(&p)
            .into_iter()
            .find(|value| value.starts_with("developer_instructions="))
            .expect("clud's developer_instructions override");
        assert!(merged.contains("be terse"), "{merged}");
        assert!(merged.len() > "developer_instructions=\"be terse\"".len() + 40);
    }
}

/// (DD-086). `cargo test` may or may not have a terminal, so ask.
fn console_launch_mode() -> LaunchMode {
    if crate::session::terminals_are_interactive() {
        LaunchMode::Pty
    } else {
        LaunchMode::Subprocess
    }
}

// -----------------------------------------------------------------
// Issue #61: --repeat scheduling tests
// -----------------------------------------------------------------
//
// These cover three areas:
//   1. parse_repeat_interval: every accepted form + the rejection
//      cases the issue calls out (negative, fractional, unknown
//      unit, overflow, empty, zero, missing unit, missing value).
//   2. repeat_implies_no_done_warning: the precedence ladder
//      (--repeat alone fires; explicit --no-done suppresses;
//      --done <path> suppresses + restores contract).
//   3. next_run_at_millis: the no-overlap invariant. The next run
//      time is always derived from *completion*, so a long-running
//      iteration pushes the schedule out instead of overlapping.

#[path = "tests/builtin_prompts.rs"]
mod builtin_prompts;

#[path = "tests/codex_native_routes.rs"]
mod codex_native_routes;

#[path = "tests/grind_launch.rs"]
mod grind_launch;

#[path = "tests/video_launch.rs"]
mod video_launch;

#[path = "tests/loop_contract.rs"]
mod loop_contract;

#[path = "tests/repeat_schedule.rs"]
mod repeat_schedule;

#[path = "tests/repeat_execution.rs"]
mod repeat_execution;

#[test]
fn landing_instructions_cover_both_backends_and_no_land() {
    for backend in ["--claude", "--codex"] {
        let default = plan(&["clud", backend, "-p", "hello"]);
        assert!(default
            .command
            .iter()
            .any(|arg| arg.contains("Land the work") && !arg.contains(['\r', '\n'])));
        let off = plan(&["clud", backend, "--no-land", "-p", "hello"]);
        assert!(off.command.iter().all(|arg| !arg.contains("Land the work")));
    }
}

#[test]
fn tool_call_rules_reach_both_backends_even_without_landing() {
    for backend in ["--claude", "--codex"] {
        for extra in [None, Some("--no-land")] {
            let mut argv = vec!["clud", backend];
            argv.extend(extra);
            argv.extend(["-p", "hello"]);
            let p = plan(&argv);
            assert!(
                p.command
                    .iter()
                    .any(|arg| arg.contains(super::builder::TOOL_CALL_RULES)),
                "{backend} {extra:?} is missing the tool-call rules"
            );
        }
    }
}

#[test]
fn print_turns_use_compact_protocol_but_explicit_native_output_is_preserved() {
    for backend in ["--claude", "--codex"] {
        let p = plan(&["clud", backend, "-p", "hello"]);
        assert!(p.prompt_summary, "{:?}", p.command);
        assert_eq!(last_arg(&p), "hello");
        let raw = if backend == "--claude" {
            vec!["clud", backend, "-p", "hello", "--", "--output-format=json"]
        } else {
            vec!["clud", backend, "-p", "hello", "--", "--json"]
        };
        let p = plan(&raw);
        assert!(!p.prompt_summary, "{:?}", p.command);
    }
    let p = plan(&["clud", "--claude", "--pty", "-p", "hello"]);
    assert!(!p.prompt_summary);
    let p = plan(&["clud", "--claude"]);
    assert!(!p.prompt_summary);
}

#[test]
fn flag_shaped_literal_prompts_are_not_native_output_requests() {
    for backend in ["--claude", "--codex"] {
        let p = plan(&["clud", backend, "--prompt=--json"]);
        assert!(p.prompt_summary, "{:?}", p.command);
    }
}

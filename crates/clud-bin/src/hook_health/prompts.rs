use std::collections::BTreeSet;
use std::path::PathBuf;

use running_process::{NativeProcess, ProcessConfig, StderrMode, StdinMode};

use super::inspect::inspect_current;
use super::types::{HookConfigError, HookFrontend, HookHealthReport, NormalizedHook, RepairAction};
use super::utils::{display_path, join_matchers};
use super::CATCH_ALL_MATCHER;
use crate::args::Args;
use crate::backend::{ModelProvider, ResolvedLaunchTarget};
use crate::command;
use crate::subprocess;

pub(in crate::hook_health) fn migration_prompt(
    hook: &NormalizedHook,
    target: HookFrontend,
) -> String {
    let target_path = match target {
        HookFrontend::Claude => ".claude/settings.json",
        HookFrontend::Codex => ".codex/hooks.json",
    };
    format!(
        "\
You are fixing Claude/Codex PreToolUse hook parity for this repository.

Migrate or copy exactly one hook from {source} to {target}.

Source config: {source_path}
Target config: {target_path}
Event: PreToolUse
Matcher/tool pattern: {matcher}

Preserve the hook intent conservatively. Claude and Codex PreToolUse hooks are similar but not full parity: permission decisions, input rewriting, context injection, matcher names, and tool coverage can differ. Do not pretend unsupported target behavior is enforceable; document unsupported semantics in the target config comment or adjacent note if needed.

After editing the target config, validate that the target hook config parses. If parsing fails, fix only the affected target hook/config file and reparse. Stop when the migrated hook parses cleanly or when the migration is blocked by unsupported target semantics.
",
        source = hook.frontend.display_name(),
        target = target.display_name(),
        source_path = display_path(&hook.source_path),
        target_path = target_path,
        matcher = hook.matcher
    )
}

pub(in crate::hook_health) fn catch_all_migration_prompt(
    hooks: &[&NormalizedHook],
    target: HookFrontend,
) -> String {
    let first = hooks[0];
    let target_path = match target {
        HookFrontend::Claude => ".claude/settings.json",
        HookFrontend::Codex => ".codex/hooks.json",
    };
    let matchers = hooks
        .iter()
        .map(|hook| hook.matcher.clone())
        .collect::<BTreeSet<_>>();
    let command = first.command.as_deref().unwrap_or("(no command recorded)");
    format!(
        "\
You are fixing Claude/Codex PreToolUse hook parity for this repository.

Migrate or copy these equivalent hooks from {source} to {target} as one Codex catch-all hook.

Source config: {source_path}
Target config: {target_path}
Event: PreToolUse
Matcher/tool pattern: {catch_all}
Source matchers covered by the catch-all hook: {matchers}
Shared command: {command}

Use matcher \"*\" for the target Codex hook so one reviewed hook can cover all matching tools and avoid repeated Codex trust approvals for the same command. Preserve the hook intent conservatively. Claude and Codex PreToolUse hooks are similar but not full parity: permission decisions, input rewriting, context injection, matcher names, and tool coverage can differ. Do not pretend unsupported target behavior is enforceable; document unsupported semantics in the target config comment or adjacent note if needed.

After editing the target config, validate that the target hook config parses. If parsing fails, fix only the affected target hook/config file and reparse. Stop when the migrated hook parses cleanly or when the migration is blocked by unsupported target semantics.
",
        source = first.frontend.display_name(),
        target = target.display_name(),
        source_path = display_path(&first.source_path),
        target_path = target_path,
        catch_all = CATCH_ALL_MATCHER,
        matchers = join_matchers(&matchers),
        command = command
    )
}

pub(in crate::hook_health) fn validation_prompt_actions(
    report: &HookHealthReport,
) -> Vec<RepairAction> {
    report
        .claude
        .parse_errors
        .iter()
        .chain(report.codex.parse_errors.iter())
        .map(|error| RepairAction::ValidationPrompt {
            frontend: error.frontend,
            config_path: error.path.clone(),
            prompt: validation_prompt(error),
        })
        .collect()
}

pub(in crate::hook_health) fn validation_prompt(error: &HookConfigError) -> String {
    format!(
        "\
You are fixing a malformed {frontend} hook config.

Config file: {path}
Parser error: {error}

Fix only this hook config file. The expected PreToolUse shape is a JSON object with a `hooks` object containing a `PreToolUse` array. Each array item should include a matcher/tool pattern and a `hooks` handler array. Reparse the file after editing and stop when it parses cleanly.
",
        frontend = error.frontend.config_name(),
        path = display_path(&error.path),
        error = error.error
    )
}

pub(in crate::hook_health) fn run_validation_followups(
    args: &Args,
    launch_target: ResolvedLaunchTarget,
    report: &HookHealthReport,
) -> i32 {
    let mut current = report.clone();
    for attempt in 0..3 {
        let prompts = validation_prompt_actions(&current)
            .into_iter()
            .filter_map(|action| match action {
                RepairAction::ValidationPrompt { prompt, .. } => Some(prompt),
                _ => None,
            })
            .collect::<Vec<_>>();
        if prompts.is_empty() {
            return 0;
        }
        for prompt in prompts {
            eprintln!(
                "[clud] running hook validation follow-up prompt {}/3",
                attempt + 1
            );
            match run_backend_prompt(args, launch_target, prompt) {
                Ok(0) => {}
                Ok(code) => return code,
                Err(error) => {
                    eprintln!("[clud] error: {error}");
                    return 1;
                }
            }
        }
        current = inspect_current();
    }
    eprintln!("[clud] hook config still fails to parse after validation follow-up prompts");
    1
}

pub(in crate::hook_health) fn run_backend_prompt(
    args: &Args,
    launch_target: ResolvedLaunchTarget,
    prompt: String,
) -> Result<i32, String> {
    let selected_backend = launch_target.effective_harness;
    // Issue #934: PATH, then the documented native install location — a
    // stale PATH must not make a present binary look missing.
    let backend_path = crate::backend_bootstrap::locate_installed_backend(selected_backend)
        .map(|path| path.to_string_lossy().to_string())
        .ok_or_else(|| {
            format!(
                "{} not found on PATH or at its native install location; cannot run hook migration prompt",
                selected_backend.executable_name()
            )
        })?;
    let plan = backend_prompt_launch_plan(args, launch_target, prompt, &backend_path);
    let config = ProcessConfig {
        command: subprocess::command_spec_for_subprocess(plan.command),
        cwd: plan.cwd.map(PathBuf::from),
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new(config);
    process
        .start()
        .map_err(|error| format!("failed to start backend prompt: {error}"))?;
    process
        .wait(None)
        .map_err(|error| format!("waiting for backend prompt: {error}"))
}

fn backend_prompt_launch_plan(
    args: &Args,
    launch_target: ResolvedLaunchTarget,
    prompt: String,
    backend_path: &str,
) -> command::LaunchPlan {
    let launch_args = Args {
        web_term: false,
        kitty_term: false,
        set_web_term: None,
        provider: None,
        unified: false,
        mode: None,
        failover: None,
        failover_allow_metered: false,
        allow_model: Vec::new(),
        provider_only: Vec::new(),
        zdr: false,
        set_openrouter_zdr: None,
        saved_openrouter_zdr: None,
        no_data_collection: false,
        redact_secrets: false,
        no_redact_secrets: false,
        effort: None,
        context_window: None,
        prompt: Some(prompt),
        message: None,
        continue_session: false,
        resume: None,
        claude: launch_target.model_provider == ModelProvider::Claude,
        codex: launch_target.model_provider == ModelProvider::Codex,
        deepseek: launch_target.model_provider == ModelProvider::DeepSeek,
        kimi: launch_target.model_provider == ModelProvider::Kimi,
        openrouter: launch_target.model_provider == ModelProvider::OpenRouter,
        harness: Some(launch_target.requested_harness),
        subprocess: true,
        pty: false,
        graphics: crate::graphics::GraphicsMode::Off,
        graphics_image: None,
        demo_gfx_sixel: false,
        model: args.model.clone(),
        safe: args.safe,
        no_land: args.no_land,
        unsafe_mode: args.unsafe_mode,
        unattended: args.unattended,
        allow_plan_mode: args.allow_plan_mode,
        last: args.last,
        resume_mode: args.resume_mode,
        coauthor: args.coauthor.clone(),
        dry_run: false,
        do_meta: false,
        do_rendered_prompt: None,
        do_goal: None,
        detach: false,
        detachable: false,
        session_name: None,
        transcript: None,
        backlog_size: None,
        verbose: args.verbose,
        no_dnd: true,
        clean_worktrees: false,
        fix_hooks: false,
        no_fix_hooks: false,
        stale_after: "1d".to_string(),
        yes: false,
        installer: false,
        install_current: false,
        install_version: None,
        no_modify_path: false,
        installer_json: false,
        force: false,
        experimental_daemon_centralized: false,
        daemon_state_dir: None,
        daemon_mode: None,
        no_daemon: true,
        keep_orphans: false,
        quiet_orphans: false,
        explain_orphans: false,
        no_cpu_banner: false,
        command: None,
        passthrough: args.passthrough.clone(),
        codex_config_overrides: args.codex_config_overrides.clone(),
        resolved_model_selection: args.resolved_model_selection.clone(),
        raw_argv: Vec::new(),
        continue_index: None,
        inline_api_key: None,
    };
    command::build_launch_plan_for_target(&launch_args, launch_target, backend_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        resolve_launch_target, Backend, HarnessSelection, LaunchTargetError, ModelProvider,
        PreferenceSource,
    };

    fn args(argv: &[&str]) -> Args {
        Args::parse_from_raw(argv.iter().map(|value| (*value).to_string()).collect())
    }

    /// Guardrail for #937 Phase 2B: `backend_prompt_launch_plan` re-encodes
    /// the resolved provider as `Args`' hand-written `claude`/`codex`/
    /// `deepseek` booleans (a struct literal, not a match -- adding a field
    /// to `Args` fails this literal to compile, which is the guardrail for
    /// *a case existing at all*, but says nothing about whether the boolean
    /// wired to a given provider is the *right* one). This closes that gap:
    /// for every provider with an Anthropic-compat descriptor, resolving a
    /// launch target for that exact provider and threading it through this
    /// function must round-trip back to the same provider on the plan, and
    /// the descriptor's `cli_flag` must be the literal `--<settings_id>`
    /// form these booleans stand in for.
    #[test]
    fn backend_prompt_plan_resolves_the_same_provider_for_every_anthropic_compat_descriptor() {
        for descriptor in crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS {
            assert_eq!(
                descriptor.cli_flag,
                format!("--{}", descriptor.settings_id),
                "cli_flag must be the literal flag spelling of settings_id"
            );
            let target = ResolvedLaunchTarget {
                routing_mode: crate::backend::RoutingMode::Direct,
                model_provider: descriptor.provider,
                requested_harness: crate::backend::HarnessSelection::Default,
                effective_harness: descriptor.provider.native_harness(),
                provider_source: crate::backend::PreferenceSource::Cli,
                harness_source: crate::backend::PreferenceSource::Cli,
            };
            let args = args(&["clud", "--fix-hooks"]);
            let plan =
                backend_prompt_launch_plan(&args, target, "repair hooks".to_string(), "claude");
            assert_eq!(plan.model_provider(), descriptor.provider);
        }
    }

    #[test]
    fn fix_hooks_prompt_uses_the_resolved_cross_route_harness() {
        let args = args(&["clud", "--codex", "--harness", "claude", "--fix-hooks"]);
        let target = resolve_launch_target(
            args.claude,
            args.codex,
            args.deepseek,
            args.harness,
            None,
            None,
        )
        .unwrap();

        let plan = backend_prompt_launch_plan(&args, target, "repair hooks".to_string(), "claude");

        assert_eq!(plan.command[0], "claude");
        assert_eq!(plan.model_provider(), ModelProvider::Codex);
        assert_eq!(plan.requested_harness, Some(HarnessSelection::Claude));
        assert_eq!(plan.effective_harness(), Backend::Claude);
        assert_eq!(plan.provider_source, Some(PreferenceSource::Cli));
        assert_eq!(plan.harness_source, Some(PreferenceSource::Cli));
    }

    #[test]
    fn fix_hooks_prompt_honors_saved_provider_and_harness_preferences() {
        let args = args(&["clud", "--fix-hooks"]);
        let target = resolve_launch_target(
            args.claude,
            args.codex,
            args.deepseek,
            args.harness,
            Some(ModelProvider::Codex),
            Some(HarnessSelection::Claude),
        )
        .unwrap();

        let plan = backend_prompt_launch_plan(&args, target, "repair hooks".to_string(), "claude");

        assert_eq!(plan.command[0], "claude");
        assert_eq!(plan.model_provider(), ModelProvider::Codex);
        assert_eq!(plan.requested_harness, Some(HarnessSelection::Claude));
        assert_eq!(plan.provider_source, Some(PreferenceSource::GlobalSetting));
        assert_eq!(plan.harness_source, Some(PreferenceSource::GlobalSetting));
    }

    #[test]
    fn fix_hooks_rejects_the_unsupported_reverse_cross_route() {
        let args = args(&["clud", "--claude", "--harness", "codex", "--fix-hooks"]);
        assert_eq!(
            resolve_launch_target(
                args.claude,
                args.codex,
                args.deepseek,
                args.harness,
                None,
                None,
            ),
            Err(LaunchTargetError::ClaudeViaCodexUnsupported)
        );
    }
}

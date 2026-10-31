use std::path::{Path, PathBuf};

use crate::args::{Args, Command};
use crate::backend::{
    Backend, HarnessSelection, LaunchMode, ModelProvider, PreferenceSource, ResolvedLaunchTarget,
};
use crate::codex_model::ModelSpec;
use crate::graphics::GraphicsConfig;
use crate::loop_spec::{done_marker_contract, git_root_from};
use crate::provider_catalog;

use super::loop_task::{resolve_loop_task, resolve_marker_paths};
use super::prompts::{
    build_claude_do_goal, build_do_prompt, build_fix_prompt, build_grind_prompt, build_up_prompt,
    do_procedure, push_prompt, push_prompt_interactive, REBASE_PROMPT,
};
use super::types::{HeadlessSession, HeadlessTurnRequest, LaunchPlan, LoopMarkers, RepeatSchedule};

/// #1847 / DD-162: route Codex approval requests to its auto-review subagent
/// instead of the user. It only matters when approvals are not bypassed
/// (`--safe`); YOLO launches raise no approval requests.
const CODEX_APPROVALS_REVIEWER_KEY: &str = "approvals_reviewer";
const CODEX_AUTO_REVIEW_CONFIG: &str = r#"approvals_reviewer="auto_review""#;
const CODEX_PROJECT_DOC_FALLBACK_KEY: &str = "project_doc_fallback_filenames";
const CODEX_MD_PROJECT_DOC_FALLBACK_CONFIG: &str = r#"project_doc_fallback_filenames=["CODEX.md"]"#;
const CLAUDE_MD_PROJECT_DOC_FALLBACK_CONFIG: &str =
    r#"project_doc_fallback_filenames=["CLAUDE.md"]"#;

/// Return a launch error when `grind` cannot honor its interactive-harness
/// contract. Claude is the only harness with the Workflow tool and native `/loop`
/// that the `/grind` router skill needs.
///
/// #1803: `clud grind reconcile` is **not** a launch. It parses as
/// `Command::Grind` but runs the feature-branch repair pass (`grind_reconcile.rs`),
/// which reads GitHub through `gh` and exits with its own status — it never
/// spawns a harness, never opens a `/loop`, and never loads the `/grind` skill.
/// Gating it here read the *resolved* harness, which comes from the saved
/// global preference, so a user whose `~/.clud/settings.json` named a
/// non-Claude default was refused with "requires the Claude harness" from
/// inside a session that really was Claude. Its siblings (`grind-facts`,
/// `grind-scripts`, `clud tool run`) were unaffected because
/// `dispatch_fast_path_command` handles them before any backend is resolved,
/// which is exactly how the `grind` family came to disagree with itself.
///
/// The exemption is deliberately keyed on `args::is_reconcile` — the same
/// predicate `main.rs` dispatches on — so the gate and the dispatch can never
/// drift apart again.
pub fn grind_launch_error(args: &Args, target: ResolvedLaunchTarget) -> Option<&'static str> {
    if matches!(args.command, Some(Command::Do { .. })) && args.do_meta {
        if !matches!(target.effective_harness, Backend::Claude) {
            return Some("meta-issue `clud do` requires the Claude harness, whose Workflow tool and `/loop` the grind router needs; use `--harness claude`");
        }
        if args.subprocess || args.detach || args.detachable {
            return Some("meta-issue `clud do` requires a foreground interactive PTY; remove `--subprocess`, `--detach` or `--detachable`");
        }
        return None;
    }
    let Some(Command::Grind { url }) = &args.command else {
        return None;
    };
    if crate::args::is_reconcile(url.as_deref()) {
        return None;
    }
    if !matches!(target.effective_harness, Backend::Claude) {
        // Name the harness that was actually resolved, and where it came from:
        // a refusal that cannot be diagnosed from the message alone is the
        // failure mode #1803 was reported as.
        return Some(match target.harness_source {
            crate::backend::PreferenceSource::Cli => "`clud grind` requires the Claude harness, whose Workflow tool and `/loop` the `/grind` skill drives; use `--harness claude`",
            _ => "`clud grind` requires the Claude harness, whose Workflow tool and `/loop` the `/grind` skill drives, but the resolved harness is not Claude; use `--harness claude`",
        });
    }
    if args.subprocess {
        return Some("`clud grind` requires an interactive PTY; remove `--subprocess`");
    }
    if args.detach || args.detachable {
        return Some("`clud grind` requires a foreground interactive PTY; remove `--detach` or `--detachable`");
    }
    None
}

/// `clud video` (#1851) loads video-use through Claude Code's session-only
/// `--plugin-dir`, which no other harness has, and needs an interactive
/// session for the skill's propose-then-approve flow.
pub fn video_launch_error(args: &Args, target: ResolvedLaunchTarget) -> Option<&'static str> {
    if !matches!(args.command, Some(Command::Video { .. })) {
        return None;
    }
    if !matches!(target.effective_harness, Backend::Claude) {
        return Some("`clud video` requires the Claude harness, whose `--plugin-dir` loads video-use for this session only; use `--harness claude`");
    }
    if args.subprocess {
        return Some("`clud video` requires an interactive PTY; remove `--subprocess`");
    }
    None
}

/// Returns true when this harness consumes the launch prompt headlessly.
///
/// `loop` and explicit `-p` prompts are orchestrated/unattended for every
/// harness. `grind` is always an interactive Claude-harness session, validated
/// by [`grind_launch_error`], with harness-owned `/loop` repetition.
pub fn interactive_builtin_resume_error(args: &Args, backend: Backend) -> Option<&'static str> {
    let is_builtin = matches!(
        args.command,
        Some(Command::Up { .. })
            | Some(Command::Rebase)
            | Some(Command::Fix { .. })
            | Some(Command::Do { .. })
    );
    (matches!(backend, Backend::Codex) && is_builtin && matches!(args.resume, Some(None)))
        .then_some(
            "`--resume` without a session cannot seed an interactive Codex built-in; \
         use `--continue`, pass `--resume=<session>`, or omit `--resume`",
        )
}

pub fn has_noninteractive_prompt(args: &Args, backend: Backend) -> bool {
    if args.prompt.is_some() {
        return true;
    }
    match &args.command {
        Some(Command::Loop { .. }) => true,
        Some(Command::Grind { .. }) => false,
        Some(Command::Do { target }) => {
            target
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                && matches!(backend, Backend::DeepSeek)
        }
        Some(Command::Up { .. }) | Some(Command::Rebase) | Some(Command::Fix { .. }) => {
            !matches!(backend, Backend::Codex)
        }
        _ => false,
    }
}

/// True when this launch routes a non-Claude model provider through the Claude
/// harness. Codex and DeepSeek both fit this pattern: the model driving the
/// harness is not Claude, and the harness offers it an `EnterPlanMode` tool
/// whose description instructs it to reach for plan mode *proactively* on any
/// non-trivial implementation ask. The result is unrequested planning sessions
/// in the middle of ordinary questions, so plan mode is stripped here
/// regardless of `--unattended` — the interactive case is precisely where it
/// was biting.
///
/// Deliberately narrow: a plain `clud` (Claude provider, Claude harness) keeps
/// plan mode, and `AskUserQuestion` is never touched by this rule.
///
/// Kimi (#937 Phase 3) is deliberately **not** listed here. #936's
/// "Decisions" section is explicit: "Do not copy DeepSeek-specific plan-mode
/// suppression without Kimi-specific failing evidence." `matches!` is not
/// compiler-exhaustive over `ModelProvider`, so adding a new provider here
/// is a silent choice, not a forced one -- see
/// `command::tests::test_kimi_bridge_does_not_suppress_plan_mode`, which
/// pins this omission as deliberate rather than an oversight.
fn is_non_claude_claude_harness_bridge(target: ResolvedLaunchTarget) -> bool {
    target.routing_mode == crate::backend::RoutingMode::Direct
        && matches!(
            target.model_provider,
            ModelProvider::Codex | ModelProvider::DeepSeek
        )
        && matches!(target.effective_harness, Backend::Claude)
}

/// True when this is the Codex-provider / Claude-harness bridge specifically.
/// Codex allows exactly one Claude process, so the `Task` tool (which creates
/// subagents) is stripped for Codex only — DeepSeek doesn't share that limit.
fn is_codex_claude_bridge(target: ResolvedLaunchTarget) -> bool {
    target.routing_mode == crate::backend::RoutingMode::Direct
        && matches!(target.model_provider, ModelProvider::Codex)
        && matches!(target.effective_harness, Backend::Claude)
}

/// Claude launches get clud's MCP bridge so the agent can spawn Codex
/// sub-agents. The Codex->Claude bridge is excluded: it already runs on Codex
/// and allows exactly one Claude process.
fn claude_mcp_bridge_arg(backend: Backend, target: ResolvedLaunchTarget) -> Option<String> {
    if backend != Backend::Claude || is_codex_claude_bridge(target) {
        return None;
    }
    super::mcp_config::claude_mcp_config_arg(&super::mcp_config::current_exe_dir()?)
}

pub fn bridge_suppresses_plan_mode(args: &Args, target: ResolvedLaunchTarget) -> bool {
    !args.allow_plan_mode && is_non_claude_claude_harness_bridge(target)
}

/// Green, stderr, TTY-only notice announcing the [`bridge_suppresses_plan_mode`]
/// suppression and how to undo it. Mirrors
/// [`crate::backend::saved_harness_override_notice`]: suppressed when stderr is
/// not a terminal or the caller wants structured output, so machine-readable
/// stderr stays clean.
pub fn plan_mode_suppression_notice(
    args: &Args,
    target: ResolvedLaunchTarget,
    stderr_is_terminal: bool,
    structured_output: bool,
) -> Option<String> {
    if structured_output || !stderr_is_terminal || !bridge_suppresses_plan_mode(args, target) {
        return None;
    }
    Some(
        "\x1b[32m[clud] Plan mode disabled on the non-Claude bridge \
         (the model can otherwise enter it unprompted). \
         Override with --allow-plan-mode\x1b[0m"
            .to_string(),
    )
}

pub fn build_launch_plan(args: &Args, backend: Backend, backend_path: &str) -> LaunchPlan {
    let target = ResolvedLaunchTarget {
        routing_mode: crate::backend::RoutingMode::Direct,
        model_provider: backend.as_model_provider(),
        requested_harness: HarnessSelection::Default,
        effective_harness: backend,
        provider_source: PreferenceSource::BuiltInDefault,
        harness_source: PreferenceSource::BuiltInDefault,
    };
    build_launch_plan_for_target(args, target, backend_path)
}

pub fn build_launch_plan_for_target(
    args: &Args,
    target: ResolvedLaunchTarget,
    backend_path: &str,
) -> LaunchPlan {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    build_launch_plan_for_target_at(args, target, backend_path, &cwd)
}

/// Build one daemon-owned headless turn through the canonical launch builder.
/// Codex's interactive `resume` and headless `exec resume` grammars differ,
/// so this cannot be represented by CLI `--resume` alone.
pub(crate) fn build_headless_turn_plan(
    args: &Args,
    target: ResolvedLaunchTarget,
    backend_path: &str,
    request: &HeadlessTurnRequest,
) -> Result<LaunchPlan, String> {
    if !request.cwd.is_absolute() {
        return Err("headless turn cwd must be absolute".to_string());
    }
    let backend = target.effective_harness;
    if !matches!(backend, Backend::Claude | Backend::Codex) {
        return Err(format!(
            "headless sessions support only Claude or Codex, not {}",
            backend.executable_name()
        ));
    }
    let mut turn_args = args.clone();
    turn_args.command = None;
    turn_args.prompt = Some(request.message.clone());
    turn_args.message = None;
    turn_args.continue_session = false;
    turn_args.resume = None;
    turn_args.pty = false;
    turn_args.subprocess = true;
    turn_args.passthrough.clear();

    let mut plan = build_launch_plan_for_target_at(&turn_args, target, backend_path, &request.cwd);
    match (backend, &request.session) {
        (Backend::Claude, HeadlessSession::Initial { claude_session_id }) => {
            let id = claude_session_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| "initial Claude turn requires a session id".to_string())?;
            insert_claude_headless_flags(&mut plan.command, "--session-id", id)?;
        }
        (
            Backend::Claude,
            HeadlessSession::Resume {
                provider_session_id,
            },
        ) => {
            require_provider_id(provider_session_id)?;
            insert_claude_headless_flags(&mut plan.command, "--resume", provider_session_id)?;
        }
        (Backend::Codex, HeadlessSession::Initial { claude_session_id }) => {
            if claude_session_id.is_some() {
                return Err("initial Codex turn must not carry a Claude session id".to_string());
            }
            insert_codex_headless_flags(&mut plan.command, None)?;
        }
        (
            Backend::Codex,
            HeadlessSession::Resume {
                provider_session_id,
            },
        ) => {
            require_provider_id(provider_session_id)?;
            insert_codex_headless_flags(&mut plan.command, Some(provider_session_id))?;
        }
        _ => unreachable!("only Claude and Codex were accepted above"),
    }
    // This flag means the foreground renderer owns the stream; daemon callers
    // keep raw JSONL for event storage instead.
    plan.stream_json_progress = false;
    plan.launch_mode = LaunchMode::Subprocess;
    Ok(plan)
}

fn require_provider_id(id: &str) -> Result<(), String> {
    (!id.is_empty())
        .then_some(())
        .ok_or_else(|| "resumed headless turn requires a provider session id".to_string())
}

fn insert_claude_headless_flags(
    command: &mut Vec<String>,
    identity_flag: &str,
    session_id: &str,
) -> Result<(), String> {
    let prompt_index = command
        .iter()
        .position(|arg| arg == "-p")
        .ok_or_else(|| "Claude headless plan is missing -p".to_string())?;
    command.splice(
        prompt_index..prompt_index,
        [
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            identity_flag.to_string(),
            session_id.to_string(),
        ],
    );
    Ok(())
}

fn insert_codex_headless_flags(
    command: &mut Vec<String>,
    session_id: Option<&str>,
) -> Result<(), String> {
    let exec_index = command
        .iter()
        .position(|arg| arg == "exec")
        .ok_or_else(|| "Codex headless plan is missing exec".to_string())?;
    let mut flags = Vec::new();
    if session_id.is_some() {
        flags.push("resume".to_string());
    }
    flags.push("--json".to_string());
    if let Some(id) = session_id {
        flags.push(id.to_string());
    }
    command.splice(exec_index + 1..exec_index + 1, flags);
    Ok(())
}

#[cfg(test)]
pub(crate) fn build_launch_plan_at(
    args: &Args,
    backend: Backend,
    backend_path: &str,
    cwd: &Path,
) -> LaunchPlan {
    let target = ResolvedLaunchTarget {
        routing_mode: crate::backend::RoutingMode::Direct,
        model_provider: backend.as_model_provider(),
        requested_harness: HarnessSelection::Default,
        effective_harness: backend,
        provider_source: PreferenceSource::BuiltInDefault,
        harness_source: PreferenceSource::BuiltInDefault,
    };
    build_launch_plan_for_target_at(args, target, backend_path, cwd)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LandBias {
    Merge,
    Pr,
    Off,
}

fn resolve_land_bias(no_land: bool, env: Option<&str>, setting: Option<&str>) -> LandBias {
    if no_land {
        return LandBias::Off;
    }
    match env.or(setting).unwrap_or("merge") {
        "merge" => LandBias::Merge,
        "pr" => LandBias::Pr,
        _ => LandBias::Off,
    }
}

fn land_instructions(args: &Args) -> &'static str {
    let setting = crate::home::user_home()
        .map(|home| crate::clud_settings::load_land_bias_at(&home))
        .unwrap_or(Ok(None))
        .unwrap_or_else(|_| Some(String::new()));
    let env =
        std::env::var_os("CLUD_LAND_BIAS").map(|value| value.into_string().unwrap_or_default());
    match resolve_land_bias(args.no_land, env.as_deref(), setting.as_deref()) {
        LandBias::Merge => super::do_prompt::LAND_CONTRACT,
        LandBias::Pr => "Land the work: when a requested code change belongs in a git repository with a remote, aim to deliver a validated and pushed PR and report its URL. When appropriate branch from the default branch, commit the changes, and run the repository validation. Reference the issue when applicable. Honor the user's explicit instructions, approval requirements, repository rules, branch protection and --safe. Do not merge unless the user requests it.",
        LandBias::Off => "",
    }
}

/// The patterns clud's command hooks refuse most often, stated before the
/// agent tries them: a refused call still costs a full round trip (#2354).
pub(super) const TOOL_CALL_RULES: &str = "Tool-call rules clud enforces (a refused call is a wasted round trip): never wait with a while/until/sleep loop, `watch` or `tail -f`, in Bash or Monitor; use run_in_background and its completion notification, or a tool with its own deadline (pr_merge_watch, clud ci). Prefer `(cd DIR && CMD)` or absolute paths over a bare `cd`. Run one GitHub mutation (push, PR create, merge, issue or API write) per call.";

fn append_instructions(target: &mut String, text: &str) {
    if !text.is_empty() {
        if !target.is_empty() {
            // Literal newlines break argv forwarding through Windows .cmd wrappers.
            target.push(' ');
        }
        target.push_str(text);
    }
}

fn merge_claude_append_prompt(passthrough: &[String], unsafe_mode: bool) -> (String, Vec<String>) {
    let mut prompt = if unsafe_mode {
        String::new()
    } else {
        crate::deletion_rules::generated().instructions
    };
    let mut remaining = Vec::new();
    let mut index = 0;
    while index < passthrough.len() {
        let argument = &passthrough[index];
        let value = if argument == "--append-system-prompt" {
            passthrough.get(index + 1).map(String::as_str)
        } else {
            argument.strip_prefix("--append-system-prompt=")
        };
        if let Some(value) = value {
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str(value);
            index += if argument == "--append-system-prompt" {
                2
            } else {
                1
            };
        } else {
            remaining.push(argument.clone());
            index += 1;
        }
    }
    (prompt, remaining)
}

fn take_codex_developer_instructions(passthrough: &mut Vec<String>) -> Option<String> {
    let parse = |value: &str| {
        value
            .strip_prefix("developer_instructions=")
            .and_then(parse_codex_instruction_override)
    };
    let mut found = None;
    let mut index = 0;
    while index < passthrough.len() {
        // Codex takes `-c <kv>`, `--config <kv>` and `--config=<kv>` (#1901).
        let (value, width) = match passthrough[index].as_str() {
            "-c" | "--config" => (passthrough.get(index + 1).and_then(|v| parse(v)), 2),
            arg => (arg.strip_prefix("--config=").and_then(parse), 1),
        };
        if let Some(value) = value {
            found = Some(value);
            passthrough.drain(index..index + width);
        } else {
            index += 1;
        }
    }
    found
}

fn parse_codex_instruction_override(raw: &str) -> Option<String> {
    format!("value = {raw}")
        .parse::<toml::Table>()
        .ok()?
        .get("value")?
        .as_str()
        .map(str::to_string)
}

#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn build_launch_plan_for_target_at(
    args: &Args,
    target: ResolvedLaunchTarget,
    backend_path: &str,
    cwd: &Path,
) -> LaunchPlan {
    let backend = target.effective_harness;
    let mut cmd = vec![backend_path.to_string()];
    let (mut deletion_instructions, mut passthrough) = if backend == Backend::Claude {
        merge_claude_append_prompt(&args.passthrough, args.unsafe_mode)
    } else {
        (String::new(), args.passthrough.clone())
    };
    let land = land_instructions(args);
    if backend == Backend::Claude {
        append_instructions(&mut deletion_instructions, land);
        append_instructions(&mut deletion_instructions, TOOL_CALL_RULES);
    }
    let codex_passthrough_instructions = (backend == Backend::Codex)
        .then(|| take_codex_developer_instructions(&mut passthrough))
        .flatten();
    let mut iterations = 1u32;
    let mut repeat_schedule: Option<RepeatSchedule> = None;
    let mut task_summary: Option<String> = None;
    let model_selection = args.resolved_model_selection.clone().or_else(|| {
        provider_catalog::resolve(
            Some(target.model_provider),
            args.model.as_deref(),
            args.effort.as_deref(),
            args.context_window.as_deref(),
        )
        .ok()
        .flatten()
    });

    let redact_secrets = if args.redact_secrets {
        Some(true)
    } else if args.no_redact_secrets {
        Some(false)
    } else {
        None
    };
    let redaction = crate::secret_redaction::policy::Policy::resolve(
        target.model_provider,
        model_selection
            .as_ref()
            .and_then(|selection| selection.wire_model.as_deref()),
        redact_secrets,
        false,
    );
    let mut target = target;
    if redaction.enabled && backend == Backend::Claude {
        target.routing_mode = crate::backend::RoutingMode::Unified;
    }

    let codex_uses_exec =
        matches!(backend, Backend::Codex) && has_noninteractive_prompt(args, backend);
    let codex_uses_resume = matches!(backend, Backend::Codex)
        && !codex_uses_exec
        && (args.continue_session || args.resume.is_some());
    // A bare `--resume` opens Codex's session picker. It cannot also carry a
    // generated prompt because Codex would parse that first positional as the
    // session id. `main` rejects the combination; this guard keeps direct plan
    // callers safe by preserving the picker and withholding the prompt.
    let seed_interactive_builtin = !(codex_uses_resume && matches!(args.resume, Some(None)));

    if matches!(backend, Backend::DeepSeek) {
        if has_noninteractive_prompt(args, backend) {
            cmd.extend(["--profile".to_string(), "headless".to_string()]);
        } else if args.passthrough.is_empty() {
            cmd.push("web".to_string());
        }
        // #1829: an OpenRouter launch selects dsh's OpenRouter route through
        // a clud-owned overlay; the runtime writes the file before spawn.
        if let Some(patch) = crate::dsh_harness::plan_patch_path(
            target.model_provider,
            crate::dsh_harness::openrouter_model(model_selection.as_ref()),
        ) {
            cmd.extend(["--patch".to_string(), patch.to_string_lossy().into_owned()]);
        }
    }

    if backend.settings_surface() == crate::backend::SettingsSurface::CodexConfigOverrides {
        for override_value in &args.codex_config_overrides {
            cmd.push("-c".to_string());
            cmd.push(override_value.clone());
        }
        if !codex_reviewer_already_chosen(&args.codex_config_overrides, &passthrough) {
            cmd.push("-c".to_string());
            cmd.push(CODEX_AUTO_REVIEW_CONFIG.to_string());
        }
        if !has_codex_project_doc_fallback_override(&args.codex_config_overrides) {
            if let Some(fallback_config) = codex_project_doc_fallback_config(cwd) {
                cmd.push("-c".to_string());
                cmd.push(fallback_config.to_string());
            }
        }
        if let Some(effort) = model_selection
            .as_ref()
            .and_then(|selection| selection.effort)
        {
            cmd.push("-c".to_string());
            cmd.push(format!("model_reasoning_effort=\"{}\"", effort.as_str()));
        }
    }

    if let (Backend::Claude, Some(Command::Do { target: Some(_) }), true) =
        (backend, &args.command, seed_interactive_builtin)
    {
        // Claude caps the `/goal` condition at 4000 chars (#2372); the full
        // rendered procedure rides in the appended system prompt instead.
        if !deletion_instructions.is_empty() {
            deletion_instructions.push_str("\n\n");
        }
        deletion_instructions.push_str(do_procedure(args.do_rendered_prompt.as_deref()));
    }

    if matches!(backend, Backend::Claude) && !deletion_instructions.is_empty() {
        cmd.push("--append-system-prompt".into());
        cmd.push(deletion_instructions);
    }

    if codex_uses_exec {
        cmd.push("exec".to_string());
    } else if codex_uses_resume {
        cmd.push("resume".to_string());
    }

    if matches!(backend, Backend::Codex) {
        // Codex resolves session-flag hook trust from the subcommand's config
        // layer. Keep these overrides alongside the exec/resume flags.
        let scan = "clud-cmd-scan";
        let matcher = CODEX_HOOK_MATCHER;
        cmd.extend(["-c".into(), format!("hooks.PreToolUse=[{{matcher=\"{matcher}\",hooks=[{{type=\"command\",command=\"{scan}\"}}]}}]")]);
        cmd.extend(["-c".into(), format!("hooks.state={{\"/<session-flags>/config.toml:pre_tool_use:0:0\"={{trusted_hash=\"{}\"}}}}", codex_hook_hash(scan))]);
        let has_passthrough_instructions = codex_passthrough_instructions.is_some();
        let existing_instructions = if has_passthrough_instructions {
            None
        } else {
            existing_codex_instructions(args, cwd)
        };
        let has_user_instructions = has_passthrough_instructions || existing_instructions.is_some();
        let mut instructions = codex_passthrough_instructions
            .or(existing_instructions)
            .unwrap_or_default();
        if !args.unsafe_mode {
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(&crate::deletion_rules::generated().instructions);
        }
        append_instructions(&mut instructions, land);
        append_instructions(&mut instructions, TOOL_CALL_RULES);
        if !instructions.is_empty() || has_user_instructions {
            cmd.extend([
                "-c".into(),
                format!(
                    "developer_instructions={}",
                    toml::Value::String(instructions)
                ),
            ]);
        }
    }

    if !args.safe {
        match backend {
            Backend::Claude => cmd.push("--dangerously-skip-permissions".to_string()),
            Backend::Codex => cmd.push("--dangerously-bypass-approvals-and-sandbox".to_string()),
            Backend::DeepSeek => {}
        }
    }

    // Two independent rules feed one `--disallowedTools` token:
    //
    // 1. `clud loop` is unattended by definition; `--unattended` extends the
    //    same policy to any other launch. Strip both tools that park a run on
    //    a human. `--dangerously-skip-permissions` does not cover these; the
    //    model reaches for them on its own, most often at the top of a `/loop`
    //    iteration.
    // 2. The Codex->Claude bridge allows exactly one Claude process. It strips
    //    the `Task` tool on every bridge launch, because Task creates Claude
    //    subagents whose requests all consume the same provider budget. Plan
    //    mode is also stripped unless explicitly restored; `AskUserQuestion`
    //    survives the bridge rules, so the token is composed rather than fixed.
    //
    // Emitted as one `=`-bound, comma-separated token on purpose. `claude`
    // declares `--disallowedTools <tools...>` as variadic, so the
    // space-separated spelling eats whatever argv token follows it — including
    // a later `-p <prompt>`, which makes claude exit 0 with no output and no
    // diagnostic. A single token cannot swallow anything, so this stays correct
    // wherever it lands relative to the prompt and to unknown-flag passthrough.
    let is_unattended = args.unattended || matches!(args.command, Some(Command::Loop { .. }));
    let mut disallowed: Vec<&str> = Vec::new();
    if matches!(backend, Backend::Claude) {
        // Order matters only for stability of the emitted token, which the
        // tests assert on literally.
        if is_unattended || bridge_suppresses_plan_mode(args, target) {
            disallowed.push("EnterPlanMode");
        }
        if is_codex_claude_bridge(target) {
            disallowed.push("Task");
        }
        if is_unattended {
            disallowed.push("AskUserQuestion");
        }
    }
    if !disallowed.is_empty() {
        cmd.push(format!("--disallowedTools={}", disallowed.join(",")));
    }
    cmd.extend(claude_mcp_bridge_arg(backend, target));

    // A Claude-harness gateway must receive its catalog discovery id, not the
    // provider wire id. Claude Code sees and classifies this value before the
    // bridge can rewrite it; handing it `gpt-5.6-sol@low` makes the
    // harness treat a valid 1M model as an unknown 200K model. The bridge owns
    // the `clud-claude-*` namespace and translates it to the wire id at the
    // request boundary.
    let codex_model = codex_model_selection(args, target, model_selection.as_ref());
    let codex_discovery_route = target.model_provider == ModelProvider::Codex
        && target.effective_harness == Backend::Claude;
    let mut emitted_model =
        if target.routing_mode == crate::backend::RoutingMode::Unified || codex_discovery_route {
            gateway_model_selection(model_selection.as_ref()).or_else(|| args.model.clone())
        } else {
            codex_model.clone().or_else(|| {
                model_selection
                    .as_ref()
                    .and_then(|selection| selection.wire_model.clone())
                    .or_else(|| args.model.clone())
            })
        };
    // Claude Code's session effort flag does not accept `none`. Preserve that
    // one provider-native value on the discovery id; the bridge strips it
    // before sending the real model id and effort to OpenAI.
    if codex_discovery_route
        && model_selection
            .as_ref()
            .and_then(|selection| selection.effort)
            == Some(crate::provider_catalog::EffortLevel::None)
    {
        if let Some(model) = emitted_model.as_mut() {
            model.push_str("@none");
        }
    }
    if let Some(emitted) = emitted_model {
        match backend {
            Backend::Claude => {
                cmd.push("--model".to_string());
                cmd.push(emitted);
            }
            Backend::Codex => {
                cmd.push("-m".to_string());
                cmd.push(emitted);
            }
            Backend::DeepSeek => {}
        }
    }
    // Effort is harness-owned session state and travels independently from
    // the discovered model id. (`none` is the sole exception above because it
    // is not a Claude Code CLI value.)
    if matches!(backend, Backend::Claude) {
        if let Some(effort) = model_selection
            .as_ref()
            .and_then(|selection| selection.effort)
            .filter(|effort| *effort != crate::provider_catalog::EffortLevel::None)
        {
            cmd.push("--effort".to_string());
            cmd.push(effort.as_str().to_string());
        }
    }

    // Codex `resume` selection must precede a generated built-in prompt:
    // `resume [SESSION_ID] [PROMPT]`. Otherwise Codex mistakes the prompt for
    // the session id. `-c` uses the explicit `--last` selector.
    if codex_uses_resume {
        if args.continue_session {
            cmd.push("--last".to_string());
        } else if let Some(Some(session_id)) = &args.resume {
            cmd.push(session_id.clone());
        }
    }

    let mut loop_markers: Option<LoopMarkers> = None;
    match &args.command {
        Some(Command::Loop {
            task,
            loop_count,
            refresh,
            no_done,
            done,
            repeat,
        }) => {
            iterations = *loop_count;
            let git_root = git_root_from(cwd);
            let repeat_interval_secs = repeat
                .as_deref()
                .map(parse_repeat_interval)
                .transpose()
                .unwrap_or_else(|err| {
                    eprintln!("error: invalid --repeat value: {err}");
                    std::process::exit(1);
                });
            repeat_schedule =
                repeat_interval_secs.map(|interval_secs| RepeatSchedule { interval_secs });
            let use_done_markers = done.is_some() || (!*no_done && repeat_schedule.is_none());
            let marker_paths = if use_done_markers {
                Some(resolve_marker_paths(cwd, &git_root, done.as_deref()))
            } else {
                None
            };
            if let Some(ref t) = task {
                let prompt_text = resolve_loop_task(t, &git_root, *refresh);
                task_summary = Some(summarize_task_name(&prompt_text, 50));
                let final_prompt = if let Some(markers) = marker_paths.as_ref() {
                    // Issue #95: feed absolute paths into the contract so the
                    // model writes to the exact path clud is polling, not
                    // some invented alternative like `~/.loop/LOOP.md`.
                    format!(
                        "{}{}",
                        prompt_text,
                        done_marker_contract(&markers.done, &markers.blocked)
                    )
                } else {
                    prompt_text
                };
                let final_prompt = if matches!(backend, Backend::Codex) {
                    format!("{final_prompt}\n\nFor large GitHub issue discussions, use `clud tool run github/issue_context.py -- --repo OWNER/REPO --issue N`; for a pull request with many reviews or review comments, use `clud tool run github/pr_context.py -- --repo OWNER/REPO --pr N` instead of `gh pr view --json reviews,comments`; follow each snapshot's continuation and refresh explicitly. If unavailable, use a bounded read and preserve access to the full issue. For noisy local test runs, use `clud tool run diagnostics/test_report.py -- -- <test command>` to keep a bounded report and private full-output artifact; inspect the artifact with its continuation. If unavailable, retain a bounded failure-focused fallback and preserve the command status.")
                } else {
                    final_prompt
                };
                push_prompt(&mut cmd, backend, final_prompt);
            }
            if let Some(markers) = marker_paths {
                loop_markers = Some(LoopMarkers {
                    done_path: markers.done.to_string_lossy().to_string(),
                    blocked_path: markers.blocked.to_string_lossy().to_string(),
                });
            }
        }
        Some(Command::Up { message, publish }) => {
            if seed_interactive_builtin {
                let prompt = build_up_prompt(message.as_deref(), *publish);
                push_prompt(&mut cmd, backend, prompt);
            }
        }
        Some(Command::Rebase) => {
            if seed_interactive_builtin {
                push_prompt(&mut cmd, backend, REBASE_PROMPT.to_string());
            }
        }
        Some(Command::Fix { url }) => {
            if seed_interactive_builtin {
                let prompt = build_fix_prompt(url.as_deref());
                push_prompt(&mut cmd, backend, prompt);
            }
        }
        Some(Command::Do { target }) => {
            // `do` runs the `/goal` contract, which drives a long interactive
            // Stop-hook loop. Headless execution hides progress and prevents
            // follow-up input, so both Claude and Codex seed an interactive
            // session. `main` resolves the optional target before plan building.
            if seed_interactive_builtin && target.is_some() {
                let prompt = match (backend, target.as_deref()) {
                    (Backend::Claude, Some(target)) => build_claude_do_goal(
                        target,
                        args.do_rendered_prompt
                            .as_deref()
                            .is_some_and(|text| !text.trim().is_empty()),
                    ),
                    _ => build_do_prompt(args.do_rendered_prompt.as_deref()),
                };
                push_prompt_interactive(&mut cmd, prompt);
            }
        }
        Some(Command::Wasm { .. }) => {
            unreachable!("wasm execution is handled directly in main")
        }
        Some(Command::DoPrompt { .. }) => {
            unreachable!("do-prompt is handled directly in main")
        }
        Some(Command::GrindScripts) => {
            unreachable!("grind-scripts is handled directly in main")
        }
        Some(Command::GrindFacts { .. }) => {
            unreachable!("grind-facts is handled directly in main")
        }
        Some(Command::SafeRm { .. }) => {
            unreachable!("safe-rm are handled directly in main")
        }
        Some(Command::InstallAssets { .. }) => {
            unreachable!("install-assets is handled directly in main")
        }
        Some(Command::Grind { url }) => {
            // `main` rejects harnesses without native interactive `/loop`
            // support. Keep this plan free of every clud loop mechanism: the
            // harness owns all continuation and completion decisions.
            let url = url.as_deref().unwrap_or("");
            task_summary = Some(format!("grind {url}"));
            push_prompt_interactive(&mut cmd, build_grind_prompt(url));
        }
        Some(Command::Video { .. }) => {
            // Session-scoped plugin: never a global skill install (DD-163).
            let home = crate::home::user_home().unwrap_or_default();
            task_summary = Some("video".to_string());
            cmd.push("--plugin-dir".to_string());
            cmd.push(
                crate::video::plugin_dir(&home)
                    .to_string_lossy()
                    .into_owned(),
            );
            if seed_interactive_builtin {
                let checkout = crate::video::checkout_dir(&home);
                push_prompt_interactive(&mut cmd, crate::video::seed_prompt(&checkout));
            }
        }
        Some(Command::Serve { .. })
        | Some(Command::Secret(_))
        | Some(Command::Auth { .. })
        | Some(Command::Models { .. })
        | Some(Command::CodexUpdate)
        | Some(Command::DshUpdate)
        | Some(Command::Doctor { .. })
        | Some(Command::Ci { .. })
        | Some(Command::CodexAuth { .. })
        | Some(Command::DeepseekAuth { .. })
        | Some(Command::Attach { .. })
        | Some(Command::Kill { .. })
        | Some(Command::Slay)
        | Some(Command::List)
        | Some(Command::Claim { .. })
        | Some(Command::Top { .. })
        | Some(Command::Logs { .. })
        | Some(Command::Log { .. })
        | Some(Command::Gc { .. })
        | Some(Command::Config { .. })
        | Some(Command::Ui { .. })
        | Some(Command::Trash { .. })
        | Some(Command::Tool { .. })
        | Some(Command::Optimize { .. })
        | Some(Command::Symbols { .. })
        | Some(Command::Test { .. })
        | Some(Command::Settings { .. })
        | Some(Command::Extern { .. })
        | Some(Command::Daemon { .. })
        | Some(Command::InternalStartCmd)
        | Some(Command::InternalOpenAiChatTranslate { .. })
        | Some(Command::InternalGeminiClientTranslate { .. })
        | Some(Command::InternalDaemon { .. })
        | Some(Command::Statusline { .. })
        | Some(Command::Session { .. })
        | Some(Command::SessionHook { .. })
        | Some(Command::InternalWorker { .. }) => {}
        Some(Command::Run) | None => {
            if let Some(ref prompt) = args.prompt {
                push_prompt(&mut cmd, backend, prompt.clone());
            }
            if let Some(ref message) = args.message {
                // -m has no codex equivalent (codex's -m is --model, handled above).
                // Pass through to claude; drop for codex to avoid clobbering --model.
                if matches!(backend, Backend::Claude) {
                    cmd.push("-m".to_string());
                    cmd.push(message.clone());
                }
            }
            if args.continue_session && matches!(backend, Backend::Claude) {
                cmd.push("--continue".to_string());
            }
            if let Some(ref resume) = args.resume {
                match backend {
                    Backend::Claude => {
                        cmd.push("--resume".to_string());
                        if let Some(ref term) = resume {
                            cmd.push(term.clone());
                        }
                    }
                    // Codex's session selector was emitted before the command
                    // arm so generated built-in prompts follow it in argv.
                    Backend::Codex => {}
                    Backend::DeepSeek => {}
                }
            }
        }
    }

    cmd.extend(passthrough);

    let is_loop_cmd = matches!(&args.command, Some(Command::Loop { .. }));
    // Headless: Claude `-p`/`--print` (from clud or passthrough), `codex exec`,
    // DeepSeek's headless profile.
    let headless = match backend {
        Backend::Claude => cmd.iter().any(|a| a == "-p" || a == "--print"),
        Backend::Codex => codex_uses_exec,
        Backend::DeepSeek => has_noninteractive_prompt(args, backend),
    };
    let launch_mode = crate::backend::resolve_launch_mode(
        args.pty,
        args.subprocess,
        headless,
        crate::session::terminals_are_interactive(),
    );

    // Issue: subprocess-mode loops on claude went silent until the iteration
    // finished, because `claude -p` buffers its single final response. Inject
    // `--output-format stream-json --verbose` so claude emits one JSON event
    // per turn step, and let the runtime render those into progress lines.
    // PTY-mode loops already stream the live TUI; codex doesn't expose this
    // flag at all.
    //
    // The flags MUST be inserted BEFORE the prompt (`-p <prompt>`) so that
    // `command[-1]` remains the prompt — downstream tooling, dry-run JSON
    // consumers, and integration tests rely on that contract.
    let stream_json_progress =
        matches!(backend, Backend::Claude) && is_loop_cmd && launch_mode == LaunchMode::Subprocess;
    if stream_json_progress {
        // For Claude, `push_prompt` emits `-p` then the prompt body. Find that
        // `-p` and slot the stream-json flags in just before it. This keeps
        // any earlier args (yolo, --model, etc.) and the prompt anchored at
        // the tail of the command.
        if let Some(p_idx) = cmd.iter().position(|a| a == "-p") {
            cmd.splice(
                p_idx..p_idx,
                [
                    "--output-format".to_string(),
                    "stream-json".to_string(),
                    "--verbose".to_string(),
                ],
            );
        }
    }

    let mut plan = LaunchPlan {
        unsafe_mode: args.unsafe_mode,
        allow_plan_mode: args.allow_plan_mode,
        command: cmd,
        iterations,
        backend,
        routing_mode: target.routing_mode,
        model_provider: Some(target.model_provider),
        requested_harness: Some(target.requested_harness),
        effective_harness: Some(target.effective_harness),
        provider_source: Some(target.provider_source),
        harness_source: Some(target.harness_source),
        launch_mode,
        cwd: Some(cwd.to_string_lossy().to_string()),
        graphics: GraphicsConfig {
            mode: args.graphics,
            image_path: args.graphics_image.clone(),
        },
        repeat_schedule,
        task_summary,
        loop_markers,
        stream_json_progress,
        codex_model,
        model_selection,
        failover: args.failover.clone(),
        failover_allow_metered: args.failover_allow_metered,
        allowed_models: args.model_allowlist(),
        provider_only: args.provider_only.clone(),
        zdr: crate::openrouter_routing::effective_zdr(args, target.model_provider),
        no_data_collection: args.no_data_collection,
        redact_secrets,
        coauthor: crate::attribution::resolve_from_env(args.coauthor.as_deref()),
        pinned_from_previous_selection: args.model_pin_is_from_previous_selection(),
        route: None,
    };
    // #1855: the route is resolved once, here, from the plan it describes --
    // so every backend renderer and `--dry-run` read one decision instead of
    // re-deriving their own copy of it.
    plan.route = Some(crate::route_plan::resolve(
        &plan,
        &crate::route_plan::Ambient::from_process(),
    ));
    plan
}

/// Codex's session-flag PreToolUse matcher: every tool, as this repo's own
/// `.codex/hooks.json` registers it. A `Bash`-only matcher left every rule
/// for another tool dead under Codex, as it was under Claude before #1954.
const CODEX_HOOK_MATCHER: &str = "*";

fn codex_hook_hash(command: &str) -> String {
    crate::codex_cmd_scan_install::codex_hook_trust_hash(CODEX_HOOK_MATCHER, command, 600)
}

fn existing_codex_instructions(args: &Args, cwd: &Path) -> Option<String> {
    if let Some(value) = args.codex_config_overrides.iter().rev().find_map(|value| {
        let raw = value.strip_prefix("developer_instructions=")?;
        parse_codex_instruction_override(raw)
    }) {
        return Some(value);
    }
    let config_dir = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::home::user_home().map(|home| home.join(".codex")))?;
    let user_config = std::fs::read_to_string(config_dir.join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok());
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let root = git_root_from(&cwd);
    let trusted =
        crate::hook_health::codex_project_trusted_in_config(&root, &config_dir.join("config.toml"));
    if trusted {
        let mut directory = cwd.as_path();
        loop {
            let project_config = directory.join(".codex").join("config.toml");
            if let Some(instructions) = std::fs::read_to_string(project_config)
                .ok()
                .and_then(|text| text.parse::<toml::Value>().ok())
                .and_then(|config| {
                    config
                        .get("developer_instructions")?
                        .as_str()
                        .map(str::to_string)
                })
            {
                return Some(instructions);
            }
            if directory == root {
                break;
            }
            let Some(parent) = directory.parent() else {
                break;
            };
            directory = parent;
        }
    }
    user_config?
        .get("developer_instructions")?
        .as_str()
        .map(str::to_string)
}

/// Canonicalize `--model` for a Codex-provider / Claude-harness launch.
///
/// `None` when this is not that cross-route or neither model nor effort was
/// selected. Model-less effort pins the bridge's reviewed catalog default. A
/// parse failure remains bridge-owned and does not substitute another model.
fn codex_model_selection(
    args: &Args,
    target: ResolvedLaunchTarget,
    selection: Option<&provider_catalog::ResolvedModelSelection>,
) -> Option<String> {
    if target.routing_mode != crate::backend::RoutingMode::Direct
        || target.model_provider != ModelProvider::Codex
        || target.effective_harness != Backend::Claude
    {
        return None;
    }
    if let Some(selection) = selection {
        let mut value = selection.wire_model.clone().or_else(|| {
            selection.effort.map(|_| {
                let row = provider_catalog::reviewed_default_model(ModelProvider::Codex)
                    .expect("Codex must have a reviewed catalog default");
                crate::codex_runtime::wire_id(row.cli_id, row.wire_id)
            })
        })?;
        if let Some(effort) = selection.effort {
            value.push('@');
            value.push_str(effort.as_str());
        }
        return Some(value);
    }
    let requested = args.model.as_deref()?;
    ModelSpec::parse(requested).ok().map(|spec| spec.display())
}

/// Translate a normalized initial selection into the ID Claude Code must send
/// back to a discovery gateway. Provider wire IDs are not safe here:
/// an unrecognized `gpt-*` would look like an ordinary native Claude ID and
/// could be proxied to Anthropic.
fn gateway_model_selection(
    selection: Option<&provider_catalog::ResolvedModelSelection>,
) -> Option<String> {
    let selection = selection?;
    if selection.provider == ModelProvider::Claude {
        return selection.wire_model.clone();
    }
    let model = match selection.model.as_deref() {
        Some(model) => provider_catalog::model_by_cli_id(model),
        None => provider_catalog::reviewed_default_model(selection.provider),
    }?;
    model.discovery_id.map(str::to_string)
}

fn has_codex_project_doc_fallback_override(overrides: &[String]) -> bool {
    overrides
        .iter()
        .any(|value| codex_override_sets_key(value, CODEX_PROJECT_DOC_FALLBACK_KEY))
}

fn codex_override_sets_key(value: &str, key: &str) -> bool {
    value
        .trim_start()
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// #1847: the user already picked a Codex approvals reviewer, through clud's
/// `config_overrides` or backend passthrough (`-c`/`--config` with the key,
/// or Codex's `--approve-for-me` / `--not-so-yolo`), so clud's auto-review
/// default must not override it.
fn codex_reviewer_already_chosen(overrides: &[String], passthrough: &[String]) -> bool {
    let sets_reviewer = |value: &str| codex_override_sets_key(value, CODEX_APPROVALS_REVIEWER_KEY);
    overrides.iter().any(|value| sets_reviewer(value))
        || passthrough.iter().enumerate().any(|(index, arg)| {
            matches!(arg.as_str(), "--approve-for-me" | "--not-so-yolo")
                || arg.strip_prefix("--config=").is_some_and(sets_reviewer)
                || (matches!(arg.as_str(), "-c" | "--config")
                    && passthrough
                        .get(index + 1)
                        .is_some_and(|value| sets_reviewer(value)))
        })
}

fn codex_project_doc_fallback_config(cwd: &Path) -> Option<&'static str> {
    let repo_root = git_root_from(cwd);
    if repo_root.join("AGENTS.md").is_file() {
        return None;
    }
    if repo_root.join("CODEX.md").is_file() {
        return Some(CODEX_MD_PROJECT_DOC_FALLBACK_CONFIG);
    }
    if repo_root.join("CLAUDE.md").is_file() {
        return Some(CLAUDE_MD_PROJECT_DOC_FALLBACK_CONFIG);
    }
    None
}

/// Parse a `--repeat` duration string into seconds.
///
/// Accepted forms (issue #61): `30s`, `5m`, `1h`, `24h`. The unit is the
/// only recognized suffix; anything more elaborate (compound durations,
/// fractional units, ISO-8601 etc.) is intentionally out of scope.
///
/// Errors when:
/// - input is empty or whitespace-only
/// - integer part is missing (e.g. `s`)
/// - unit part is missing (e.g. `30`)
/// - integer is `0` (a zero interval would busy-loop)
/// - fractional values (`1.5h`) — the `.` makes integer parsing fail
/// - negative values (`-1h`) — the leading `-` is treated as the unit
///   start, which fails the empty-integer check
/// - unsupported units (`30d`, `1y`)
/// - the multiplied result would overflow `u64` seconds
pub(crate) fn parse_repeat_interval(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("duration cannot be empty".to_string());
    }
    let split_at = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| "duration must include a unit like s, m, or h".to_string())?;
    if split_at == 0 {
        return Err("duration must start with a positive integer".to_string());
    }
    let (num_part, unit_part) = trimmed.split_at(split_at);
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("invalid duration value: {num_part}"))?;
    if n == 0 {
        return Err("duration must be greater than zero".to_string());
    }
    let unit = unit_part.trim().to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        _ => return Err(format!("unsupported duration unit: {unit_part}")),
    };
    n.checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".to_string())
}

/// Decide whether `clud loop` flags imply that done-marker injection should
/// be disabled for this invocation. Issue #61.
///
/// Truth table (`repeat`, `no_done`, `done`):
/// - (Some, false, None)  → warn + disable (the `--repeat` implies `--no-done` case)
/// - (Some, true,  None)  → user already passed `--no-done`, no warning
/// - (Some, _,    Some)   → `--done <path>` overrides; no warning, contract on
/// - (None, _,    _)      → no `--repeat`, no warning emitted by this helper
///
/// Returns `Some(message)` to be printed to stderr when the warning should
/// fire, otherwise `None`.
pub fn repeat_implies_no_done_warning(
    repeat: Option<&str>,
    no_done: bool,
    done: Option<&str>,
) -> Option<&'static str> {
    if repeat.is_some() && !no_done && done.is_none() {
        Some(
            "[clud] warning: `--repeat` implies `--no-done`; \
             DONE marker injection/checking is disabled.",
        )
    } else {
        None
    }
}

/// Compute the wall-clock millis at which the next repeat run should fire,
/// given the millis at which the previous run *completed*. Issue #61.
///
/// This is the load-bearing "no-overlap" invariant: the next run is
/// scheduled **after the previous run completes**, not after the previous
/// run started. So a run that takes longer than the repeat interval simply
/// pushes the next run further into the future — runs serialize, never
/// overlap.
///
/// Saturates at `u64::MAX` rather than panicking, mirroring the daemon's
/// `saturating_mul` on the seconds→millis conversion.
pub fn next_run_at_millis(completed_at_millis: u64, interval_secs: u64) -> u64 {
    completed_at_millis.saturating_add(interval_secs.saturating_mul(1000))
}

pub fn summarize_task_name(input: &str, max_chars: usize) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() || normalized.chars().count() <= max_chars {
        return normalized;
    }
    let keep = max_chars.saturating_sub(3);
    let prefix: String = normalized.chars().take(keep).collect();
    format!("{prefix}...")
}

#[cfg(test)]
mod codex_hook_hash_tests {
    use super::codex_hook_hash;

    #[test]
    fn canonical_hook_hash_golden_vectors() {
        assert_eq!(
            codex_hook_hash("clud-cmd-scan"),
            "sha256:de4254a10d700c90ae0152fb911cf432a7e284fe4a94659452c1cab80d9f05b3"
        );
        assert_eq!(
            codex_hook_hash("/tmp/clud cmd-scan --event PreToolUse"),
            "sha256:f1e8926650feac924e61dd910ad96a3fc22466eedd26e245743f739989ca27b5"
        );
    }
}

#[cfg(test)]
mod land_bias_tests {
    use super::{resolve_land_bias, LandBias};

    #[test]
    fn preference_precedence_and_invalid_values() {
        assert_eq!(resolve_land_bias(false, None, None), LandBias::Merge);
        assert_eq!(resolve_land_bias(false, None, Some("pr")), LandBias::Pr);
        assert_eq!(
            resolve_land_bias(false, Some("merge"), Some("pr")),
            LandBias::Merge
        );
        assert_eq!(
            resolve_land_bias(false, Some("off"), Some("merge")),
            LandBias::Off
        );
        assert_eq!(
            resolve_land_bias(true, Some("merge"), Some("merge")),
            LandBias::Off
        );
        assert_eq!(
            resolve_land_bias(false, Some("invalid"), None),
            LandBias::Off
        );
    }
}

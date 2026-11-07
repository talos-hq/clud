//! Lightweight Claude/Codex `PreToolUse` hook parity diagnostics and repair.
//!
//! The launch path only warns. `--fix-hooks` is the explicit, repo-scoped
//! remediation path and is the only mode that writes deterministic config
//! changes or asks a backend agent to perform semantic hook translation.

mod codex_trust;
mod inspect;
mod prompts;
mod repairs;
mod types;
mod utils;
mod warnings;

use crate::args::{Args, Command as CliCommand};
use crate::backend::{Backend, ResolvedLaunchTarget};
use std::io::Write;

pub use codex_trust::{codex_project_key, codex_project_trusted, codex_project_trusted_in_config};
pub use inspect::{hook_home_dir, inspect_current, inspect_paths};
pub use repairs::plan_repairs;
pub use types::{
    CodexProjectTrust, DeterministicRepairError, FrontendHookSummary, HookConfigError,
    HookFrontend, HookHealthReport, NormalizedHook, RepairAction,
};

use inspect::inspect_current as inspect_current_impl;
use prompts::{run_backend_prompt, run_validation_followups};
pub(crate) use repairs::{apply_deterministic_repairs, deterministic_repair_actions};
use warnings::{print_dry_run_plan, print_report_warnings, uv_run_hook_warnings};

#[cfg(test)]
use codex_trust::{add_codex_project_trust, is_extended_key_for, migrate_codex_hooks_feature_flag};
#[cfg(test)]
use std::collections::BTreeSet;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::{Path, PathBuf};

pub(in crate::hook_health) const PRE_TOOL_USE: &str = "PreToolUse";
pub(in crate::hook_health) const CODEX_PRE_TOOL_USE_STATE: &str = "pre_tool_use";
pub(in crate::hook_health) const CATCH_ALL_MATCHER: &str = "*";
pub(in crate::hook_health) const FIX_HINT: &str = "Run `clud --fix-hooks`.";
pub(in crate::hook_health) const LEGACY_CODEX_HOOKS_FEATURE: &str = "codex_hooks";
pub(in crate::hook_health) const CURRENT_CODEX_HOOKS_FEATURE: &str = "hooks";

pub fn should_check_launch(args: &Args, launch_target: ResolvedLaunchTarget) -> bool {
    if launch_target.effective_harness != Backend::Codex {
        return false;
    }
    should_warn_uv_launch(args)
}

pub fn should_warn_uv_launch(args: &Args) -> bool {
    if args.fix_hooks || args.clean_worktrees {
        return false;
    }
    matches!(
        args.command,
        None | Some(CliCommand::Loop(_))
            | Some(CliCommand::Up { .. })
            | Some(CliCommand::Rebase)
            | Some(CliCommand::Fix { .. })
            | Some(CliCommand::Do { .. })
    )
}

/// The native Claude hook runner owns failures. Warn once before launch when
/// project hook commands can sync an unresolvable working-tree dependency.
pub fn emit_uv_launch_warnings() {
    let report = inspect_current_impl();
    let Some(message) = format_launch_warnings(&uv_run_hook_warnings(&report.claude)) else {
        return;
    };
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = handle.write_all(message.as_bytes());
    let _ = handle.flush();
}

/// Write the launch hook-health warnings to stderr. Issue #1346 (same root
/// cause as #515): every warning line is composed into a single buffer and
/// emitted with one locked, flushed `write_all`, rather than a sequence of
/// `eprintln!` calls. On Windows, `clud --codex` launches the backend through
/// a ConPTY that repaints the viewport when the child attaches; lines emitted
/// as separate writes just before takeover were being wiped. One flushed
/// block commits every warning up front so they survive.
pub fn emit_launch_warnings() {
    let report = inspect_current_impl();
    let Some(message) = format_launch_warnings(&report.warnings) else {
        return;
    };
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = handle.write_all(message.as_bytes());
    let _ = handle.flush();
}

/// Compose every launch warning as `[clud] warning: {warning}\n` into one
/// string, or `None` when there is nothing to report.
pub(crate) fn format_launch_warnings(warnings: &[String]) -> Option<String> {
    if warnings.is_empty() {
        return None;
    }
    let mut message = String::new();
    for warning in warnings {
        message.push_str("[clud] warning: ");
        message.push_str(warning);
        message.push('\n');
    }
    Some(message)
}

pub fn apply_default_repairs() -> Result<usize, DeterministicRepairError> {
    let report = inspect_current_impl();
    apply_deterministic_repairs(deterministic_repair_actions(&report))
}

pub fn run_fix_hooks(args: &Args, launch_target: ResolvedLaunchTarget) -> i32 {
    let mut report = inspect_current_impl();
    print_report_warnings(&report);

    if args.dry_run {
        print_dry_run_plan(&report);
        return 0;
    }

    if let Err(error) = apply_deterministic_repairs(deterministic_repair_actions(&report)) {
        eprintln!("[clud] error: failed to update {error}");
        return 1;
    }

    report = inspect_current_impl();
    let prompt_actions = plan_repairs(&report)
        .into_iter()
        .filter_map(|action| match action {
            RepairAction::BackendPrompt { prompt, .. } => Some(prompt),
            RepairAction::ValidationPrompt { prompt, .. } => Some(prompt),
            RepairAction::AddCodexProjectTrust { .. }
            | RepairAction::MigrateCodexHooksFeatureFlag { .. }
            | RepairAction::NormalizeCodexBatchHookExitCode { .. } => None,
        })
        .collect::<Vec<_>>();

    for (idx, prompt) in prompt_actions.iter().enumerate() {
        eprintln!(
            "[clud] running hook migration prompt {}/{}",
            idx + 1,
            prompt_actions.len()
        );
        let exit_code = match run_backend_prompt(args, launch_target, prompt.clone()) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("[clud] error: {error}");
                return 1;
            }
        };
        if exit_code != 0 {
            eprintln!("[clud] hook migration prompt exited with {exit_code}");
            return exit_code;
        }
        report = inspect_current_impl();
        let validation_exit = run_validation_followups(args, launch_target, &report);
        if validation_exit != 0 {
            return validation_exit;
        }
        print_report_warnings(&report);
    }

    let final_report = inspect_current_impl();
    if final_report.warnings.is_empty() {
        eprintln!("[clud] hook health check is clean");
    } else {
        eprintln!("[clud] remaining hook health warnings:");
        print_report_warnings(&final_report);
    }
    0
}

#[cfg(test)]
#[path = "../hook_health_tests.rs"]
pub(crate) mod tests;

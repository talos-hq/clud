//! PR-check gate: deny `git push` / `gh pr create` in a repo that declares
//! `pr_checks.commands` until `clud pr-check` has passed for the current
//! `HEAD^{tree}` (#2720).
//!
//! Settings, receipts and the runner live in `crate::pr_checks`. This module
//! recognizes the trigger and makes the decision; [`decide`] is pure over
//! injected [`Facts`] so every branch is unit-testable.
//!
//! **Fail open**, like the rest of `block_bad_cmd`: a command the shell
//! parser cannot read, a missing repo root or HEAD, or unreadable settings
//! allow the call. Not a security boundary.

use std::path::Path;

use super::block_bad_cmd_gate::statement_words;
use crate::pr_checks::{self, PrChecksConfig, SKIP_ENV};

/// Everything [`decide`] needs, gathered by [`evaluate`].
#[derive(Debug, Clone, Default)]
pub(super) struct Facts {
    /// `CLUD_SKIP_PR_CHECKS=1` in the command text or the environment.
    pub skip: bool,
    /// The repo's effective `pr_checks`; `None` means not gated.
    pub config: Option<PrChecksConfig>,
    /// A passing receipt exists for the current tree and command list.
    pub receipt_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Decision {
    Allow,
    /// Allowed only because of the skip switch; the caller logs it.
    Skipped,
    Deny(String),
}

/// The decision for a triggering command.
pub(super) fn decide(facts: &Facts) -> Decision {
    let Some(config) = facts.config.as_ref() else {
        return Decision::Allow;
    };
    if !config.enabled || facts.receipt_present {
        return Decision::Allow;
    }
    if facts.skip {
        return Decision::Skipped;
    }
    Decision::Deny(deny_reason(&config.commands))
}

fn deny_reason(commands: &[String]) -> String {
    let list = commands
        .iter()
        .map(|c| format!("`{c}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "PR checks have not passed for the current HEAD tree. Run `clud pr-check` first; it \
         runs this repo's pr_checks.commands ({list}) and records a receipt, then retry the \
         push. Opt out: prefix the command with `{SKIP_ENV}=1`, set `pr_checks.enabled: false` \
         in .clud/settings.local.json or ~/.clud/settings.json, or `CLUD_ALLOW_ALL_CMDS=1` \
         (#2720)."
    )
}

/// The decision for `command` run in `cwd`, or `None` when the command does
/// not push, or any fact cannot be gathered (fail open).
pub(super) fn evaluate(command: &str, cwd: &Path) -> Option<Decision> {
    if !triggers(command) {
        return None;
    }
    let skip = word_set(command, SKIP_ENV)
        || std::env::var(SKIP_ENV).ok().as_deref().map(str::trim) == Some("1");
    let (root, tree) = pr_checks::git_facts(cwd)?;
    let config = pr_checks::load_config(&root).ok()??;
    let receipt_present = crate::daemon::default_state_dir()
        .map(|dir| pr_checks::receipt_path(&dir, &root, &tree, &config.commands).is_file())
        .unwrap_or(false);
    Some(decide(&Facts {
        skip,
        config: Some(config),
        receipt_present,
    }))
}

/// Whether `NAME=1` appears in `command` as a whole word.
fn word_set(command: &str, name: &str) -> bool {
    let needle = format!("{name}=1");
    command.match_indices(&needle).any(|(at, _)| {
        let before = command[..at].chars().next_back();
        let after = command[at + needle.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether any statement runs `git push` (not `--dry-run`) or `gh pr create`.
pub(super) fn triggers(command: &str) -> bool {
    if !command.contains("push") && !command.contains("create") {
        return false;
    }
    let Ok(statements) = statement_words(command) else {
        return false;
    };
    statements.iter().any(|words| statement_triggers(words))
}

fn program_name(word: &str) -> &str {
    let bare = word.rsplit(['/', '\\']).next().unwrap_or(word);
    bare.strip_suffix(".exe").unwrap_or(bare)
}

fn statement_triggers(words: &[String]) -> bool {
    // Skip `VAR=value` prefixes and transparent wrappers (`env`, `ghd`).
    let mut rest = words;
    while let Some(first) = rest.first() {
        let is_assignment = first.contains('=') && !first.starts_with('-');
        if is_assignment || matches!(program_name(first), "env" | "ghd" | "command") {
            rest = &rest[1..];
        } else {
            break;
        }
    }
    let Some(program) = rest.first() else {
        return false;
    };
    let args = &rest[1..];
    match program_name(program) {
        "git" => {
            let sub = subcommand(args, &["-C", "-c", "--git-dir", "--work-tree"]);
            sub.is_some_and(|(sub, tail)| {
                sub == "push" && !tail.iter().any(|a| a == "--dry-run" || a == "-n")
            })
        }
        "gh" => {
            let sub = subcommand(args, &["-R", "--repo"]);
            sub.is_some_and(|(sub, tail)| {
                sub == "pr" && tail.first().is_some_and(|w| w == "create")
            })
        }
        _ => false,
    }
}

/// The first non-flag argument and what follows it, skipping flags and the
/// values of `value_flags`.
fn subcommand<'a>(args: &'a [String], value_flags: &[&str]) -> Option<(&'a str, &'a [String])> {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        if value_flags.contains(&word.as_str()) {
            i += 2;
        } else if word.starts_with('-') {
            i += 1;
        } else {
            return Some((word.as_str(), &args[i + 1..]));
        }
    }
    None
}

#[cfg(test)]
#[path = "block_bad_cmd_pr_checks_tests.rs"]
mod block_bad_cmd_pr_checks_tests;

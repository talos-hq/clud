//! Repo-declared PR checks and their receipts (#2720).
//!
//! A repo lists the commands an agent must pass before it pushes under
//! `pr_checks.commands` in `.clud/settings.json`. `clud pr-check` runs them
//! in order, stops at the first failure, and on full success writes a receipt
//! keyed by the repo root, `HEAD^{tree}` and the command list. The
//! `block_bad_cmd` PreToolUse rule (`block_bad_cmd_pr_checks.rs`) denies
//! `git push` / `gh pr create` in a gated repo until that receipt exists.
//!
//! Keyed by tree, not commit: amending a message or rebasing onto an
//! identical tree does not force a rerun. This is a fast local check, not an
//! attestation; hosted CI stays the merge gate (DD-190). See
//! `docs/architecture/hook-dispatch.md#pr-checks-gate-2720`.

use std::path::{Path, PathBuf};

use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Skip the gate for one command (`CLUD_SKIP_PR_CHECKS=1 git push`) or a
/// whole session (exported).
pub const SKIP_ENV: &str = "CLUD_SKIP_PR_CHECKS";
const RECEIPT_DIR: &str = "pr-checks";
const HASH_DOMAIN: &str = "clud-pr-checks-v1\0";

/// The effective `pr_checks` settings for one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrChecksConfig {
    /// Shell commands run in order by `clud pr-check`.
    pub commands: Vec<String>,
    /// `false` when a settings layer opts out with `pr_checks.enabled: false`.
    pub enabled: bool,
}

/// Resolve `pr_checks` from settings layers ordered highest priority first
/// (repo-local, repo, user). The first layer that declares `commands` owns
/// the list; the first that declares `enabled` owns the switch. `None` when
/// no layer declares a non-empty command list: the repo is not gated.
#[must_use]
pub fn config_from(layers: &[Value]) -> Option<PrChecksConfig> {
    let section = |doc: &Value| doc.get("pr_checks").cloned();
    let commands: Vec<String> = layers
        .iter()
        .filter_map(section)
        .find_map(|s| s.get("commands").and_then(Value::as_array).cloned())?
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    if commands.is_empty() {
        return None;
    }
    let enabled = layers
        .iter()
        .filter_map(section)
        .find_map(|s| s.get("enabled").and_then(Value::as_bool))
        .unwrap_or(true);
    Some(PrChecksConfig { commands, enabled })
}

/// Read the settings layers for `repo_root`. `Err` when a file exists but
/// cannot be read or parsed (callers fail open).
pub fn load_config(repo_root: &Path) -> Result<Option<PrChecksConfig>, String> {
    let clud_dir = repo_root.join(".clud");
    let mut paths = vec![
        clud_dir.join(crate::repo_clud_config::LOCAL_SETTINGS_FILE),
        clud_dir.join("settings.json"),
    ];
    if let Ok(home) = crate::clud_settings::home_dir_path() {
        paths.push(crate::clud_settings::settings_path_at(&home));
    }
    let mut layers = Vec::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if text.trim().is_empty() {
            continue;
        }
        let doc: Value = serde_json::from_str(&text)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        layers.push(doc);
    }
    Ok(config_from(&layers))
}

/// The repo root and `HEAD^{tree}` for `cwd`, or `None` outside a repo.
#[must_use]
pub fn git_facts(cwd: &Path) -> Option<(PathBuf, String)> {
    let root = crate::worktrees::run_git(cwd, &["rev-parse", "--show-toplevel"]).ok()?;
    let tree = crate::worktrees::run_git(cwd, &["rev-parse", "HEAD^{tree}"]).ok()?;
    let (root, tree) = (root.trim(), tree.trim());
    if root.is_empty() || tree.is_empty() {
        return None;
    }
    Some((PathBuf::from(root), tree.to_string()))
}

/// Where the receipt for this repo, tree and command list lives. Changing
/// the declared commands invalidates every earlier receipt.
#[must_use]
pub fn receipt_path(
    state_dir: &Path,
    repo_root: &Path,
    tree: &str,
    commands: &[String],
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN.as_bytes());
    hasher.update(repo_root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(tree.as_bytes());
    for command in commands {
        hasher.update(b"\0");
        hasher.update(command.as_bytes());
    }
    let digest = hasher.finalize();
    let hash: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    state_dir.join(RECEIPT_DIR).join(format!("{hash}.json"))
}

/// Run `commands` with `runner` in order, stopping at the first non-zero
/// exit; write the receipt only when every command exits 0. Returns the exit
/// code `clud pr-check` reports.
pub fn check_and_record(
    state_dir: &Path,
    repo_root: &Path,
    tree: &str,
    commands: &[String],
    mut runner: impl FnMut(&str) -> i32,
) -> i32 {
    for command in commands {
        eprintln!("[clud pr-check] $ {command}");
        let code = runner(command);
        if code != 0 {
            eprintln!("[clud pr-check] FAILED (exit {code}): {command}; no receipt written");
            return if code > 0 { code } else { 1 };
        }
    }
    let path = receipt_path(state_dir, repo_root, tree, commands);
    let body = serde_json::json!({
        "repo_root": repo_root.to_string_lossy(),
        "tree": tree,
        "commands": commands,
    });
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, body.to_string()));
    if let Err(error) = written {
        eprintln!("[clud pr-check] checks passed but the receipt could not be written: {error}");
        return 1;
    }
    eprintln!("[clud pr-check] all checks passed for tree {tree}; push is unblocked");
    0
}

fn run_shell(command: &str, repo_root: &Path) -> i32 {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Shell(command.to_string()),
        cwd: Some(repo_root.to_path_buf()),
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    if let Err(error) = process.start() {
        eprintln!("[clud pr-check] failed to start {command:?}: {error}");
        return 1;
    }
    process.wait(None).unwrap_or(1)
}

/// `clud pr-check`: run the current repo's declared checks.
#[must_use]
pub fn run() -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some((root, tree)) = git_facts(&cwd) else {
        eprintln!("[clud pr-check] not inside a git repository with a HEAD commit");
        return 2;
    };
    let config = match load_config(&root) {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!(
                "[clud pr-check] {} declares no pr_checks.commands; nothing to run",
                root.display()
            );
            return 0;
        }
        Err(error) => {
            eprintln!("[clud pr-check] {error}");
            return 2;
        }
    };
    let state_dir = match crate::daemon::default_state_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("[clud pr-check] {error}");
            return 2;
        }
    };
    check_and_record(&state_dir, &root, &tree, &config.commands, |command| {
        run_shell(command, &root)
    })
}

#[cfg(test)]
#[path = "pr_checks_tests.rs"]
mod pr_checks_tests;

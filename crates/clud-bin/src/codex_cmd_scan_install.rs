//! Issue #1972: install `clud-cmd-scan` as a global Codex `PreToolUse` hook.
//!
//! clud-launched Codex sessions already get the scanner through `-c
//! hooks.PreToolUse=...` (`command::builder`). A bare `codex` started in any
//! other repo did not, so its shell calls ran unguarded. On every Codex
//! global launch setup this pass makes sure `~/.codex/hooks.json` carries
//!
//! ```json
//! {"matcher": "*", "hooks": [{"type": "command", "command": "clud-cmd-scan"}]}
//! ```
//!
//! and that `~/.codex/config.toml` trusts that exact handler under
//! `[hooks.state."<hooks.json>:pre_tool_use:G:H"]`, which Codex requires
//! before it runs a hook from a file.
//!
//! Safety rules mirror `codex_hook_normalize` and share its lock
//! (`~/.clud/settings.lock`): a missing file is created; every other hook and
//! unknown field is preserved; an existing `clud-cmd-scan` handler (any
//! matcher) counts as installed and is never duplicated; malformed JSON or
//! TOML, or an unexpected shape, is logged and left untouched; an existing
//! trust entry is never overwritten; nothing is rewritten when nothing
//! changed. Opt out with `"hooks": {"codex_global_cmd_scan": false}` in
//! `~/.clud/settings.json`.

use std::io::{self, Write};
use std::path::Path;

use serde_json::{json, Map, Value};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::codex_hook_normalize::{acquire_lock, LOCK_FILE_NAME, SETTINGS_FILE_NAME};

/// The scanner's multicall name, as installed on PATH.
pub const SCAN_COMMAND: &str = "clud-cmd-scan";
/// `hooks.<key>` in `~/.clud/settings.json`; `false` disables this pass.
pub const OPT_OUT_KEY: &str = "codex_global_cmd_scan";
/// Codex's default handler timeout, part of the trust hash.
const DEFAULT_TIMEOUT: u64 = 600;
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// What one pass changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InstallOutcome {
    pub hooks_written: bool,
    pub trust_written: bool,
    pub opted_out: bool,
}

/// Codex's trust hash for one command handler: sha256 of the canonical JSON
/// of the handler's normalized group.
pub fn codex_hook_trust_hash(matcher: &str, command: &str, timeout: u64) -> String {
    use kernal_api::hash::Sha256Hasher as Sha256;
    let value = json!({
        "event_name": "pre_tool_use",
        "hooks": [{"async": false, "command": command, "timeout": timeout, "type": "command"}],
        "matcher": matcher,
    });
    format!("sha256:{:x}", Sha256::digest(value.to_string().as_bytes()))
}

/// Run one pass with explicit paths (`codex_dir` holds `hooks.json` and
/// `config.toml`). Never fails the launch for a malformed user file.
pub fn run_at<W: Write + ?Sized>(
    clud_dir: &Path,
    codex_dir: &Path,
    out: &mut W,
    verbose: bool,
) -> io::Result<InstallOutcome> {
    std::fs::create_dir_all(clud_dir)?;
    let settings_path = clud_dir.join(SETTINGS_FILE_NAME);
    if !settings_path.exists() {
        std::fs::write(&settings_path, "{}\n")?;
    }
    let _lock = acquire_lock(&clud_dir.join(LOCK_FILE_NAME))?;
    if opted_out(&settings_path) {
        return Ok(InstallOutcome {
            opted_out: true,
            ..InstallOutcome::default()
        });
    }
    let hooks_path = codex_dir.join("hooks.json");
    let Some((handler, hooks_written)) = ensure_hooks_entry(&hooks_path, out, verbose)? else {
        return Ok(InstallOutcome::default());
    };
    let trust_written = ensure_trust(codex_dir, &hooks_path, &handler, out, verbose)?;
    Ok(InstallOutcome {
        hooks_written,
        trust_written,
        opted_out: false,
    })
}

fn opted_out(settings_path: &Path) -> bool {
    std::fs::read(settings_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|doc| doc.get("hooks")?.get(OPT_OUT_KEY)?.as_bool())
        == Some(false)
}

/// Where the scan handler sits and what Codex hashes for it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Handler {
    group: usize,
    index: usize,
    matcher: String,
    timeout: u64,
}

fn log<W: Write + ?Sized>(out: &mut W, verbose: bool, message: std::fmt::Arguments<'_>) {
    if verbose {
        let _ = writeln!(out, "[clud] codex cmd-scan hook: {message}");
    }
}

/// Ensure the entry exists. `None` means the file was left alone (unreadable,
/// malformed, or an unexpected shape).
fn ensure_hooks_entry<W: Write + ?Sized>(
    hooks_path: &Path,
    out: &mut W,
    verbose: bool,
) -> io::Result<Option<(Handler, bool)>> {
    let mut doc = match std::fs::read_to_string(hooks_path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(doc) => doc,
            Err(e) => {
                log(
                    out,
                    verbose,
                    format_args!("malformed JSON in {}: {e}", hooks_path.display()),
                );
                return Ok(None);
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => {
            log(
                out,
                verbose,
                format_args!("cannot read {}: {e}", hooks_path.display()),
            );
            return Ok(None);
        }
    };
    let Some(groups) = pre_tool_use_groups(&mut doc) else {
        log(
            out,
            verbose,
            format_args!(
                "unexpected shape in {}; left untouched",
                hooks_path.display()
            ),
        );
        return Ok(None);
    };
    if let Some(handler) = find_handler(groups) {
        return Ok(Some((handler, false)));
    }
    groups.push(json!({
        "matcher": "*",
        "hooks": [{"type": "command", "command": SCAN_COMMAND}],
    }));
    let handler = Handler {
        group: groups.len() - 1,
        index: 0,
        matcher: "*".into(),
        timeout: DEFAULT_TIMEOUT,
    };
    let mut text = serde_json::to_string_pretty(&doc)
        .map_err(|e| io::Error::other(format!("serialize hooks.json: {e}")))?;
    text.push('\n');
    if let Some(parent) = hooks_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(hooks_path, text)?;
    let _ = writeln!(
        out,
        "{GREEN}[clud] installed clud-cmd-scan PreToolUse hook in {}{RESET}",
        hooks_path.display()
    );
    Ok(Some((handler, true)))
}

/// `doc.hooks.PreToolUse`, created when absent; `None` if any level has the
/// wrong type.
fn pre_tool_use_groups(doc: &mut Value) -> Option<&mut Vec<Value>> {
    let hooks = doc
        .as_object_mut()?
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    hooks
        .as_object_mut()?
        .entry("PreToolUse")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
}

fn find_handler(groups: &[Value]) -> Option<Handler> {
    groups.iter().enumerate().find_map(|(group, entry)| {
        let handlers = entry.get("hooks")?.as_array()?;
        let index = handlers.iter().position(|h| {
            h.get("command").and_then(Value::as_str).map(str::trim) == Some(SCAN_COMMAND)
        })?;
        Some(Handler {
            group,
            index,
            matcher: entry
                .get("matcher")
                .and_then(Value::as_str)
                .unwrap_or("*")
                .to_string(),
            timeout: handlers[index]
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT),
        })
    })
}

/// Seed `[hooks.state."<key>"] trusted_hash` for the handler unless any entry
/// for that key already exists.
fn ensure_trust<W: Write + ?Sized>(
    codex_dir: &Path,
    hooks_path: &Path,
    handler: &Handler,
    out: &mut W,
    verbose: bool,
) -> io::Result<bool> {
    let config_path = codex_dir.join("config.toml");
    let text = match std::fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            log(
                out,
                verbose,
                format_args!("cannot read {}: {e}", config_path.display()),
            );
            return Ok(false);
        }
    };
    let mut doc = match text.parse::<DocumentMut>() {
        Ok(doc) => doc,
        Err(e) => {
            log(
                out,
                verbose,
                format_args!("malformed TOML in {}: {e}", config_path.display()),
            );
            return Ok(false);
        }
    };
    let key = format!(
        "{}:pre_tool_use:{}:{}",
        hooks_path.to_string_lossy(),
        handler.group,
        handler.index
    );
    let Some(state) = state_table(&mut doc) else {
        log(
            out,
            verbose,
            format_args!(
                "unexpected hooks.state in {}; left untouched",
                config_path.display()
            ),
        );
        return Ok(false);
    };
    if state.contains_key(&key) {
        return Ok(false);
    }
    let mut entry = Table::new();
    entry["trusted_hash"] = value(codex_hook_trust_hash(
        &handler.matcher,
        SCAN_COMMAND,
        handler.timeout,
    ));
    state.insert(&key, Item::Table(entry));
    std::fs::write(&config_path, doc.to_string())?;
    Ok(true)
}

fn state_table(doc: &mut DocumentMut) -> Option<&mut Table> {
    let hooks = doc
        .entry("hooks")
        .or_insert_with(|| Item::Table(implicit_table()))
        .as_table_mut()?;
    hooks
        .entry("state")
        .or_insert_with(|| Item::Table(implicit_table()))
        .as_table_mut()
}

fn implicit_table() -> Table {
    let mut table = Table::new();
    table.set_implicit(true);
    table
}

#[cfg(test)]
#[path = "codex_cmd_scan_install_tests.rs"]
mod tests;

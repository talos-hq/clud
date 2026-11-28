//! Issue #234: normalize user/global Codex `hooks.json` timeouts of exactly
//! `5` to `30`.
//!
//! Codex hook handlers with `"timeout": 5` time out during normal
//! `clud`-driven interactive sessions on slower hosts (Windows in
//! particular). Codex's own default is much higher than 30s, so a missing
//! `timeout` is fine — only the explicit `5` is the trap.
//!
//! Behavior during Codex global launch setup:
//!
//! 1. Ensure `~/.clud/` and `~/.clud/settings.json` exist.
//! 2. Acquire `~/.clud/settings.lock` (cross-platform advisory lock, same
//!    `fs4` pattern used by `session_registry`).
//! 3. Read `~/.codex/hooks.json`. Missing file or malformed JSON: log a
//!    one-line warning in verbose mode and return.
//! 4. Walk the parsed value, find every object with an integer
//!    `"timeout": 5`, and rewrite it to `30`. Values >5, non-integer,
//!    or missing `timeout` are left alone.
//! 5. If at least one value changed, re-serialize and overwrite the file,
//!    then emit a green `[clud] updated Codex hook timeout: 5s -> 30s`
//!    line to stderr.
//!
//! The pass is idempotent and not a one-time migration: if a user, tool,
//! or Codex update later sets a global hook timeout back to `5`, the next
//! eligible launch upgrades it again. The repo-local `.codex/hooks.json`
//! is intentionally untouched — this only normalizes the user/global file.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use crate::file_lock::FileExt;
use serde_json::Value;

const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// Filename for the cross-process advisory lock that gates the
/// read-modify-write of `~/.codex/hooks.json`.
pub const LOCK_FILE_NAME: &str = "settings.lock";

/// Filename for the `~/.clud` settings JSON file that this feature
/// ensures exists (per issue #234 acceptance criteria).
pub const SETTINGS_FILE_NAME: &str = "settings.json";

/// Codex's documented default timeout is well above 30, so any handler
/// that has no `timeout` field at all is already fine.
pub const NORMALIZE_FROM: u64 = 5;
pub const NORMALIZE_TO: u64 = 30;

/// Result of one normalization pass. Mostly useful for unit tests; the
/// production caller only needs to know whether anything changed (so the
/// green status line is emitted exactly when there's something to report).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NormalizeOutcome {
    pub changed: u32,
}

impl NormalizeOutcome {
    pub fn changed(self) -> u32 {
        self.changed
    }
}

/// Compatibility entry point that resolves the user's home directory, runs the
/// normalization pass, and prints the green status line to stderr
/// on any change. All failures are non-fatal — a missing home dir, an
/// unwritable `~/.clud`, or any I/O hiccup must never block a launch.
pub fn run_global_normalization(verbose: bool) {
    let Some(home) = crate::home::user_home() else {
        if verbose {
            eprintln!("[clud] codex hook normalize: no home dir");
        }
        return;
    };
    let clud_dir = home.join(".clud");
    let hooks_path = home.join(".codex").join("hooks.json");
    let mut stderr = io::stderr().lock();
    if let Err(e) = run_at(&clud_dir, &hooks_path, &mut stderr, verbose) {
        if verbose {
            let _ = writeln!(stderr, "[clud] codex hook normalize: {e}");
        }
    }
}

/// Run one normalization pass with explicit paths. Used by both
/// `run_global_normalization` and the unit tests; `clud_dir` is the dir
/// that holds the lock file plus the auto-created `settings.json`, and
/// `hooks_path` is the absolute path to the Codex hooks JSON to inspect.
pub fn run_at<W: Write + ?Sized>(
    clud_dir: &Path,
    hooks_path: &Path,
    out: &mut W,
    verbose: bool,
) -> io::Result<NormalizeOutcome> {
    std::fs::create_dir_all(clud_dir)?;
    let settings_path = clud_dir.join(SETTINGS_FILE_NAME);
    if !settings_path.exists() {
        // The issue requires us to create this file when missing so the
        // settings dir mirrors what current installs already carry on
        // disk. We seed an empty JSON object so a future tool that opens
        // and parses it gets a valid document.
        std::fs::write(&settings_path, "{}\n")?;
    }
    let lock_path = clud_dir.join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;

    if !hooks_path.exists() {
        return Ok(NormalizeOutcome::default());
    }

    let original = match std::fs::read_to_string(hooks_path) {
        Ok(text) => text,
        Err(e) => {
            if verbose {
                let _ = writeln!(
                    out,
                    "[clud] codex hook normalize: cannot read {}: {e}",
                    hooks_path.display()
                );
            }
            return Ok(NormalizeOutcome::default());
        }
    };

    let mut json: Value = match serde_json::from_str(&original) {
        Ok(v) => v,
        Err(e) => {
            if verbose {
                let _ = writeln!(
                    out,
                    "[clud] codex hook normalize: malformed JSON in {}: {e}",
                    hooks_path.display()
                );
            }
            return Ok(NormalizeOutcome::default());
        }
    };

    let changed = normalize_value(&mut json);
    if changed == 0 {
        return Ok(NormalizeOutcome { changed: 0 });
    }

    let mut rewritten = serde_json::to_string_pretty(&json)
        .map_err(|e| io::Error::other(format!("serialize hooks.json: {e}")))?;
    if !rewritten.ends_with('\n') {
        rewritten.push('\n');
    }
    std::fs::write(hooks_path, rewritten)?;

    let plural = if changed == 1 { "" } else { "s" };
    let _ = writeln!(
        out,
        "{GREEN}[clud] updated Codex hook timeout: 5s -> 30s ({changed} hook{plural} in {}){RESET}",
        hooks_path.display()
    );

    Ok(NormalizeOutcome { changed })
}

/// Recursively walk `value`, rewriting every integer `"timeout": 5` to
/// `30`. Returns the number of edits made.
///
/// The walk inspects every object key, but only mutates `timeout` fields
/// whose current value is exactly the integer `5` — non-integer values,
/// floats (e.g. `5.0`), and any value `> 5` are left alone, matching the
/// issue's acceptance criteria.
pub fn normalize_value(value: &mut Value) -> u32 {
    let mut count = 0u32;
    walk(value, &mut count);
    count
}

fn walk(value: &mut Value, count: &mut u32) {
    match value {
        Value::Object(map) => {
            if let Some(timeout) = map.get_mut("timeout") {
                if is_exact_int(timeout, NORMALIZE_FROM) {
                    *timeout = Value::from(NORMALIZE_TO);
                    *count += 1;
                }
            }
            for (_, v) in map.iter_mut() {
                walk(v, count);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                walk(v, count);
            }
        }
        _ => {}
    }
}

/// `true` iff `value` is a JSON number that's exactly the unsigned
/// integer `expected`. Rejects floats like `5.0`, negative numbers, and
/// any non-number variant.
///
/// `Value::as_u64` returns `Some(n)` only when the number is a
/// non-negative *integer* that fits in `u64` — exactly the predicate we
/// want here. Float values (including `5.0` written literally in JSON)
/// return `None` and are therefore left unchanged.
fn is_exact_int(value: &Value, expected: u64) -> bool {
    matches!(value, Value::Number(_)) && value.as_u64() == Some(expected)
}

pub(crate) fn acquire_lock(path: &Path) -> io::Result<LockGuard> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    FileExt::lock_exclusive(&file)
        .map_err(|e| io::Error::other(format!("lock_exclusive {}: {e}", path.display())))?;
    Ok(LockGuard { _file: file })
}

/// RAII guard for the cross-process `~/.clud/settings.lock` lock. The OS
/// releases the advisory lock when the file handle drops or the process
/// exits, so Drop intentionally does nothing; we just keep `File` alive.
pub(crate) struct LockGuard {
    _file: File,
}

#[cfg(test)]
#[path = "codex_hook_normalize_tests.rs"]
mod tests;

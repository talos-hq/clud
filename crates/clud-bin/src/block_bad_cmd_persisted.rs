//! Persisted-output read bound (#2233).
//!
//! When one tool result is too large, Claude Code writes it to
//! `~/.claude/projects/<project>/<session>/tool-results/<id>.txt` and leaves a
//! ~2 KB preview in the transcript. A `Read` of that file with no range
//! re-injects the whole artifact, and every later request in the session
//! re-sends it. Over the ten days probed to 2026-11-05 this was the single
//! largest exposure cell on most days: 35,988 B read once and re-sent 62 times
//! is 2.2 MB (2026-11-04 and 11-05), 34,277 B re-sent 1,079 times is 37 MB
//! (2026-11-02), and the same shape appears on 2026-10-27, 10-30, 11-01 and
//! 11-02 in five different projects. Nothing in the dispatcher refused it: the
//! whole-file `cat` budget, the ranged-read budget and the unchanged-re-read
//! guard all pass a *first* whole-file `Read` of a spill file.
//!
//! This guard refuses that first whole read once with the bounded
//! alternatives. A read that names `offset`/`limit` (or any backend's range
//! spelling) is a partial read and always passes, and a retry of the exact
//! same whole read passes, so a session can never be wedged. **Fail open**: no
//! session id, a missing or unreadable file, a non-`tool-results` path, or any
//! state read or write failure allows the call. This is a nudge, not a
//! security boundary, and interactive verbosity is unchanged (the refused
//! call was a re-injection, not new output).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Files smaller than this are cheap to re-send; a spill file is by
/// definition bigger than the harness preview, and this keeps the guard off
/// hand-written notes that happen to live beside an artifact.
pub(super) const MIN_GUARDED_BYTES: u64 = 8 * 1024;
/// `0` (or `off`) disables the guard for the whole session.
pub(super) const DISABLE_ENV: &str = "CLUD_PERSISTED_READ_GUARD";
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "persisted-read-guard";
const HASH_DOMAIN: &str = "clud-persisted-read-guard-v1\0";
/// A refusal older than this is forgotten, so the file can be read whole
/// again the next day.
const MAX_AGE_SECS: u64 = 24 * 60 * 60;
const MAX_ENTRIES: usize = 64;
/// Any of these in `tool_input` makes the read a range, not the whole file.
const RANGE_KEYS: &[&str] = &[
    "offset",
    "limit",
    "start_line",
    "end_line",
    "line_offset",
    "max_lines",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(super) struct Ledger {
    #[serde(default)]
    pub refused: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Not a persisted output, a ranged read, or too small to matter.
    Allow,
    /// First whole read of this artifact: record the refusal and deny.
    Deny,
    /// A retry of an already refused read: allow.
    Retry,
}

/// `Read` as Claude, Codex and the shell-shaped backends spell it.
pub(super) fn applies_to(tool_name: &str) -> bool {
    matches!(
        super::block_bad_cmd_gate::tool_basename(tool_name).as_str(),
        "read" | "read_file" | "readfile"
    )
}

/// Whether `CLUD_PERSISTED_READ_GUARD` turns the guard off.
pub(super) fn disabled_from(env: Option<&str>) -> bool {
    env.is_some_and(|value| {
        let value = value.trim();
        value.eq_ignore_ascii_case("0") || value.eq_ignore_ascii_case("off")
    })
}

fn disabled() -> bool {
    disabled_from(std::env::var(DISABLE_ENV).ok().as_deref())
}

fn path_from(tool_input: Option<&Value>) -> Option<PathBuf> {
    let input = tool_input?;
    ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// A read that asks for a slice rather than the file.
pub(super) fn is_ranged(tool_input: Option<&Value>) -> bool {
    let Some(input) = tool_input.and_then(Value::as_object) else {
        return false;
    };
    RANGE_KEYS
        .iter()
        .any(|key| input.get(*key).is_some_and(|value| !value.is_null()))
}

/// `~/.claude/projects/<project>/<session>/tool-results/<id>.*`.
///
/// The parent directory must be `tool-results` *and* an ancestor must be
/// `.claude`, so a repository that happens to keep its own `tool-results/`
/// directory is never touched.
pub(super) fn is_persisted_output(path: &Path) -> bool {
    let in_tool_results = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "tool-results");
    in_tool_results
        && path
            .ancestors()
            .any(|ancestor| ancestor.file_name().is_some_and(|name| name == ".claude"))
}

fn guarded_len(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() < MIN_GUARDED_BYTES {
        return None;
    }
    Some(meta.len())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn signature(path: &Path) -> String {
    let digest =
        Sha256::digest(format!("{HASH_DOMAIN}path\0{}", path.to_string_lossy()).as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn session_file(state_dir: &Path, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{HASH_DOMAIN}session\0{session_id}").as_bytes());
    let hash: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    state_dir.join(DIR_NAME).join(format!("{hash}.json"))
}

/// The decision over injected facts: whether this call is a refused read's
/// retry, and the ledger to persist.
pub(super) fn decide(ledger: &mut Ledger, key: &str, now: u64) -> Verdict {
    let mut verdict = Verdict::Deny;
    if let Some(at) = ledger.refused.get(key) {
        if now.saturating_sub(*at) <= MAX_AGE_SECS {
            verdict = Verdict::Retry;
        }
    } else {
        ledger.refused.insert(key.to_string(), now);
    }
    ledger
        .refused
        .retain(|_, at| now.saturating_sub(*at) <= MAX_AGE_SECS);
    while ledger.refused.len() > MAX_ENTRIES {
        let Some(oldest) = ledger
            .refused
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        ledger.refused.remove(&oldest);
    }
    verdict
}

/// Record the refusal in `state_dir` and return the verdict. Every failure
/// allows: a session must never be blocked because clud could not write a
/// state file.
pub(super) fn check_at(state_dir: &Path, session_id: &str, path: &Path, now: u64) -> Verdict {
    if session_id.is_empty() {
        return Verdict::Allow;
    }
    let key = signature(path);
    let state_path = session_file(state_dir, session_id);
    let existing = std::fs::read(&state_path).ok();
    let mut ledger = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Ledger>(bytes).ok())
        .unwrap_or_default();
    let verdict = decide(&mut ledger, &key, now);
    if verdict == Verdict::Allow {
        return Verdict::Allow;
    }
    let Ok(bytes) = serde_json::to_vec(&ledger) else {
        return Verdict::Allow;
    };
    if super::block_bad_cmd_repeat::write_fast(&state_path, &bytes, existing.is_none()).is_err() {
        return Verdict::Allow;
    }
    if existing.is_none() {
        if let Some(dir) = state_path.parent() {
            super::block_bad_cmd_repeat::prune(dir, SystemTime::now());
        }
    }
    verdict
}

/// Hook entry: the denial reason for this read, or `None` to allow.
pub(super) fn reason(
    tool_name: &str,
    tool_input: Option<&Value>,
    session_id: Option<&str>,
) -> Option<String> {
    if disabled() || !applies_to(tool_name) || is_ranged(tool_input) {
        return None;
    }
    let session_id = session_id?;
    let path = path_from(tool_input)?;
    if !is_persisted_output(&path) {
        return None;
    }
    let bytes = guarded_len(&path)?;
    let state_dir = crate::daemon::default_state_dir().ok()?;
    match check_at(&state_dir, session_id, &path, now_secs()) {
        Verdict::Deny => Some(deny_message(&path, bytes)),
        Verdict::Allow | Verdict::Retry => None,
    }
}

pub(super) fn deny_message(path: &Path, bytes: u64) -> String {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let kb = bytes / 1024;
    let path_text = path.display();
    format!(
        "[clud persisted-read-guard] `{name}` is a persisted tool output ({kb} KB) whose preview \
         is already in this transcript; reading it whole re-sends all {kb} KB on every later \
         request in this session. Read only the ranges you need: `Read` with `offset` and `limit` \
         (for example `limit: 150`), `Grep` for the line you are after, or \
         `clud tool run code/slice.py {path_text} --lines A-B` (caps the text at 8 KiB and its \
         footer names the next line). A session launched with `{DISABLE_ENV}=0` reads spills \
         whole again."
    )
}

#[path = "block_bad_cmd_persisted_tests.rs"]
#[cfg(test)]
mod block_bad_cmd_persisted_tests;

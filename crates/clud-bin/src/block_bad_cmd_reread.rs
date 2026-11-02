//! Unchanged-re-read guard (#2233).
//!
//! A `Read` of a file whose bytes have not changed since the same read
//! already happened in this session returns information the model already
//! holds, and every later request in the session pays for it again. Over the
//! 24 h probed on 2026-10-31 that was 308 of 1,035 reads — 30 % — and
//! 1,029,780 bytes re-supplied, led by agent slices of one 2,142-line file
//! read 23 times.
//!
//! This guard denies such a read with a short message that names the bounded
//! alternatives. It keys on the call's canonical input (so a different
//! `offset`/`limit` is a different read) and on the file's `(len, mtime)`
//! fingerprint: an edited file has a new fingerprint and is allowed again, so
//! the read-after-edit verification loop is untouched.
//!
//! **Fail open.** Like the repeat guard this is a friction-reducing nudge, not
//! a security boundary: a missing session id, an unreadable home, a file that
//! cannot be stat-ed, or any state read or write failure allows the call.
//! Interactive verbosity is unchanged — the denied call is a repeat, not new
//! output.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Files at least this big are guarded: below it a re-read is a few hundred
/// bytes and refusing it would cost more in friction than it saves.
pub(super) const MIN_GUARDED_BYTES: u64 = 8 * 1024;
/// `0` (or `off`) disables the guard for the whole session.
pub(super) const DISABLE_ENV: &str = "CLUD_REREAD_GUARD";
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "reread-guard";
/// Most entries kept per session; the oldest `at` goes first.
pub(super) const MAX_ENTRIES: usize = 256;
const HASH_DOMAIN: &str = "clud-reread-guard-v1\0";

/// What one Read call asked for, plus the fingerprint of the file now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Request {
    /// The call's canonical input, hashed; a different range is a new key.
    pub key: String,
    /// Absolute path as the harness spelled it.
    pub path: PathBuf,
    /// `(len, mtime_secs, mtime_nanos)`, or `None` when the file is absent.
    pub stamp: Option<Stamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    pub len: u64,
    pub secs: u64,
    pub nanos: u32,
}

/// One recorded read: what the file looked like, and when it was read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    pub len: u64,
    pub secs: u64,
    pub nanos: u32,
    /// Unix seconds of the read, for the "read it N ago" text.
    pub at: u64,
    /// File name only: the message is cheaper and the model knows the path.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(super) struct Ledger {
    #[serde(default)]
    pub entries: std::collections::BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    /// Allowed, but recorded: this is the first read of this fingerprint.
    Record,
    Deny {
        ago_secs: u64,
        kb: u64,
    },
}

/// `Read` as Claude, Codex and the shell-shaped backends spell it.
pub(super) fn applies_to(tool_name: &str) -> bool {
    matches!(
        super::block_bad_cmd_gate::tool_basename(tool_name).as_str(),
        "read" | "read_file" | "readfile"
    )
}

/// Whether `CLUD_REREAD_GUARD` turns the guard off.
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

fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta.modified().ok()?;
    let since = mtime.duration_since(UNIX_EPOCH).ok()?;
    Some(Stamp {
        len: meta.len(),
        secs: since.as_secs(),
        nanos: since.subsec_nanos(),
    })
}

/// The request this call represents, or `None` when there is nothing to guard.
pub(super) fn request_for(tool_name: &str, tool_input: Option<&Value>) -> Option<Request> {
    let path = path_from(tool_input)?;
    Some(Request {
        key: super::block_bad_cmd_repeat::call_key(tool_name, tool_input),
        stamp: stamp_of(&path),
        path,
    })
}

/// The decision table over injected facts: the verdict and the entry to keep.
///
/// `now` is Unix seconds. A different fingerprint is the file having changed,
/// so it always allows and re-records.
pub(super) fn decide(previous: Option<&Entry>, current: &Entry) -> Verdict {
    let Some(previous) = previous else {
        return Verdict::Record;
    };
    if previous.len != current.len
        || previous.secs != current.secs
        || previous.nanos != current.nanos
    {
        return Verdict::Record;
    }
    Verdict::Deny {
        ago_secs: current.at.saturating_sub(previous.at),
        kb: current.len / 1024,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn session_file(state_dir: &Path, session_id: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{HASH_DOMAIN}session\0{session_id}").as_bytes());
    let hash: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    state_dir.join(DIR_NAME).join(format!("{hash}.json"))
}

fn trim_entries(ledger: &mut Ledger) {
    while ledger.entries.len() > MAX_ENTRIES {
        let Some(oldest) = ledger
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.at)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        ledger.entries.remove(&oldest);
    }
}

/// Record this read in `state_dir` and return the verdict. Every failure
/// allows.
pub(super) fn check_at(state_dir: &Path, session_id: &str, request: &Request, now: u64) -> Verdict {
    let Some(stamp) = request.stamp else {
        return Verdict::Allow;
    };
    if stamp.len < MIN_GUARDED_BYTES || session_id.is_empty() {
        return Verdict::Allow;
    }
    let current = Entry {
        len: stamp.len,
        secs: stamp.secs,
        nanos: stamp.nanos,
        at: now,
        name: request
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
    };
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read(&path).ok();
    let mut ledger = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Ledger>(bytes).ok())
        .unwrap_or_default();
    let verdict = decide(ledger.entries.get(&request.key), &current);
    ledger.entries.insert(request.key.clone(), current);
    trim_entries(&mut ledger);
    let Ok(bytes) = serde_json::to_vec(&ledger) else {
        return Verdict::Allow;
    };
    if super::block_bad_cmd_repeat::write_fast(&path, &bytes, existing.is_none()).is_err() {
        return Verdict::Allow;
    }
    if existing.is_none() {
        if let Some(dir) = path.parent() {
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
    if disabled() || !applies_to(tool_name) {
        return None;
    }
    let session_id = session_id?;
    let read = request_for(tool_name, tool_input)?;
    let state_dir = crate::daemon::default_state_dir().ok()?;
    match check_at(&state_dir, session_id, &read, now_secs()) {
        Verdict::Deny { ago_secs, kb } => Some(deny_message(&read, ago_secs, kb)),
        Verdict::Allow | Verdict::Record => None,
    }
}

fn ago_text(ago_secs: u64) -> String {
    match ago_secs {
        0..=9 => "just now".to_string(),
        10..=59 => format!("{ago_secs}s"),
        60..=3599 => format!("{}min", ago_secs / 60),
        3600..=86_399 => format!("{}h", ago_secs / 3600),
        _ => format!("{}d", ago_secs / 86_400),
    }
}

pub(super) fn deny_message(request: &Request, ago_secs: u64, kb: u64) -> String {
    let name = request
        .path
        .file_name()
        .map_or_else(String::new, |file| file.to_string_lossy().into_owned());
    format!(
        "[clud reread-guard] {name} ({kb} kB) is byte-identical to the copy you read {} ago — this \
         read returns nothing new and would be re-sent on every later request in this session. Work \
         from the copy you have. If you need a different part, pass `offset`+`limit` (one range, \
         <=200 lines); to map an unfamiliar file first, `clud tool run code/outline.py <path>`. An \
         edit changes the file and this is allowed again. Off for this session: \
         `{DISABLE_ENV}=0`.",
        ago_text(ago_secs)
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_reread_tests.rs"]
mod tests;

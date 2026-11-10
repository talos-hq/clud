//! Unchanged-re-read guard (#2233).
//!
//! A `Read` of a file whose bytes have not changed since the same read
//! already happened in this session returns information the model already
//! holds, and every later request in the session pays for it again. Over the
//! 24 h probed on 2026-10-31 that was 308 of 1,035 reads — 30 % — and
//! 1,029,780 bytes re-supplied, led by agent slices of one 2,142-line file
//! read 23 times.
//!
//! The first version of this guard keyed the call on its exact
//! `offset`/`limit`, so it only saw byte-identical repeats. The habit the
//! transcripts actually show is **overlapping windows**: the same file read
//! again with a window that starts a little earlier or later, which is a
//! different key and therefore was never refused. Measured on 24 h of
//! transcripts (2026-11-10) that is 73 fully-covered reads over 700 kB, and
//! the live ledgers carried the same shape — 67 files recorded with several
//! entries of one unchanged fingerprint, e.g. one 551 kB `README.md` read
//! twice, and `schemas.py` read 8 times in one session through windows that
//! overlapped almost completely.
//!
//! So the ledger now keeps, per `(session, path, fingerprint)`, the **line
//! intervals already brought into the conversation**. A read whose interval is
//! contained in one of them returns only lines the model already has and is
//! refused. Because line numbering is what the harness uses, over-claiming the
//! end of an interval can only cover lines past end-of-file, which carry no
//! content — the guard therefore never claims to have shown text it did not.
//!
//! Refusals are budgeted per file ([`MAX_REFUSALS`]): past it a covered read is
//! allowed again and re-stamped. Compaction really can drop earlier content
//! from the conversation, and a permanent wall would leave that reader with no
//! escape except a session-wide kill switch; two refusals are enough to tell a
//! habitual re-read from a genuine restore. A changed fingerprint always
//! starts over, so read-after-edit verification is untouched.
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
/// Most files kept per session; the oldest `at` goes first.
pub(super) const MAX_ENTRIES: usize = 256;
/// Refusals of covered reads of one unchanged file before the read passes.
/// A compaction can drop content from the conversation, and the reader must
/// have an escape that is not a session-wide switch.
pub(super) const MAX_REFUSALS: u32 = 2;
/// Most intervals kept per file; the narrowest goes first, which can only
/// widen what is allowed.
pub(super) const MAX_SPANS: usize = 64;
const HASH_DOMAIN: &str = "clud-reread-guard-v2\0";

/// A line interval a past read already brought into the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Span {
    /// First line, 1-based, as the harness numbers it.
    pub start: u64,
    /// Last line; `u64::MAX` when the read ran to the end of the file.
    pub end: u64,
    /// Unix seconds of the read that brought these lines in.
    pub at: u64,
}

impl Span {
    fn contains(self, other: Span) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    fn is_whole_file(self) -> bool {
        self.start <= 1 && self.end == u64::MAX
    }

    fn width(self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// What one Read call asked for, plus the fingerprint of the file now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Request {
    /// Hash of the canonical path: one entry per file per session.
    pub key: String,
    /// Absolute path as the harness spelled it.
    pub path: PathBuf,
    /// `(len, mtime_secs, mtime_nanos)`, or `None` when the file is absent.
    pub stamp: Option<Stamp>,
    /// The line interval this call would print, `at` filled in later.
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    pub len: u64,
    pub secs: u64,
    pub nanos: u32,
}

/// One file in one session: what it looked like, the lines already read, and
/// how often a covered read was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    pub len: u64,
    pub secs: u64,
    pub nanos: u32,
    /// Unix seconds of the last read that added lines.
    pub at: u64,
    /// File name only: the message is cheaper and the model knows the path.
    pub name: String,
    /// Intervals already in the conversation, sorted and merged.
    #[serde(default)]
    pub spans: Vec<Span>,
    /// Covered reads refused since this fingerprint appeared.
    #[serde(default)]
    pub refusals: u32,
}

impl Entry {
    fn same_stamp(&self, other: &Entry) -> bool {
        self.len == other.len && self.secs == other.secs && self.nanos == other.nanos
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(super) struct Ledger {
    #[serde(default)]
    pub entries: std::collections::BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    /// Allowed, but recorded: these lines are new to the conversation.
    Record,
    Deny {
        ago_secs: u64,
        kb: u64,
        /// The interval the refused call asked for, for the message.
        span: Span,
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
        .map(|path| expand_tilde(Path::new(path)))
}

/// A `Read` spelled with `~/` is the same file as its absolute path; without
/// this the two spellings would sit in separate entries and neither would see
/// the other's intervals.
fn expand_tilde(raw: &Path) -> PathBuf {
    let Some(rest) = raw.to_str().and_then(|text| text.strip_prefix("~/")) else {
        return raw.to_path_buf();
    };
    std::env::var_os("HOME")
        .map_or_else(|| raw.to_path_buf(), |home| PathBuf::from(home).join(rest))
}

/// The line interval a `Read` input asks for. `offset` is 1-based; a missing
/// `limit` runs to the end of the file.
pub(super) fn span_from(tool_input: Option<&Value>) -> Span {
    let input = tool_input;
    let start = input
        .and_then(|value| value.get("offset"))
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let end = match input
        .and_then(|value| value.get("limit"))
        .and_then(Value::as_u64)
    {
        Some(limit) if limit > 0 => start.saturating_add(limit - 1),
        _ => u64::MAX,
    };
    Span { start, end, at: 0 }
}

fn file_key(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN.as_bytes());
    hasher.update(b"file\0");
    hasher.update(path.to_string_lossy().as_bytes());
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    let mut span = span_from(tool_input);
    span.at = 0;
    debug_assert!(applies_to(tool_name));
    Some(Request {
        key: file_key(&path),
        stamp: stamp_of(&path),
        path,
        span,
    })
}

/// Sort, merge overlapping or adjacent intervals, and keep the widest
/// [`MAX_SPANS`]. Dropping the narrowest interval can only *allow* more reads,
/// so the cap can never turn into a wrong refusal.
pub(super) fn trim_spans(spans: &mut Vec<Span>) {
    spans.sort_by_key(|span| (span.start, span.end));
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        match merged.last_mut() {
            Some(last) if span.start <= last.end.saturating_add(1) => {
                last.end = last.end.max(span.end);
                last.at = last.at.min(span.at);
            }
            _ => merged.push(span),
        }
    }
    while merged.len() > MAX_SPANS {
        let narrowest = merged
            .iter()
            .enumerate()
            .min_by_key(|(_, span)| span.width())
            .map(|(index, _)| index);
        let Some(index) = narrowest else { return };
        merged.remove(index);
    }
    *spans = merged;
}

/// The decision table over the ledger: the verdict, and the entry to keep.
///
/// `fresh` is the entry this read would record if it happened; `now` is its
/// `at`. A different fingerprint means the file changed, so everything is
/// re-recorded and the read passes.
pub(super) fn advance(previous: Option<&Entry>, fresh: Entry, asked: Span) -> (Verdict, Entry) {
    let Some(previous) = previous else {
        return (Verdict::Record, fresh);
    };
    if !previous.same_stamp(&fresh) {
        return (Verdict::Record, fresh);
    }
    let Some(cover) = previous
        .spans
        .iter()
        .copied()
        .find(|span| span.contains(asked))
    else {
        let mut extended = previous.clone();
        extended.at = fresh.at;
        extended.spans.push(asked);
        trim_spans(&mut extended.spans);
        return (Verdict::Record, extended);
    };
    if previous.refusals >= MAX_REFUSALS {
        // The budget is spent: hand the read back and keep the record, so the
        // next covered read of this file is not charged again either.
        let mut allowed = previous.clone();
        allowed.at = fresh.at;
        return (Verdict::Allow, allowed);
    }
    let mut refused = previous.clone();
    refused.refusals = refused.refusals.saturating_add(1);
    let verdict = Verdict::Deny {
        ago_secs: fresh.at.saturating_sub(cover.at),
        kb: fresh.len / 1024,
        // `at` is meaningless on a refused interval: normalise it so the
        // verdict is a pure function of the ledger and the request.
        span: Span { at: 0, ..asked },
    };
    (verdict, refused)
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
    let mut span = request.span;
    span.at = now;
    let fresh = Entry {
        len: stamp.len,
        secs: stamp.secs,
        nanos: stamp.nanos,
        at: now,
        name: request
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        spans: vec![span],
        refusals: 0,
    };
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read(&path).ok();
    let mut ledger = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Ledger>(bytes).ok())
        .unwrap_or_default();
    let (verdict, kept) = advance(ledger.entries.get(&request.key), fresh, span);
    ledger.entries.insert(request.key.clone(), kept);
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
        Verdict::Deny { ago_secs, kb, span } => Some(deny_message(&read, span, ago_secs, kb)),
        Verdict::Allow | Verdict::Record => None,
    }
}

/// "just now" already reads as a time, so it takes no `ago` suffix.
fn ago_phrase(ago_secs: u64) -> String {
    if ago_secs <= 9 {
        "just now".to_string()
    } else {
        format!("{} ago", ago_text(ago_secs))
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

/// The interval as the message names it. A whole-file request is named as
/// such, because "lines 1-18446744073709551615" is not a sentence.
pub(super) fn span_text(span: Span) -> String {
    if span.is_whole_file() {
        return "the whole file".to_string();
    }
    if span.end == u64::MAX {
        return format!("lines {}-end", span.start);
    }
    format!("lines {}-{}", span.start, span.end)
}

pub(super) fn deny_message(request: &Request, span: Span, ago_secs: u64, kb: u64) -> String {
    let name = request
        .path
        .file_name()
        .map_or_else(String::new, |file| file.to_string_lossy().into_owned());
    format!(
        "[clud reread-guard] {name} ({kb} kB): {} is already in this conversation (read {}) and \
         the file has not changed since, so this read returns nothing new — every later request \
         re-sends it. Work from the copy you have. If you need a different part, pass \
         `offset`+`limit` (one range, <=200 lines); to map an unfamiliar file first, \
         `clud tool run code/outline.py <path>`. An edit changes the file and this is allowed \
         again. Off for this session: `{DISABLE_ENV}=0`.",
        span_text(span),
        ago_phrase(ago_secs)
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_reread_tests.rs"]
mod tests;

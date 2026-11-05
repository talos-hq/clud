//! Bounded process inspection (#2233).
//!
//! `pgrep -af PATTERN` and `ps ... args` print every matching process's *whole*
//! command line, and a build or gate process carries its entire heredoc in
//! argv. Measured on 24 h of local transcripts (`reports/rows-2026-11-03.jsonl`):
//! 284 process-inspection calls produced 455 kB of tool text, average 1.6 kB,
//! and one of them — `ps -eo pid,etime,args | grep -E 'local.gate|local_gate' |
//! grep -v grep | head` — returned **25,686 B for a handful of processes**.
//! Every later request in that session re-sends it.
//!
//! Nothing needs the argv tail: "is it running, since when, which pid" is the
//! question, and that answer is in the first ~160 columns of each line. So this
//! guard keeps every matching process visible and only bounds the width. Past
//! [`DEFAULT_LIMIT`] unbounded dumps in one session, the next **distinct** dump
//! is refused ONCE with the exact bounded form of the pipeline it just wrote
//! (`… | cut -c1-160`). A retry of the identical pipeline always passes, so the
//! guard can never wedge a session.
//!
//! Deliberately narrow:
//! * shell tools only, and only commands that print process argv (`pgrep -a`,
//!   `ps` with `args`/`aux`/`-ef`/`-eo`);
//! * a pipeline that already bounds its width (`cut -c`, `head -c`, `--width`,
//!   `fold`, `lsof -F`-style field output) is not counted;
//! * a redirect is a write, not tool output, and passes untouched;
//! * command substitutions and backticks are skipped: their output is consumed
//!   by the shell, not by the transcript;
//! * **fail open**: no session id, unreadable state, empty command, or a
//!   `kill $(pgrep …)`-shaped call allows the call. This is a nudged habit, not
//!   a security boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Unbounded dumps allowed per session; the next distinct one is refused once.
pub(super) const DEFAULT_LIMIT: u32 = 3;
/// A dump arriving after this long starts a new count.
pub(super) const RESET_SECS: u64 = 60 * 60;
/// Session-wide limit override; `0` disables the guard.
pub(super) const LIMIT_ENV: &str = "CLUD_PROC_SCAN_LIMIT";
/// Per-call override, written into the command: `CLUD_ALLOW_PROC_SCAN=1`.
pub(super) const ALLOW_TOKEN: &str = "CLUD_ALLOW_PROC_SCAN=1";
/// Columns kept per process line by the advised replacement.
pub(super) const BOUND_COLUMNS: u32 = 160;
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "proc-scan-guard";
const HASH_DOMAIN: &str = "clud-proc-scan-guard-v1\0";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const MAX_FILES: usize = 200;

/// Which program produced the unbounded argv, so the message can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Pgrep,
    Ps,
}

/// One unbounded process dump a command asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Dump {
    pub kind: Kind,
    /// The pipeline as the agent wrote it, whitespace-normalised.
    pub pipeline: String,
}

impl Dump {
    /// Stable identity for "this exact dump", so a forced retry is not refused
    /// twice.
    pub(super) fn signature(&self) -> String {
        let digest = Sha256::digest(format!("{HASH_DOMAIN}{}", self.pipeline).as_bytes());
        digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// The same pipeline with every line bounded to [`BOUND_COLUMNS`].
    pub(super) fn bounded(&self) -> String {
        format!("{} | cut -c1-{BOUND_COLUMNS}", self.pipeline)
    }
}

/// The persisted per-session state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    #[serde(default)]
    pub dumps: u32,
    #[serde(default)]
    pub last: u64,
    /// Signature -> refusal time. A retry of a refused dump is allowed.
    #[serde(default)]
    pub refused: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    Deny { dump: Dump, count: u32 },
}

/// The decision table over injected facts only: whether to refuse, and the
/// state to persist. `limit == 0` disables the guard.
pub(super) fn decide(
    previous: Option<&State>,
    dump: &Dump,
    now: u64,
    limit: u32,
) -> (Verdict, State) {
    let mut state = previous.cloned().unwrap_or_default();
    if now.saturating_sub(state.last) > RESET_SECS {
        state = State::default();
    }
    let signature = dump.signature();
    state.dumps = state.dumps.saturating_add(1);
    state.last = now;
    let mut verdict = Verdict::Allow;
    if limit > 0 && state.dumps > limit && !state.refused.contains_key(&signature) {
        state.refused.insert(signature, now);
        verdict = Verdict::Deny {
            dump: dump.clone(),
            count: state.dumps,
        };
    }
    trim(&mut state, now);
    (verdict, state)
}

/// Drop expired refusals, then the oldest ones so one long session cannot grow
/// the state file without bound.
fn trim(state: &mut State, now: u64) {
    state
        .refused
        .retain(|_, at| now.saturating_sub(*at) <= RESET_SECS);
    while state.refused.len() > 16 {
        let oldest = state
            .refused
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => {
                state.refused.remove(&key);
            }
            None => break,
        }
    }
}

fn program(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Split on `;`, `&`, newlines, `&&` and `||`, skipping quoted text. A pipeline
/// is never split on `|`: its stages share one width bound. Shell fd
/// redirections (`2>&1`, `>&2`) are masked first so their `&` cannot split.
fn pipelines(command: &str) -> Vec<String> {
    const MASKS: [(&str, &str); 3] = [("2>&1", "\u{1}E1"), (">&2", "\u{1}E2"), ("1>&2", "\u{1}E3")];
    let mut masked = command.to_string();
    for (from, to) in MASKS {
        masked = masked.replace(from, to);
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = masked.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(open) = quote {
            current.push(ch);
            if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            ';' | '\n' | '&' => {
                if ch == '&' && chars.peek() == Some(&'&') {
                    chars.next();
                }
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    out.push(current);
    out.into_iter()
        .map(|segment| {
            let mut text = segment.split_whitespace().collect::<Vec<_>>().join(" ");
            for (from, to) in MASKS {
                text = text.replace(to, from);
            }
            text
        })
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Whether the pipeline already bounds each line's width.
fn width_bounded(tokens: &[&str]) -> bool {
    for (index, token) in tokens.iter().enumerate() {
        let next = tokens.get(index + 1).copied().unwrap_or_default();
        match program(token) {
            "cut" if next.starts_with("-c") => return true,
            "head" if next.starts_with("-c") => return true,
            "fold" | "colrm" => return true,
            _ => {}
        }
        if *token == "--width" || token.starts_with("--width=") {
            return true;
        }
    }
    false
}

/// Whether the pipeline's stdout goes to a file rather than to the transcript.
fn writes_file(tokens: &[&str]) -> bool {
    tokens.iter().any(|token| {
        let stripped = token
            .replace("2>&1", "")
            .replace(">&2", "")
            .replace("1>&2", "");
        stripped.contains('>')
    })
}

fn pgrep_prints_argv(tokens: &[&str], at: usize) -> bool {
    tokens[at + 1..].iter().any(|flag| {
        *flag == "--list-full"
            || (flag.starts_with('-')
                && !flag.starts_with("--")
                && flag.len() > 1
                && flag.contains('a'))
    })
}

fn ps_prints_argv(tokens: &[&str], at: usize) -> bool {
    let mut args = tokens[at + 1..].iter().copied().peekable();
    while let Some(arg) = args.next() {
        if arg == "aux" || arg == "-ef" || arg == "-ax" || arg == "ax" || arg == "-A" {
            return true;
        }
        // `-eo pid,etime,args`, `-o args=`, `eo ...`: only the field list
        // decides whether the whole command line is printed.
        let fields = if let Some(rest) = arg.strip_prefix("-eo") {
            Some(rest.to_string())
        } else if let Some(rest) = arg.strip_prefix("-o") {
            Some(rest.to_string())
        } else if arg == "-e" || arg == "eo" || arg == "-O" {
            Some(
                args.peek()
                    .map(|next| (*next).to_string())
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        if let Some(fields) = fields {
            let fields = if fields.is_empty() {
                args.next()
                    .map(|next| (*next).to_string())
                    .unwrap_or_default()
            } else {
                fields
            };
            if fields.contains("args") || fields.contains("command") || fields.contains("cmd") {
                return true;
            }
        }
    }
    false
}

/// The unbounded process dumps a command asks for, in command order.
pub(super) fn dumps(command: &str) -> Vec<Dump> {
    if command.contains("$(") || command.contains('`') {
        return Vec::new();
    }
    let mut out = Vec::new();
    for pipeline in pipelines(command) {
        let tokens: Vec<&str> = pipeline
            .split([' ', '|'])
            .filter(|word| !word.is_empty())
            .collect();
        if tokens.is_empty() || width_bounded(&tokens) || writes_file(&tokens) {
            continue;
        }
        let mut kind = None;
        for (at, token) in tokens.iter().enumerate() {
            match program(token) {
                "pgrep" if pgrep_prints_argv(&tokens, at) => {
                    kind = Some(Kind::Pgrep);
                    break;
                }
                "ps" if ps_prints_argv(&tokens, at) => {
                    kind = Some(Kind::Ps);
                    break;
                }
                _ => {}
            }
        }
        if let Some(kind) = kind {
            out.push(Dump { kind, pipeline });
        }
    }
    out
}

/// Whether the command carries [`ALLOW_TOKEN`] as a whole word.
pub(super) fn opts_out(command: &str) -> bool {
    command.match_indices(ALLOW_TOKEN).any(|(at, _)| {
        let before = command[..at].chars().next_back();
        let after = command[at + ALLOW_TOKEN.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn applies_to(tool_name: &str) -> bool {
    matches!(
        super::block_bad_cmd_gate::tool_basename(tool_name).as_str(),
        "bash" | "shell" | "sh"
    )
}

fn session_file(state_dir: &Path, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{HASH_DOMAIN}session\0{session_id}").as_bytes());
    let hash: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    state_dir.join(DIR_NAME).join(format!("{hash}.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Delete state files older than [`MAX_AGE`], then keep the newest
/// [`MAX_FILES`]. Runs only when a session's file is first created.
pub(super) fn prune(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kept: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if now.duration_since(modified).is_ok_and(|age| age > MAX_AGE) {
            let _ = std::fs::remove_file(&path);
        } else {
            kept.push((modified, path));
        }
    }
    if kept.len() > MAX_FILES {
        kept.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        for (_, path) in kept.drain(MAX_FILES..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Record this dump against the session's state in `state_dir` and return the
/// verdict. Every failure allows.
pub(super) fn record(
    state_dir: &Path,
    session_id: &str,
    dump: &Dump,
    now: u64,
    limit: u32,
) -> Verdict {
    if limit == 0 || session_id.is_empty() {
        return Verdict::Allow;
    }
    let file = session_file(state_dir, session_id);
    let existing = std::fs::read(&file).ok();
    let previous = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<State>(bytes).ok());
    let (verdict, state) = decide(previous.as_ref(), dump, now, limit);
    let Ok(bytes) = serde_json::to_vec(&state) else {
        return Verdict::Allow;
    };
    if existing.is_none() {
        if std::fs::create_dir_all(state_dir.join(DIR_NAME)).is_err() {
            return Verdict::Allow;
        }
        prune(&state_dir.join(DIR_NAME), SystemTime::now());
    }
    if std::fs::write(&file, bytes).is_err() {
        return Verdict::Allow;
    }
    verdict
}

/// The limit: [`LIMIT_ENV`] wins, else [`DEFAULT_LIMIT`]. Unparsable values
/// fall back to the default; `0` disables the guard.
pub(super) fn limit_from(env: Option<&str>) -> u32 {
    env.and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_LIMIT)
}

/// Hook entry: the refusal reason for this call, or `None` to allow.
pub(super) fn reason(
    tool_name: &str,
    tool_input: Option<&Value>,
    session_id: Option<&str>,
) -> Option<String> {
    if !applies_to(tool_name) {
        return None;
    }
    let limit = limit_from(std::env::var(LIMIT_ENV).ok().as_deref());
    if limit == 0 {
        return None;
    }
    let session_id = session_id.filter(|id| !id.is_empty())?;
    let command = tool_input?.get("command")?.as_str()?;
    if opts_out(command) {
        return None;
    }
    let dump = dumps(command).into_iter().next()?;
    let state_dir = crate::daemon::default_state_dir().ok()?;
    match record(&state_dir, session_id, &dump, now_secs(), limit) {
        Verdict::Allow => None,
        Verdict::Deny { dump, count } => Some(deny_message(&dump, count, limit)),
    }
}

pub(super) fn deny_message(dump: &Dump, count: u32, limit: u32) -> String {
    let what = match dump.kind {
        Kind::Pgrep => "`pgrep -a` prints each match's whole command line",
        Kind::Ps => "`ps` with args/aux prints each match's whole command line",
    };
    format!(
        "[clud proc-scan-guard] this session has already run {count} unbounded process dumps \
         (limit {limit}). {what}, and a build or gate process carries its whole heredoc in argv — \
         one measured call of this shape returned 25,686 B for a handful of processes, re-sent on \
         every later request. Bound the width; every matching process, pid and elapsed time stays \
         visible: `{}`. Set {LIMIT_ENV}=0 to disable, or prefix the command with {ALLOW_TOKEN}.",
        dump.bounded()
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_proc_scan_tests.rs"]
mod tests;

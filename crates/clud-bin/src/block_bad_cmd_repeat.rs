//! Repeated-identical-call guard (#1674, parent #1276).
//!
//! In #1276 one MiMo response emitted 294 (then 371) identical Bash calls,
//! and every result went back into context. This guard denies a PreToolUse
//! call once the same call (tool name + canonical input) has run more than
//! [`DEFAULT_LIMIT`] times in a row in one session. A different call in
//! between, or an idle gap longer than [`IDLE_RESET_SECS`], resets the
//! streak. Contract and the evidence for the limit:
//! `docs/architecture/hook-dispatch.md#repeated-call-guard-1674` and DD-140.
//!
//! **Fail open.** This is a safety net against runaway loops, not a security
//! boundary: a missing session id, an unreadable home, or any state read or
//! write failure allows the call. State holds only a hash and two numbers.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Calls in a row that are still allowed; call `DEFAULT_LIMIT + 1` is denied.
pub(super) const DEFAULT_LIMIT: u32 = 200;
/// A repeat arriving more than this long after the previous call starts a
/// new streak: a scheduled tick (`/loop`, a long `sleep`) is not a burst.
pub(super) const IDLE_RESET_SECS: u64 = 120;
/// Per-call override, written into the command as `CLUD_ALLOW_REPEAT=1`.
pub(super) const ALLOW_REPEAT_TOKEN: &str = "CLUD_ALLOW_REPEAT=1";
/// Session-wide limit override; `0` disables the guard.
pub(super) const LIMIT_ENV: &str = "CLUD_REPEAT_CALL_LIMIT";
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "repeat-guard";
/// Gap-independent repeats of one read-only query before the poll note (#2631).
/// The burst guard above resets after [`IDLE_RESET_SECS`], so an agent that
/// polls one query every few minutes for hours never trips it; this second
/// counter ignores the gap and speaks once.
pub(super) const POLL_NOTE_AT: u32 = 6;
/// Repeat count for the second, shorter reminder of the same note.
pub(super) const POLL_NOTE_AGAIN_AT: u32 = 24;
/// Identical read-only queries allowed in one session before the poll guard
/// refuses the next one; call `POLL_DENY_AT + 1` is denied. The two notes above
/// are advisory, and on 2026-11-05 one session ignored both and issued the same
/// `gh issue view` query 127 times in 24 h (618 across five days), re-sending
/// its whole 270 kB conversation on every tick. Normal waiting resolves well
/// below this; anything above it is a wait, not a check.
pub(super) const POLL_DENY_AT: u32 = 96;
/// Session-wide poll limit override; `0` disables the poll guard.
pub(super) const POLL_LIMIT_ENV: &str = "CLUD_POLL_CALL_LIMIT";
/// Identical query calls further apart than this start a new poll count.
pub(super) const POLL_WINDOW_SECS: u64 = 6 * 60 * 60;
/// Poll queries one session's ledger remembers. One slot was not enough: an
/// agent that alternated `sleep 60; gh pr list --head X` with `cat
/// <task>.output; sleep 55; gh pr list --head X` presented a different exact
/// command on every other call, so the single slot reset on each flip and 197
/// identical queries produced no note at all (2026-11-08/09).
pub(super) const MAX_POLL_KEYS: usize = 8;
const HASH_DOMAIN: &str = "clud-repeat-guard-v1\0";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const MAX_FILES: usize = 500;

/// The persisted streak: a call hash, its run length and the last call time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Streak {
    pub key: String,
    pub count: u32,
    pub last: u64,
    /// Gap-independent repeat count for one read-only query (#2631). `0` in
    /// files written before this counter existed. Mirrors the entry the call
    /// just advanced in `polls`, so older readers still see a count.
    #[serde(default)]
    pub steady: u32,
    /// When the steady count last advanced.
    #[serde(default)]
    pub steady_last: u64,
    /// Every poll query this session has repeated recently, one entry each.
    /// Empty in files written before this ledger existed.
    #[serde(default)]
    pub polls: Vec<PollEntry>,
}

/// One poll query's gap-independent repeat count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PollEntry {
    pub key: String,
    pub count: u32,
    pub last: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    /// Burst guard: the same call more than `limit` times in a row (#1674).
    Deny {
        count: u32,
    },
    /// Poll guard: one read-only query repeated past its own, gap-independent
    /// limit (a patient wait rather than a burst).
    PollDeny {
        count: u32,
    },
}

/// The decision table, over injected facts only. Returns the verdict and the
/// streak to persist. `limit == 0` disables the guard.
pub(super) fn decide(
    previous: Option<&Streak>,
    key: &str,
    now: u64,
    limit: u32,
) -> (Verdict, Streak) {
    let continues = previous
        .is_some_and(|prev| prev.key == key && now.saturating_sub(prev.last) <= IDLE_RESET_SECS);
    let count = if continues {
        previous.map_or(1, |prev| prev.count.saturating_add(1))
    } else {
        1
    };
    let streak = Streak {
        key: key.to_string(),
        count,
        last: now,
        steady: 0,
        steady_last: 0,
        polls: Vec::new(),
    };
    let verdict = if limit > 0 && count > limit {
        Verdict::Deny { count }
    } else {
        Verdict::Allow
    };
    (verdict, streak)
}

/// The gap-independent repeat count for one read-only query, read from the
/// session's poll ledger. A *different* call no longer resets it — only this
/// query's own last run matters — because a poll an agent dresses differently
/// on alternate turns (a leading `sleep`, a different background-task read) is
/// still the same query, and one interleaved call used to erase the count. A
/// gap longer than [`POLL_WINDOW_SECS`] starts a new count: the point is the
/// patient poll the burst guard deliberately ignores.
///
/// A streak written before the ledger existed is seeded by [`migrated`], so an
/// upgrade does not restart a live count.
pub(super) fn poll_count(polls: &[PollEntry], key: &str, now: u64) -> (u32, u64) {
    polls
        .iter()
        .filter(|entry| entry.key == key && now.saturating_sub(entry.last) <= POLL_WINDOW_SECS)
        .map(|entry| entry.count.max(1))
        .max()
        .map_or((1, now), |count| (count.saturating_add(1), now))
}

/// The ledger a call counts against, with a `steady` slot folded in.
///
/// A session file written before the ledger existed holds one slot instead:
/// the exact `key` plus `steady`. That slot belongs to the call whose exact key
/// it names, so it is folded into **this** query only when the call being
/// counted is that same call — attributing it to any other query would grant
/// one query's count to another.
fn ledger_for_count(streak: Option<&Streak>, poll_key: &str, exact_key: &str) -> Vec<PollEntry> {
    let Some(prev) = streak else {
        return Vec::new();
    };
    let mut polls = prev.polls.clone();
    if prev.key != exact_key || prev.steady == 0 {
        return polls;
    }
    match polls.iter_mut().find(|entry| entry.key == poll_key) {
        Some(entry) => {
            if prev.steady > entry.count {
                entry.count = prev.steady;
                entry.last = entry.last.max(prev.steady_last);
            }
        }
        None => polls.push(PollEntry {
            key: poll_key.to_string(),
            count: prev.steady,
            last: prev.steady_last.max(prev.last),
        }),
    }
    polls
}

/// The ledger after this call: upsert `key`, drop entries outside the window,
/// and keep the newest [`MAX_POLL_KEYS`].
pub(super) fn ledger_after(polls: &[PollEntry], key: &str, count: u32, now: u64) -> Vec<PollEntry> {
    let mut entries: Vec<PollEntry> = polls
        .iter()
        .filter(|entry| entry.key != key && now.saturating_sub(entry.last) <= POLL_WINDOW_SECS)
        .cloned()
        .collect();
    entries.push(PollEntry {
        key: key.to_string(),
        count,
        last: now,
    });
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.last));
    entries.truncate(MAX_POLL_KEYS);
    entries
}

/// Words a shell puts in front of a program without changing what runs.
const PREFIX_WORDS: &[&str] = &[
    "do", "then", "else", "if", "while", "until", "time", "sudo", "command", "exec", "env", "nice",
    "nohup", "xargs", "watch",
];

/// `gh` verbs that only read.
const GH_READ: &[&str] = &[
    "view", "list", "status", "search", "diff", "checks", "download",
];

/// `gh` nouns that come before the verb.
const GH_NOUNS: &[&str] = &[
    "pr", "issue", "run", "release", "workflow", "label", "cache",
];

/// Remote verbs that only read, for an `ssh` poll (`ssh host 'systemctl
/// is-active x'`). The remote command is quoted, so it arrives as loose words.
const REMOTE_READ: &[&str] = &[
    "is-active",
    "is-enabled",
    "status",
    "show",
    "ps",
    "inspect",
    "logs",
    "cat",
    "tail",
    "journalctl",
    "get",
    "describe",
];

/// Verbs of a project task runner (`uv run --project ci errors --since 15m`)
/// that only read external state. A runner can execute anything, so the verb
/// is the whole signal; only read verbs are listed.
const RUNNER_READ: &[&str] = &[
    "errors", "events", "logs", "log", "status", "list", "ls", "show", "get", "view", "diff",
    "tail", "results", "report", "ps", "inspect", "describe", "wait",
];

/// Flags of a project runner that take the next word as their value, so the
/// value is not mistaken for the verb.
const RUNNER_VALUE_FLAGS: &[&str] = &[
    "--project",
    "-p",
    "--directory",
    "-C",
    "--package",
    "--python",
    "--with",
    "--env-file",
];

/// Whether the segment from `start` runs a project runner's read-only verb:
/// `uv run [--project X] <verb> [args...]`. Anything else (a missing `run`, a
/// non-read verb, a write flag) is not a query.
fn runner_reads(words: &[String], start: usize) -> bool {
    let word = |offset: usize| -> &str {
        words
            .get(offset)
            .map_or("", |w| w.trim_matches(&['\'', '"'][..]))
    };
    let mut index = start + 1;
    if word(index) != "run" {
        return false;
    }
    index += 1;
    while index < words.len() {
        let flag = word(index);
        if flag.starts_with('-') {
            index += if RUNNER_VALUE_FLAGS.contains(&flag) {
                2
            } else {
                1
            };
            continue;
        }
        return RUNNER_READ.contains(&flag) && !writes(words);
    }
    false
}

/// Flags that make a request write instead of read.
const WRITE_FLAGS: &[&str] = &[
    "-X",
    "-d",
    "-F",
    "-T",
    "-D",
    "--method",
    "--data",
    "--field",
    "--raw-field",
    "--upload-file",
    "--input",
    "-f",
];

/// Whether one word is a write flag, including the `--data-name value` family.
fn is_write_flag(word: &str) -> bool {
    let word = word.trim_matches(&['\'', '"'][..]);
    WRITE_FLAGS.contains(&word)
        || word.starts_with("--data-")
        || word.starts_with("--field=")
        || word.starts_with("--raw-field=")
}

/// Whether any word in the simple command turns it into a write.
fn writes(words: &[String]) -> bool {
    words.iter().any(|word| is_write_flag(word))
}

/// Whether one simple command only reads external state. Used solely to decide
/// whether a repeat deserves an advisory note: a false negative costs nothing
/// and a false positive is one line of advice.
fn reads_state(words: &[String]) -> bool {
    let mut index = 0;
    while index < words.len() {
        let word = words[index].trim_matches(&['\'', '"'][..]);
        let assignment = word.contains('=') && !word.contains(['/', ' ', '\t']);
        if PREFIX_WORDS.contains(&word) || assignment {
            index += 1;
            continue;
        }
        if word == "timeout" {
            // `timeout [-k D] <duration> <command>`: the duration is not a program.
            index += 1;
            while index < words.len() && words[index].starts_with('-') {
                index += 1;
            }
            index += 1;
            continue;
        }
        let arg = |offset: usize| -> &str {
            words
                .get(index + offset)
                .map_or("", |w| w.trim_matches(&['\'', '"'][..]))
        };
        let next = arg(1);
        return match word {
            "gh" => {
                if GH_READ.contains(&next) {
                    return true;
                }
                if GH_NOUNS.contains(&next) {
                    return GH_READ.contains(&arg(2));
                }
                next == "api" && !writes(words)
            }
            "uv" => runner_reads(words, index),
            "git" => next == "ls-remote",
            "curl" => !writes(words),
            "ssh" => words
                .iter()
                .any(|word| REMOTE_READ.contains(&word.as_str())),
            "systemctl" | "docker" | "kubectl" => matches!(
                next,
                "" | "status"
                    | "is-active"
                    | "is-enabled"
                    | "show"
                    | "ps"
                    | "inspect"
                    | "logs"
                    | "get"
                    | "describe"
                    | "cat"
                    | "tail"
            ),
            _ => false,
        };
    }
    false
}

/// Whether `command` contains a read-only query of external state, split the
/// way a shell would split it.
pub(super) fn is_query(command: &str) -> bool {
    segments(command).iter().any(|text| {
        let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        reads_state(&words)
    })
}

/// Segments that schedule or narrate a wait instead of reading state. A poll
/// dressed with one of these is the same query: dropping them is what lets an
/// alternating `sleep N; gh pr list --head X` / `cat <task>.output; sleep N;
/// gh pr list --head X` loop share one count.
const SCHEDULING_PROGRAMS: &[&str] = &["sleep", "true", ":", "echo", "printf", "date", "pwd"];

/// Readers of a background task's own output file. The harness delivers that
/// output when the task ends, so re-reading it on every turn is narration, not
/// a new query.
const TASK_OUTPUT_READERS: &[&str] = &["cat", "tail", "head", "less", "wc"];

/// Split a command the way [`is_query`] does, into `;`-separated pieces.
fn segments(command: &str) -> Vec<String> {
    let mut segment = String::with_capacity(command.len());
    let mut out: Vec<String> = Vec::new();
    for ch in command.chars() {
        if ";&|\n(){}".contains(ch) {
            if !segment.trim().is_empty() {
                out.push(std::mem::take(&mut segment));
            }
        } else {
            segment.push(ch);
        }
    }
    if !segment.trim().is_empty() {
        out.push(segment);
    }
    out
}

/// Whether one segment only schedules, narrates or re-reads a background
/// task's own output.
fn is_scheduling(words: &[String]) -> bool {
    let mut words = words.iter();
    let Some(first) = words.next() else {
        return true;
    };
    let program = crate::block_bad_cmd::block_bad_cmd_shell::program_name(first);
    if SCHEDULING_PROGRAMS.contains(&program.as_str()) {
        return true;
    }
    TASK_OUTPUT_READERS.contains(&program.as_str())
        && words.any(|word| word.contains("/tasks/") && word.contains(".output"))
}

/// Pipeline stages that only present or filter another command's output: they
/// choose how much of an answer is shown, not which answer it is, so
/// `| grep -v "ERR 429" | tail -10` must not split one poll into two keys.
const PRESENTATION_PROGRAMS: &[&str] = &[
    "grep", "egrep", "fgrep", "rg", "tail", "head", "sed", "cut", "wc", "sort", "uniq", "tr",
    "column", "fmt", "fold", "jq", "awk", "less", "more",
];

/// A segment that only sets context for the query — a directory change — never
/// the query itself.
fn is_context(words: &[String]) -> bool {
    let Some(first) = words.first() else {
        return true;
    };
    crate::block_bad_cmd::block_bad_cmd_shell::program_name(first) == "cd"
}

/// The segment with a leading `timeout [-k D] <duration>` wrapper removed: the
/// wrapper bounds how long a query may take, not which query it is.
fn strip_timeout(words: &[String]) -> Vec<String> {
    let first = words.first().map_or("", String::as_str);
    if crate::block_bad_cmd::block_bad_cmd_shell::program_name(first) != "timeout" {
        return words.to_vec();
    }
    let mut index = 1;
    while index < words.len() && words[index].starts_with('-') {
        index += 1;
    }
    words
        .iter()
        .skip(index.saturating_add(1))
        .cloned()
        .collect()
}

/// Whether a segment only presents another segment's output.
fn is_presentation(words: &[String]) -> bool {
    let Some(first) = words.first() else {
        return true;
    };
    PRESENTATION_PROGRAMS
        .contains(&crate::block_bad_cmd::block_bad_cmd_shell::program_name(first).as_str())
}

/// One segment, whitespace-collapsed, its `--json` field list sorted, and its
/// words sorted: flag order and field order are presentation, so either one
/// reordered must not split one query into two keys.
fn canonical_segment(text: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let raw: Vec<&str> = text.split_whitespace().collect();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == "--json" && index + 1 < raw.len() {
            let mut fields: Vec<&str> = raw[index + 1].split(',').collect();
            fields.sort_unstable();
            words.push("--json".to_string());
            words.push(fields.join(","));
            index += 2;
            continue;
        }
        words.push(raw[index].to_string());
        index += 1;
    }
    words.sort_unstable();
    words.join(" ")
}

/// The state-reading core of a command: its query segments, with scheduling
/// noise dropped. Empty when the command is nothing but noise.
pub(super) fn poll_core(command: &str) -> String {
    let kept: Vec<Vec<String>> = segments(command)
        .iter()
        .map(|segment| {
            strip_timeout(
                &segment
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<String>>(),
            )
        })
        .filter(|words| !words.is_empty())
        .filter(|words| !is_scheduling(words) && !is_context(words))
        .collect();
    // Presentation stages are noise only next to a real query: a lone
    // `grep -r <pattern>` IS the query, and dropping it would merge unrelated
    // searches into one count.
    let has_query = kept.iter().any(|words| !is_presentation(words));
    kept.iter()
        .filter(|words| !(has_query && is_presentation(words)))
        .map(|words| canonical_segment(&words.join(" ")))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<String>>()
        .join("; ")
}

/// The poll ledger key: `sha256(domain + "poll" + tool + core)`, so the same
/// query under a different wait dressing shares one count. Falls back to the
/// exact [`call_key`] when the command has no readable core.
pub(super) fn poll_key(tool_name: &str, tool_input: Option<&Value>) -> String {
    let command = tool_input
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let core = poll_core(command);
    if core.is_empty() {
        return call_key(tool_name, tool_input);
    }
    let digest = Sha256::digest(format!("{HASH_DOMAIN}poll\0{tool_name}\0{core}").as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The one advisory line the poll note carries (#2631).
pub(super) fn poll_message(count: u32) -> String {
    format!(
        "[clud poll-note] this same read-only query has now run {count} times in this session, \
         and every repeat re-sends the whole conversation before the same answer comes back. If \
         you are waiting for external state, block once instead of polling: \
         `clud tool run wait/until.py -- --cmd '<the same command>' --interval 120 --timeout 3600` \
         returns as soon as its output changes. If the answer has not changed, waiting will not \
         change it: an empty `gh pr list --head <branch>` means the PR does not exist yet, and \
         creating it is synchronous (`gh pr create`). Otherwise read the last answer and decide. \
         This note is advisory; identical repeats stop being allowed past {POLL_DENY_AT} in a \
         session."
    )
}

/// Canonical JSON: object keys sorted at every depth, so key order in the
/// payload cannot split one call into two keys.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The streak key: first 16 hex chars of
/// `sha256(domain + tool_name + "\0" + canonical(tool_input))`.
pub(super) fn call_key(tool_name: &str, tool_input: Option<&Value>) -> String {
    let mut text = String::new();
    canonical(tool_input.unwrap_or(&Value::Null), &mut text);
    let digest = Sha256::digest(format!("{HASH_DOMAIN}{tool_name}\0{text}").as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn session_file(state_dir: &Path, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{HASH_DOMAIN}session\0{session_id}").as_bytes());
    let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    state_dir.join(DIR_NAME).join(format!("{hash}.json"))
}

/// Whether the command carries `CLUD_ALLOW_REPEAT=1` as a whole word.
pub(super) fn opts_out(command: &str) -> bool {
    command.match_indices(ALLOW_REPEAT_TOKEN).any(|(at, _)| {
        let before = command[..at].chars().next_back();
        let after = command[at + ALLOW_REPEAT_TOKEN.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The limit: [`LIMIT_ENV`] wins, then `hooks.repeat_call_limit` in
/// `~/.clud/settings.json`, then [`DEFAULT_LIMIT`]. Unparsable values fall
/// back to the default.
pub(super) fn limit_from(env: Option<&str>, settings: Option<&Value>) -> u32 {
    if let Some(value) = env.and_then(|v| v.trim().parse::<u32>().ok()) {
        return value;
    }
    settings
        .and_then(|doc| doc.get("hooks"))
        .and_then(|hooks| hooks.get("repeat_call_limit"))
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LIMIT, |v| u32::try_from(v).unwrap_or(u32::MAX))
}

/// The poll limit: [`POLL_LIMIT_ENV`] wins, then `hooks.poll_call_limit` in
/// `~/.clud/settings.json`, then [`POLL_DENY_AT`]. `0` disables the poll guard
/// only; the burst guard keeps its own limit.
pub(super) fn poll_limit_from(env: Option<&str>, settings: Option<&Value>) -> u32 {
    if let Some(value) = env.and_then(|v| v.trim().parse::<u32>().ok()) {
        return value;
    }
    settings
        .and_then(|doc| doc.get("hooks"))
        .and_then(|hooks| hooks.get("poll_call_limit"))
        .and_then(Value::as_u64)
        .map_or(POLL_DENY_AT, |v| u32::try_from(v).unwrap_or(u32::MAX))
}

/// `~/.clud/settings.json`, parsed, or `None` when it is absent or malformed.
fn settings_doc() -> Option<Value> {
    crate::clud_settings::home_dir_path()
        .ok()
        .and_then(|home| std::fs::read(crate::clud_settings::settings_path_at(&home)).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
}

fn effective_limit() -> u32 {
    let env = std::env::var(LIMIT_ENV).ok();
    if env
        .as_deref()
        .is_some_and(|v| v.trim().parse::<u32>().is_ok())
    {
        return limit_from(env.as_deref(), None);
    }
    limit_from(None, settings_doc().as_ref())
}

fn effective_poll_limit() -> u32 {
    let env = std::env::var(POLL_LIMIT_ENV).ok();
    if env
        .as_deref()
        .is_some_and(|v| v.trim().parse::<u32>().is_ok())
    {
        return poll_limit_from(env.as_deref(), None);
    }
    poll_limit_from(None, settings_doc().as_ref())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Delete streak files older than [`MAX_AGE`], then keep the newest
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
            .and_then(|m| m.modified())
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

/// Temp file + rename, **without** `fsync`: this runs in front of every tool
/// call, and an fsync per call cost ~110 ms of p99 on CI disks (#1674). A
/// streak lost to a crash only undercounts, which this fail-open guard
/// tolerates. Owner-only mode on Unix; the file holds only a hash and counts.
pub(super) fn write_fast(path: &Path, bytes: &[u8], create_dir: bool) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if create_dir {
        std::fs::create_dir_all(parent)?;
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let temp = parent.join(format!(".tmp-{}-{nanos}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    crate::platform::block_bad_cmd_repeat::set_owner_only_mode(&mut options);
    let result = options
        .open(&temp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// What the hook should do with this call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Allow,
    /// Advisory only: the call runs, with one line added to the agent's context.
    Note(String),
    Deny(String),
    /// The patient-poll stop: one read-only query repeated past
    /// [`POLL_DENY_AT`].
    PollDeny(String),
}

/// The poll facts a call carries, as one argument so the burst inputs stay
/// readable: whether the command reads state, the ledger key of its
/// state-reading core, and the session's poll limit.
#[derive(Debug, Clone, Copy)]
pub(super) struct Past<'a> {
    pub is_query: bool,
    pub poll_key: &'a str,
    pub poll_limit: u32,
}

/// Record this call against the session's ledger in `state_dir` and return
/// the burst verdict plus the poll-note count. Every failure allows.
pub(super) fn check_at(
    state_dir: &Path,
    session_id: &str,
    key: &str,
    past: Past<'_>,
    now: u64,
    limit: u32,
) -> (Verdict, Option<u32>) {
    if limit == 0 || session_id.is_empty() {
        return (Verdict::Allow, None);
    }
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read(&path).ok();
    let previous = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Streak>(bytes).ok());
    let ledger = ledger_for_count(
        previous.as_ref().filter(|_| past.is_query),
        past.poll_key,
        key,
    );
    let (verdict, mut streak) = decide(previous.as_ref(), key, now, limit);
    let (steady, steady_last) = if past.is_query {
        poll_count(&ledger, past.poll_key, now)
    } else {
        (0, 0)
    };
    streak.steady = steady;
    streak.steady_last = steady_last;
    streak.polls = if past.is_query {
        ledger_after(&ledger, past.poll_key, steady, now)
    } else {
        previous
            .as_ref()
            .map(|prev| prev.polls.clone())
            .unwrap_or_default()
    };
    let Ok(bytes) = serde_json::to_vec(&streak) else {
        return (verdict, None);
    };
    if write_fast(&path, &bytes, existing.is_none()).is_err() {
        return (verdict, None);
    }
    if existing.is_none() {
        if let Some(dir) = path.parent() {
            prune(dir, SystemTime::now());
        }
    }
    // The poll guard speaks after the burst guard: a burst deny already stops
    // the call, and its message names the same waiter.
    let poll_limit = past.poll_limit;
    if verdict == Verdict::Allow && poll_limit > 0 && steady > poll_limit {
        return (Verdict::PollDeny { count: steady }, None);
    }
    let note = matches!(steady, POLL_NOTE_AT | POLL_NOTE_AGAIN_AT).then_some(steady);
    (verdict, note)
}

/// Hook entry: deny, an advisory note, or allow.
pub(super) fn outcome(
    tool_name: &str,
    tool_input: Option<&Value>,
    session_id: Option<&str>,
) -> Outcome {
    let Some(session_id) = session_id else {
        return Outcome::Allow;
    };
    let limit = effective_limit();
    if limit == 0 {
        return Outcome::Allow;
    }
    let Ok(state_dir) = crate::daemon::default_state_dir() else {
        return Outcome::Allow;
    };
    let key = call_key(tool_name, tool_input);
    let command = tool_input
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let query = is_query(command);
    let poll = poll_key(tool_name, tool_input);
    let past = Past {
        is_query: query,
        poll_key: &poll,
        poll_limit: effective_poll_limit(),
    };
    match check_at(&state_dir, session_id, &key, past, now_secs(), limit) {
        (Verdict::Deny { count }, _) => Outcome::Deny(deny_message(tool_name, count, limit)),
        (Verdict::PollDeny { count }, _) => {
            Outcome::PollDeny(poll_deny_message(count, past.poll_limit))
        }
        (Verdict::Allow, Some(count)) => Outcome::Note(poll_message(count)),
        (Verdict::Allow, None) => Outcome::Allow,
    }
}

/// The poll guard's refusal: the same read-only query, repeated far past what
/// any check needs. Names the waiter and the deliberate-repeat override.
pub(super) fn poll_deny_message(count: u32, limit: u32) -> String {
    format!(
        "[clud poll-guard] this same read-only query has now run {count} times in this session \
         (limit {limit}), and every repeat re-sent the whole conversation before the same answer \
         came back. Stop polling and block once instead: \
         `clud tool run wait/until.py -- --cmd '<the same command>' --interval 120 --timeout 3600` \
         runs it once as a baseline and returns as soon as its output changes. To re-read this \
         identical command deliberately anyway, prefix it with `{ALLOW_REPEAT_TOKEN}` (audited in \
         the hook log)."
    )
}

pub(super) fn deny_message(tool_name: &str, count: u32, limit: u32) -> String {
    format!(
        "[clud repeat-guard] this exact {tool_name} call has now been issued {count} times in a \
         row (limit {limit}). Stop repeating it: re-read the last result and make a fresh \
         decision. Do not poll: to wait on something, run it once with `run_in_background` and \
         act on its completion notice, or for a PR's CI run \
         `clud tool run github/pr_merge_watch.py <PR>` (#1894). Otherwise change the call. To allow one repeat deliberately, prefix the command with \
         `{ALLOW_REPEAT_TOKEN}` (audited in the hook log)."
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_repeat_tests.rs"]
mod tests;

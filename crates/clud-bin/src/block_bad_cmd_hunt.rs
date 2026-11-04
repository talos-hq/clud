//! Multi-range shell read hunt guard (#2233).
//!
//! One `sed -n '40,60p' a.rs; sed -n '120,160p' a.rs; sed -n '400,430p' b.rs`
//! call lands two to four disjoint windows in the transcript, and every later
//! request in that session re-sends all of them. Measured on 24 h of local
//! transcripts: 1,143 shell `sed` calls produced 3.9 MB of results, 91 % of
//! them multi-command hunts, and that 3.9 MB is ~37 % of all tool-result text
//! (10.5 MB) that the fleet re-sends 71× on average.
//!
//! The bundled [`code/slice.py`](../../../assets/tools/code/slice.py) reads
//! every range in one call, merges overlaps, prefixes `LINE| ` instead of
//! repeating the path, and stops at a byte cap with a footer naming the line
//! to continue from. Agents keep typing `sed` because it is familiar, so the
//! guard converts the habit at the hook instead of in a doc: past
//! [`DEFAULT_LIMIT`] hunts in one session the next hunt is refused ONCE with
//! the exact `slice` invocation for the ranges it just named. A retry of the
//! identical command passes, so the guard can never wedge a session.
//!
//! Deliberately narrow:
//! * shell tools only, and only segments that are `sed -n A,Bp FILE` — a single
//!   ranged read is not a hunt;
//! * every target must exist, carry a source extension, and be at least
//!   [`MIN_BYTES`]; hunts over small files are cheap;
//! * the ranges must add up to at least [`MIN_TOTAL_LINES`] requested lines,
//!   so a two-line confirmation read is never called a hunt;
//! * `sed -i` and redirects are writes, not reads, and pass untouched;
//! * **fail open**: no session id, unreadable state, unparsable command, or an
//!   unknown flag cluster allows the call. This is a nudged behaviour, not a
//!   security boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Multi-range hunts allowed per session; the next distinct hunt is refused.
pub(super) const DEFAULT_LIMIT: u32 = 3;
/// A hunt arriving after this long starts a new count: a later pass over a
/// file is not the same hunt.
pub(super) const RESET_SECS: u64 = 60 * 60;
/// Files smaller than this are cheap to slice with any tool; never counted.
pub(super) const MIN_BYTES: u64 = 8 * 1024;
/// Requested lines across the hunt must reach this before it is worth a refusal.
pub(super) const MIN_TOTAL_LINES: u32 = 80;
/// Per-call override, written into the command: `CLUD_ALLOW_READ_HUNT=1`.
pub(super) const ALLOW_TOKEN: &str = "CLUD_ALLOW_READ_HUNT=1";
/// Session-wide limit override; `0` disables the guard.
pub(super) const LIMIT_ENV: &str = "CLUD_READ_HUNT_LIMIT";
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "read-hunt-guard";
const HASH_DOMAIN: &str = "clud-read-hunt-guard-v1\0";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const MAX_FILES: usize = 500;
/// Ranges printed in one replacement command; more would not fit the advice.
const MAX_ADVISED_RANGES: usize = 6;
/// Files whose replacement commands fit in the advice.
const MAX_ADVISED_FILES: usize = 2;
/// Extensions worth one bounded read. Unknown extensions are never counted, so
/// data files and logs cannot trip the guard.
const SOURCE_EXTENSIONS: [&str; 27] = [
    "rs", "py", "toml", "md", "json", "jsonl", "ts", "tsx", "js", "jsx", "nix", "yml", "yaml", "c",
    "h", "cc", "cpp", "hpp", "go", "java", "kt", "rb", "sh", "css", "html", "sql", "proto",
];

/// One ranged read a command asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Range {
    pub path: String,
    pub start: u32,
    pub end: u32,
}

/// The ranges a single command asks for, in command order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Hunt {
    pub ranges: Vec<Range>,
}

impl Hunt {
    pub(super) fn lines(&self) -> u32 {
        self.ranges
            .iter()
            .map(|range| range.end.saturating_sub(range.start).saturating_add(1))
            .sum()
    }

    /// Stable identity for "this exact hunt", so a forced retry is not refused
    /// a second time.
    pub(super) fn signature(&self) -> String {
        let mut key = String::new();
        for range in &self.ranges {
            key.push_str(&format!("{}\0{}\0{}\n", range.path, range.start, range.end));
        }
        let digest = Sha256::digest(format!("{HASH_DOMAIN}{key}").as_bytes());
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The persisted per-session state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    #[serde(default)]
    pub hunts: u32,
    #[serde(default)]
    pub last: u64,
    /// Signature -> refusal time. A retry of a refused hunt is allowed.
    #[serde(default)]
    pub refused: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    Deny { hunt: Hunt, count: u32 },
}

/// The decision table over injected facts only: whether to refuse, and the
/// state to persist. `limit == 0` disables the guard.
pub(super) fn decide(
    previous: Option<&State>,
    hunt: &Hunt,
    now: u64,
    limit: u32,
) -> (Verdict, State) {
    let mut state = previous.cloned().unwrap_or_default();
    if now.saturating_sub(state.last) > RESET_SECS {
        state = State::default();
    }
    let signature = hunt.signature();
    state.hunts = state.hunts.saturating_add(1);
    state.last = now;
    let mut verdict = Verdict::Allow;
    if limit > 0 && state.hunts > limit && !state.refused.contains_key(&signature) {
        state.refused.insert(signature, now);
        verdict = Verdict::Deny {
            hunt: hunt.clone(),
            count: state.hunts,
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

fn is_source_file(token: &str) -> bool {
    let Some(extension) = token.rsplit_once('.').map(|(_, ext)| ext) else {
        return false;
    };
    SOURCE_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
}

fn plausible_path(token: &str) -> bool {
    let token = token.trim_matches(|c| c == '"' || c == '\'');
    !token.is_empty()
        && !token.starts_with('-')
        && !token.contains(['>', '<', '*', '?', '$', '(', ')', '`', '=', '|', ';', '&'])
        && is_source_file(token)
}

fn program(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// `40,60p` (quoted or not) -> `(40, 60)`. Anything else is not a range.
fn parse_range(token: &str) -> Option<(u32, u32)> {
    let token = token.trim_matches(|c| c == '"' || c == '\'');
    let body = token.strip_suffix('p')?;
    let (start, end) = body.split_once(',')?;
    let start = start.trim().parse::<u32>().ok()?;
    let end = end.trim().parse::<u32>().ok()?;
    (start <= end && start > 0).then_some((start, end))
}

fn is_in_place_sed(tokens: &[&str]) -> bool {
    let Some(at) = tokens.iter().position(|token| program(token) == "sed") else {
        return false;
    };
    tokens[at + 1..].iter().any(|token| {
        if *token == "--in-place" || token.starts_with("--in-place=") {
            return true;
        }
        token.starts_with('-') && !token.starts_with("--") && token.len() > 1 && token.contains('i')
    })
}

/// Whether the segment writes a file (`>` / `>>` / `2>`), which makes it a
/// write, not a read whose output lands in the transcript.
fn writes_file(tokens: &[&str]) -> bool {
    tokens
        .iter()
        .any(|token| token.contains('>') && !token.contains('<'))
}

/// The ranged reads a command asks for. Segments split on `;`, `&&`, `&` and
/// newlines so only `sed -n` reads contribute. A segment carrying a pipe is
/// skipped: its output is bounded by whatever it feeds, so it is not the
/// unbounded hunt this guard is about.
pub(super) fn hunt_ranges(command: &str) -> Vec<Range> {
    let mut out = Vec::new();
    for segment in command.split([';', '&', '\n']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        if tokens.is_empty()
            || segment.contains('|')
            || is_in_place_sed(&tokens)
            || writes_file(&tokens)
        {
            continue;
        }
        if !tokens.iter().any(|token| program(token) == "sed") {
            continue;
        }
        let mut range: Option<(u32, u32)> = None;
        let mut path: Option<String> = None;
        for token in &tokens {
            if range.is_none() {
                if let Some(parsed) = parse_range(token) {
                    range = Some(parsed);
                    continue;
                }
            } else if plausible_path(token) {
                path = Some(token.trim_matches(|c| c == '"' || c == '\'').to_string());
            }
        }
        if let (Some((start, end)), Some(path)) = (range, path) {
            out.push(Range { path, start, end });
        }
    }
    out
}

/// The hunt this command is, or `None` when it does not qualify. Targets must
/// resolve to existing source files of at least [`MIN_BYTES`] and the command
/// must ask for at least [`MIN_TOTAL_LINES`] lines across at least two ranges.
pub(super) fn detect(command: &str, cwd: &Path) -> Option<Hunt> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut ranges = Vec::new();
    for range in hunt_ranges(command) {
        let path = resolve(&range.path, cwd, home.as_deref())?;
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() >= MIN_BYTES) {
            return None;
        }
        ranges.push(Range {
            path: path.to_string_lossy().into_owned(),
            ..range
        });
    }
    if ranges.len() < 2 {
        return None;
    }
    let hunt = Hunt { ranges };
    (hunt.lines() >= MIN_TOTAL_LINES).then_some(hunt)
}

fn resolve(target: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = target.strip_prefix("~/") {
        return Some(home?.join(rest));
    }
    let path = Path::new(target);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    Some(
        resolved
            .components()
            .filter(|component| !matches!(component, std::path::Component::CurDir))
            .collect(),
    )
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

fn shell_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "bash" | "Shell" | "shell")
}

fn session_file(state_dir: &Path, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{HASH_DOMAIN}session\0{session_id}").as_bytes());
    let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
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
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let modified = meta.modified().unwrap_or(now);
        if now.duration_since(modified).is_ok_and(|age| age > MAX_AGE) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        kept.push((modified, path));
    }
    if kept.len() <= MAX_FILES {
        return;
    }
    kept.sort_by_key(|(modified, _)| *modified);
    let excess = kept.len() - MAX_FILES;
    for (_, path) in kept.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// Compare-and-decide against the persisted session state, then store the new
/// one. Any filesystem error allows the call.
pub(super) fn check_at(
    state_dir: &Path,
    session_id: &str,
    hunt: &Hunt,
    now: u64,
    limit: u32,
) -> Verdict {
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<State>(&text).ok());
    let (verdict, state) = decide(existing.as_ref(), hunt, now, limit);
    let Ok(bytes) = serde_json::to_vec(&state) else {
        return Verdict::Allow;
    };
    if existing.is_none() {
        if std::fs::create_dir_all(state_dir.join(DIR_NAME)).is_err() {
            return Verdict::Allow;
        }
        prune(&state_dir.join(DIR_NAME), SystemTime::now());
    }
    if std::fs::write(&path, bytes).is_err() {
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

fn effective_limit() -> u32 {
    limit_from(std::env::var(LIMIT_ENV).ok().as_deref())
}

/// Hook entry: the refusal reason for this call, or `None` to allow.
pub(super) fn reason(
    tool_name: &str,
    tool_input: Option<&Value>,
    session_id: Option<&str>,
    cwd: &Path,
) -> Option<String> {
    if !shell_tool(tool_name) {
        return None;
    }
    let session_id = session_id?;
    let limit = effective_limit();
    if limit == 0 {
        return None;
    }
    let command = tool_input?.get("command")?.as_str()?;
    if opts_out(command) {
        return None;
    }
    let hunt = detect(command, cwd)?;
    let state_dir = crate::daemon::default_state_dir().ok()?;
    match check_at(&state_dir, session_id, &hunt, now_secs(), limit) {
        Verdict::Allow => None,
        Verdict::Deny { hunt, count } => Some(deny_message(&hunt, count, limit)),
    }
}

/// The replacement commands for the ranges the hunt just named, one command
/// per file because `code/slice.py` reads a single file per call.
pub(super) fn slice_command(hunt: &Hunt) -> String {
    let mut files: Vec<(&str, Vec<&Range>)> = Vec::new();
    for range in &hunt.ranges {
        match files.iter_mut().find(|(path, _)| *path == range.path) {
            Some((_, ranges)) => ranges.push(range),
            None => files.push((&range.path, vec![range])),
        }
    }
    let mut commands: Vec<String> = Vec::new();
    for (path, ranges) in files.iter().take(MAX_ADVISED_FILES) {
        let mut command = format!("clud tool run code/slice.py {path}");
        for range in ranges.iter().take(MAX_ADVISED_RANGES) {
            command.push_str(&format!(" --lines {}-{}", range.start, range.end));
        }
        if ranges.len() > MAX_ADVISED_RANGES {
            command.push_str(&format!(
                " (+{} more --lines)",
                ranges.len() - MAX_ADVISED_RANGES
            ));
        }
        commands.push(command);
    }
    if files.len() > MAX_ADVISED_FILES {
        commands.push("one more per remaining file".to_string());
    }
    commands.join(" ; ")
}

pub(super) fn deny_message(hunt: &Hunt, count: u32, limit: u32) -> String {
    format!(
        "[clud read-hunt-guard] this session has already run {count} multi-range shell reads \
         (limit {limit}). All {} of the ranges in this call go into the transcript and every \
         later request in the session re-sends them. Read each file in one bounded call instead: \
         `{}` — it merges overlapping ranges, prints `LINE| text` once per line, caps the text \
         at 8 KiB, \
         and its footer names the line to continue from. Outline first when the file is \
         unfamiliar: `clud tool run code/outline.py -- <path>`. To run this call as written, \
         prefix it with `{ALLOW_TOKEN}`.",
        hunt.ranges.len(),
        slice_command(hunt)
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_hunt_tests.rs"]
mod tests;

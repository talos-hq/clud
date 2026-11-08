//! Oversized shell read guard: one wide `sed` range, or a whole-file `cat` (#2233).
//!
//! The read-hunt guard (`block_bad_cmd_hunt`) covers one call that names several
//! disjoint ranges. The other half of the `sed` habit is a *single* wide range:
//! `sed -n '120,720p' file` prints every line of a 600-line window straight into
//! the transcript, and each later request in that session re-sends all of it.
//! Measured over 24 h of local transcripts (2,903 tool calls, 291 MB of context
//! exposure): single-range slices that print at least 8 KiB are only 14 calls
//! but 164 kB of text and **4.8 % of all exposure**, because they land early in
//! long sessions.
//!
//! The same budget covers the other half of the habit, which no guard saw before:
//! `cat some_doc.md` prints a whole file and, unlike `sed`, `cat` has no range to
//! bound. Measured over the same 24 h and weighted by the requests that later
//! re-sent each result, whole-file `cat` of a source or doc file is the largest
//! unguarded read shape left (e.g. `cat docs/handoffs/<date>.md`, 12,292 B early
//! in a long session). `cat` is sized exactly from the file metadata, so the
//! decision costs no read.
//!
//! For exactly those calls the bounded reader is provably smaller: `code/slice.py`
//! stops at its `--max-bytes` cap (8 KiB default) and its footer names the line
//! to continue from. Same fixture file, same ranges, measured here:
//! `120,520p` 14,713 B -> 8,240 B and `280,646p` 12,311 B -> 8,228 B. Under the
//! cap the slice reader is *larger* than `sed` (it prefixes every line), so this
//! guard counts nothing below [`CAP_BYTES`] — the predicted printed size is
//! computed from the file itself, which makes the decision exact rather than
//! heuristic.
//!
//! Deliberately narrow:
//! * shell tools only, and only a command whose `sed` reads name **one** range
//!   (multi-range calls belong to the hunt guard, so no call is counted twice);
//! * the target must exist, carry a source extension and be at least
//!   [`MIN_FILE_BYTES`]; logs and data files are never counted;
//! * the range must predict at least [`CAP_BYTES`] of printed text;
//! * a redirect or `sed -i` is a write and passes untouched;
//! * a read through a pipe the stages *bound* (`head -N`, `cut -c1-M`) is sized
//!   with that bound applied instead of being waved through: `hunt_ranges`
//!   skips every piped segment, so `sed -n '1,700p' f | head -200` (200 full
//!   lines) and `cat a.md | head -300` reached the transcript unmeasured. A
//!   pipe through `tail`/`grep`/`sort`/`awk` has no predictable bound and is
//!   still left alone;
//! * a `cat` inside a subshell (`(cd dir; cat a.rs b.rs)`) is sized like any
//!   other, instead of being dropped by the `)` on its last path;
//! * past [`DEFAULT_LIMIT`] oversized reads in one session the next *distinct*
//!   one is refused ONCE with the exact `code/slice.py` call. A retry of the
//!   identical command passes, so the guard can never wedge a session;
//! * **fail open**: no session id, unreadable state, unreadable or huge file,
//!   or an unparsable command allows the call. This is a nudged behaviour, not
//!   a security boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::block_bad_cmd_hunt::{
    hunt_ranges, is_in_place_sed, is_source_file, parse_range, plausible_path, program,
    writes_file, Range,
};

/// Oversized single-range reads allowed per session before the next distinct
/// one is refused.
pub(super) const DEFAULT_LIMIT: u32 = 2;
/// A read arriving after this long starts a new count.
pub(super) const RESET_SECS: u64 = 60 * 60;
/// Files smaller than this are cheap to read with any tool; never counted.
pub(super) const MIN_FILE_BYTES: u64 = 8 * 1024;
/// `code/slice.py`'s default `--max-bytes`: at or above this the bounded reader
/// prints less than the raw range would.
pub(super) const CAP_BYTES: u64 = 8 * 1024;
/// Files larger than this are not scanned to predict the printed size.
const MAX_SCAN_BYTES: u64 = 4 * 1024 * 1024;
/// Floor for one file inside a whole-file `cat`: the *total* is what the guard
/// charges against [`CAP_BYTES`], so three 4 KiB docs are as expensive as one
/// 12 KiB file and must count the same.
const CAT_MIN_FILE_BYTES: u64 = 1024;
/// Per-call override, written into the command.
pub(super) const ALLOW_TOKEN: &str = "CLUD_ALLOW_SLICE_BUDGET=1";
/// Session-wide limit override; `0` disables the guard.
pub(super) const LIMIT_ENV: &str = "CLUD_SLICE_BUDGET_LIMIT";
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "read-slice-guard";
const HASH_DOMAIN: &str = "clud-read-slice-guard-v1\0";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const MAX_FILES: usize = 500;

/// One oversized ranged read: the file, its range, and the text it would print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Slice {
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub bytes: u64,
}

impl Slice {
    /// Stable identity for "this exact read", so a forced retry passes.
    fn signature(&self) -> String {
        let key = format!("{}\0{}\0{}", self.path, self.start, self.end);
        let digest = Sha256::digest(format!("{HASH_DOMAIN}{key}").as_bytes());
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The persisted per-session state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    #[serde(default)]
    pub slices: u32,
    #[serde(default)]
    pub last: u64,
    /// Signature -> refusal time. A retry of a refused read is allowed.
    #[serde(default)]
    pub refused: BTreeMap<String, u64>,
    /// Oversized whole-file `cat` reads, counted separately from ranged reads.
    #[serde(default)]
    pub cats: u32,
    /// Signature -> refusal time for whole-file reads.
    #[serde(default)]
    pub refused_cats: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    Deny { slice: Slice, count: u32 },
}

/// The decision table over injected facts only: whether to refuse, and the
/// state to persist. `limit == 0` disables the guard.
pub(super) fn decide(
    previous: Option<&State>,
    slice: &Slice,
    now: u64,
    limit: u32,
) -> (Verdict, State) {
    let mut state = previous.cloned().unwrap_or_default();
    if now.saturating_sub(state.last) > RESET_SECS {
        state = State::default();
    }
    let signature = slice.signature();
    state.slices = state.slices.saturating_add(1);
    state.last = now;
    let mut verdict = Verdict::Allow;
    if limit > 0 && state.slices > limit && !state.refused.contains_key(&signature) {
        state.refused.insert(signature, now);
        verdict = Verdict::Deny {
            slice: slice.clone(),
            count: state.slices,
        };
    }
    trim(&mut state, now);
    (verdict, state)
}

/// Drop expired refusals, then the oldest ones so one long session cannot grow
/// the state file without bound.
fn trim(state: &mut State, now: u64) {
    for refused in [&mut state.refused, &mut state.refused_cats] {
        refused.retain(|_, at| now.saturating_sub(*at) <= RESET_SECS);
        while refused.len() > 16 {
            let oldest = refused
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    refused.remove(&key);
                }
                None => break,
            }
        }
    }
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

/// A pipeline stage's bound on the text a read prints: `head -N` keeps the
/// first N lines, `cut -c1-M` keeps the first M characters of every line. No
/// caps means the read prints its whole range, exactly as an unpiped `sed` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Caps {
    lines: Option<u32>,
    cols: Option<u32>,
}

impl Caps {
    const fn unbounded() -> Self {
        Self {
            lines: None,
            cols: None,
        }
    }

    fn bounds(self) -> bool {
        self.lines.is_some() || self.cols.is_some()
    }

    /// Lines kept, or `usize::MAX` when no stage caps lines.
    fn keep(self) -> usize {
        self.lines.map_or(usize::MAX, |lines| {
            usize::try_from(lines).unwrap_or(usize::MAX)
        })
    }

    /// The width one line prints at, after the caps. `cut -c` counts characters
    /// and an uncut line prints its bytes, so both units are used for what they
    /// actually measure.
    fn width(self, line: &str) -> u64 {
        match self.cols {
            None => line.len() as u64,
            Some(cols) => line
                .chars()
                .count()
                .min(usize::try_from(cols).unwrap_or(usize::MAX)) as u64,
        }
    }
}

/// The caps the stages after a pipe place on a read, or `None` when any stage's
/// bound is unknown. Only `head -N` and `cut -c...` are modelled because both
/// are exact; `tail`, `sort`, `grep` and `awk` move or filter the text in ways
/// this guard cannot predict, so a read through them stays invisible.
fn pipe_caps(segment: &str) -> Option<Caps> {
    let mut caps = Caps::unbounded();
    let mut stages = segment.split('|').skip(1).peekable();
    stages.peek()?;
    for stage in stages {
        let tokens: Vec<&str> = stage.split_whitespace().collect();
        match program(tokens.first()?) {
            "head" => {
                let lines = head_lines(&tokens[1..])?;
                caps.lines = Some(caps.lines.map_or(lines, |seen| seen.min(lines)));
            }
            "cut" => {
                let cols = cut_cols(&tokens[1..])?;
                caps.cols = Some(caps.cols.map_or(cols, |seen| seen.min(cols)));
            }
            _ => return None,
        }
    }
    caps.bounds().then_some(caps)
}

/// `head`, `head -20`, `head -n 20`, `head -n20` -> the line count (bare `head`
/// keeps 10). `head -c BYTES` is not a line bound and is rejected.
fn head_lines(args: &[&str]) -> Option<u32> {
    let mut count = 10;
    let mut index = 0;
    while let Some(arg) = args.get(index).copied() {
        let value = if arg == "-n" {
            index += 1;
            *args.get(index)?
        } else {
            arg.strip_prefix('-')?
        };
        count = value.strip_prefix('n').unwrap_or(value).parse().ok()?;
        index += 1;
    }
    Some(count)
}

/// `cut -c1-350` / `cut -c 1-350` / `cut -c1,20-40` -> the widest column window
/// in characters. Any other `cut` form walks fields, not columns, and is
/// rejected.
fn cut_cols(args: &[&str]) -> Option<u32> {
    let spec = match args.first()?.strip_prefix("-c")? {
        "" => *args.get(1)?,
        rest => rest,
    };
    let mut widest = 0;
    for part in spec.split(',') {
        let part = part.trim_matches(|c| c == '"' || c == '\'');
        let width = match part.split_once('-') {
            Some((first, last)) => {
                let first: u32 = first.parse().ok()?;
                match last {
                    "" => u32::MAX,
                    last => last
                        .parse::<u32>()
                        .ok()?
                        .saturating_sub(first)
                        .saturating_add(1),
                }
            }
            None => 1,
        };
        widest = widest.max(width);
    }
    Some(widest)
}

/// The bytes a ranged read prints, after the pipeline's caps.
fn printed_bytes_capped(path: &Path, start: u32, end: u32, caps: Caps) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SCAN_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let first = usize::try_from(start).ok()?.saturating_sub(1);
    let take = usize::try_from(end.saturating_sub(start).saturating_add(1)).ok()?;
    text.lines().nth(first)?;
    let bytes: u64 = text
        .lines()
        .skip(first)
        .take(take)
        .take(caps.keep())
        .map(|line| caps.width(line) + 1)
        .sum();
    Some(bytes)
}

/// The ranged reads that sit in front of a *bounding* pipe, with the caps that
/// pipe places on them. [`hunt_ranges`] skips every piped segment because a
/// pipe may bound the text; when the stages name the bound the read is
/// sizeable, and `sed -n '1,700p' f | head -200` used to pass every guard.
fn piped_ranges(command: &str) -> Vec<(Range, Caps)> {
    let mut out = Vec::new();
    for segment in command.split([';', '&', '\n']) {
        let Some((head, _)) = segment.split_once('|') else {
            continue;
        };
        let tokens: Vec<&str> = head.split_whitespace().collect();
        if tokens.is_empty()
            || segment.contains('>')
            || is_in_place_sed(&tokens)
            || writes_file(&tokens)
        {
            continue;
        }
        if !tokens.iter().any(|token| program(token) == "sed") {
            continue;
        }
        let Some(caps) = pipe_caps(segment) else {
            continue;
        };
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
            out.push((Range { path, start, end }, caps));
        }
    }
    out
}

/// The oversized read this range is, after the caps, or `None` when it is cheap.
fn sized(range: &Range, caps: Caps, cwd: &Path) -> Option<Slice> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = resolve(&range.path, cwd, home.as_deref())?;
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() < MIN_FILE_BYTES {
        return None;
    }
    let bytes = printed_bytes_capped(&path, range.start, range.end, caps)?;
    if bytes < CAP_BYTES {
        return None;
    }
    Some(Slice {
        path: path.to_string_lossy().into_owned(),
        start: range.start,
        end: range.end,
        bytes,
    })
}

/// The oversized read this command is, or `None` when it does not qualify.
pub(super) fn detect(command: &str, cwd: &Path) -> Option<Slice> {
    let ranges = hunt_ranges(command);
    if ranges.len() == 1 {
        let range: Range = ranges.into_iter().next()?;
        return sized(&range, Caps::unbounded(), cwd);
    }
    if !ranges.is_empty() {
        return None;
    }
    let piped = piped_ranges(command);
    if piped.len() != 1 {
        return None;
    }
    let (range, caps) = piped.into_iter().next()?;
    sized(&range, caps, cwd)
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
fn prune(dir: &Path, now: SystemTime) {
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
    slice: &Slice,
    now: u64,
    limit: u32,
) -> Verdict {
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<State>(&text).ok());
    let (verdict, state) = decide(existing.as_ref(), slice, now, limit);
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
    let state_dir = crate::daemon::default_state_dir().ok()?;
    let now = now_secs();
    if let Some(slice) = detect(command, cwd) {
        return match check_at(&state_dir, session_id, &slice, now, limit) {
            Verdict::Allow => None,
            Verdict::Deny { slice, count } => Some(deny_message(&slice, count, limit)),
        };
    }
    let whole = detect_whole(command, cwd)?;
    match check_whole_at(&state_dir, session_id, &whole, now, limit) {
        CatVerdict::Allow => None,
        CatVerdict::Deny { whole, count } => Some(whole_message(&whole, count, limit)),
    }
}

pub(super) fn deny_message(slice: &Slice, count: u32, limit: u32) -> String {
    format!(
        "[clud read-slice-guard] this `sed` range would print {bytes} B into the transcript, \
         and every later request in this session re-sends it. This session has already made \
         {count} oversized ranged reads (limit {limit}). Read it in one bounded call instead: \
         `clud tool run code/slice.py {path} --lines {start}-{end}` — it caps the text at 8 KiB \
         and its footer names the line to continue from. Outline first when the file is \
         unfamiliar: `clud tool run code/outline.py -- {path}`. To run this call as written, \
         prefix it with `{ALLOW_TOKEN}`.",
        bytes = slice.bytes,
        path = slice.path,
        start = slice.start,
        end = slice.end,
    )
}

/// One whole-file read a command asks for: every source file it `cat`s and the
/// text that would land in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WholeRead {
    pub paths: Vec<String>,
    pub bytes: u64,
}

impl WholeRead {
    /// Stable identity for "this exact read", so a forced retry passes.
    fn signature(&self) -> String {
        let digest = Sha256::digest(format!("{HASH_DOMAIN}cat\0{}", self.paths.join("\0")));
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The first file named: the one the advice points at.
    fn first(&self) -> &str {
        self.paths.first().map_or("", String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CatVerdict {
    Allow,
    Deny { whole: WholeRead, count: u32 },
}

/// The whole-file decision table. Mirrors [`decide`] but counts a separate
/// habit, so a session that reads wide `sed` ranges still gets the full
/// allowance for whole-file `cat`.
pub(super) fn decide_whole(
    previous: Option<&State>,
    whole: &WholeRead,
    now: u64,
    limit: u32,
) -> (CatVerdict, State) {
    let mut state = previous.cloned().unwrap_or_default();
    if now.saturating_sub(state.last) > RESET_SECS {
        state = State::default();
    }
    let signature = whole.signature();
    state.cats = state.cats.saturating_add(1);
    state.last = now;
    let mut verdict = CatVerdict::Allow;
    if limit > 0 && state.cats > limit && !state.refused_cats.contains_key(&signature) {
        state.refused_cats.insert(signature, now);
        verdict = CatVerdict::Deny {
            whole: whole.clone(),
            count: state.cats,
        };
    }
    trim(&mut state, now);
    (verdict, state)
}

/// The whole-file read this command is, or `None` when it does not qualify.
///
/// Only `cat` segments count, only plain source/doc paths, and each file must
/// exist at [`CAT_MIN_FILE_BYTES`]..[`MAX_SCAN_BYTES`] with the segment total
/// reaching [`CAP_BYTES`]. Any segment with a pipe or a
/// redirect is bounded or a write and is skipped. A command the hunt guard owns
/// (two or more ranged reads) is left to it, so no call is counted twice.
pub(super) fn detect_whole(command: &str, cwd: &Path) -> Option<WholeRead> {
    if hunt_ranges(command).len() >= 2 {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut paths: Vec<String> = Vec::new();
    let mut bytes: u64 = 0;
    for segment in command.split([';', '&', '\n']) {
        let read = segment.split('|').next().unwrap_or(segment);
        let tokens: Vec<&str> = read.split_whitespace().collect();
        if tokens.is_empty() || segment.contains('>') || program(tokens[0]) != "cat" {
            continue;
        }
        // A `cat ... | head -N` prints N full lines, which can still be tens of
        // kilobytes; a stage whose bound is unknown skips the segment as before.
        let caps = if segment.contains('|') {
            let Some(caps) = pipe_caps(segment) else {
                continue;
            };
            caps
        } else {
            Caps::unbounded()
        };
        let mut segment_bytes: u64 = 0;
        let mut segment_paths: Vec<String> = Vec::new();
        let mut eligible = true;
        for token in &tokens[1..] {
            if token.starts_with('-') {
                continue;
            }
            // `(cd dir; cat a.rs b.rs)` closes the subshell on the last path,
            // which used to make the whole segment ineligible.
            let token = token
                .trim_matches(|c| c == '"' || c == '\'')
                .trim_end_matches(')');
            if !plausible_path(token) || !is_source_file(token) {
                eligible = false;
                break;
            }
            let Some(path) = resolve(token, cwd, home.as_deref()) else {
                eligible = false;
                break;
            };
            let Ok(meta) = std::fs::metadata(&path) else {
                eligible = false;
                break;
            };
            if !meta.is_file() || meta.len() < CAT_MIN_FILE_BYTES || meta.len() > MAX_SCAN_BYTES {
                eligible = false;
                break;
            }
            segment_bytes = segment_bytes.saturating_add(meta.len());
            segment_paths.push(path.to_string_lossy().into_owned());
        }
        if !eligible || segment_paths.is_empty() {
            continue;
        }
        if caps.bounds() {
            let Some(bounded) = capped_cat_bytes(&segment_paths, caps) else {
                continue;
            };
            segment_bytes = bounded;
        }
        bytes = bytes.saturating_add(segment_bytes);
        paths.extend(segment_paths);
    }
    (bytes >= CAP_BYTES && !paths.is_empty()).then_some(WholeRead { paths, bytes })
}

/// The bytes a `cat` of `paths` prints after the pipeline's caps.
fn capped_cat_bytes(paths: &[String], caps: Caps) -> Option<u64> {
    let mut remaining = caps.keep();
    let mut total = 0u64;
    for path in paths {
        let text = std::fs::read_to_string(path).ok()?;
        for line in text.lines() {
            if remaining == 0 {
                return Some(total);
            }
            total = total.saturating_add(caps.width(line) + 1);
            remaining -= 1;
        }
    }
    Some(total)
}

/// Compare-and-decide a whole-file read against the persisted session state.
/// Any filesystem error allows the call.
pub(super) fn check_whole_at(
    state_dir: &Path,
    session_id: &str,
    whole: &WholeRead,
    now: u64,
    limit: u32,
) -> CatVerdict {
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<State>(&text).ok());
    let (verdict, state) = decide_whole(existing.as_ref(), whole, now, limit);
    let Ok(bytes) = serde_json::to_vec(&state) else {
        return CatVerdict::Allow;
    };
    if existing.is_none() {
        if std::fs::create_dir_all(state_dir.join(DIR_NAME)).is_err() {
            return CatVerdict::Allow;
        }
        prune(&state_dir.join(DIR_NAME), SystemTime::now());
    }
    if std::fs::write(&path, bytes).is_err() {
        return CatVerdict::Allow;
    }
    verdict
}

pub(super) fn whole_message(whole: &WholeRead, count: u32, limit: u32) -> String {
    format!(
        "[clud read-cat-guard] this `cat` would print {bytes} B from {files} file(s) into the \
         transcript, and every later request in this session re-sends it. This session has \
         already made {count} oversized whole-file reads (limit {limit}). Read only what you \
         need: `clud tool run code/outline.py -- {path}` lists every definition and heading \
         with its line range, then `clud tool run code/slice.py {path} --lines A-B` prints that \
         range byte-capped with a footer naming the line to continue from. For a first look use \
         `head -n 80 {path}`. To run this call as written, prefix it with `{ALLOW_TOKEN}`.",
        bytes = whole.bytes,
        files = whole.paths.len(),
        path = whole.first(),
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_slice_tests.rs"]
mod tests;

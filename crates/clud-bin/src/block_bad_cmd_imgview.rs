//! Image-view budget (#2233).
//!
//! A screenshot read is not a text read. Measured on 24 h of local transcripts
//! (`reports/rows-2026-11-03.jsonl`): image `Read`s were 162 calls but **19.9 %
//! of all context exposure** (182 MB of the 916 MB re-sent), while every text
//! `Read` together was 2.7 %. One session opened the same scratchpad path 46
//! times — 45 distinct frames, ~190 KB of base64 each — and every frame stays
//! in the conversation for the rest of the session, so each later request pays
//! for all of them again.
//!
//! The cost is the *retained frames*, so this guard counts views per
//! `(session, image path)` and does two things:
//!
//! * past half the limit it rides an advisory note on the allow, naming the
//!   real cost and the cheaper capture (clip or scale the capture, crop the
//!   region that changed) — advice only, the read runs;
//! * past the limit it refuses **once** per file per session, then runs the
//!   retry. A refusal that cannot be repeated cannot brick an interactive
//!   loop, and a `Read` carries no command text to hang a per-call override on,
//!   so a permanent wall would have no escape at all.
//!
//! The per-path budget is not enough on its own, because the thing that costs
//! money is the *session's* retained frames, whatever their paths. Measured on
//! 24 h of transcripts (`reports/rows-2026-11-05.jsonl`): one audit session
//! returned **31 frames across 29 distinct paths** (~180 KB each), so the
//! per-path counter peaked at 2 and never asked once — the agent paid for all
//! 31 frames on every later request. So the same state also records each
//! distinct frame's bytes (a content hash, so a re-read or a copy cannot
//! inflate the count) and applies a session-wide budget: a note past
//! [`SESSION_NUDGE`] frames, one refusal past [`SESSION_LIMIT`]. Byte-identical
//! re-reads are **not** handled here — `block_bad_cmd_reread` already hashes
//! file bytes and refuses them.
//!
//! Deliberately narrow: `Read`-class tools only (a shell `cat x.png` is not
//! counted), image extensions only, existing files at least [`MIN_BYTES`].
//! **Fail open** on every missing fact: no session id, no state dir, a path
//! that cannot be stat-ed, a file that cannot be read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Distinct views of one image allowed before the single refusal.
pub(super) const DEFAULT_LIMIT: u32 = 12;
/// Quiet time after which the count for a path starts over: a fresh look an
/// hour later is a new task, not a poll.
pub(super) const RESET_SECS: u64 = 60 * 60;
/// Smaller images are cheap to retain; they are never counted.
pub(super) const MIN_BYTES: u64 = 8 * 1024;
/// Session-wide limit override; `0` disables the guard.
pub(super) const LIMIT_ENV: &str = "CLUD_IMAGE_READ_LIMIT";
/// Session-wide frame budget override; `0` keeps the per-path budget only.
pub(super) const SESSION_ENV: &str = "CLUD_IMAGE_SESSION_LIMIT";
/// Distinct frames in one session before the advisory note starts.
pub(super) const SESSION_NUDGE: u32 = 12;
/// Distinct frames in one session before the one session-wide refusal.
pub(super) const SESSION_LIMIT: u32 = 24;
/// Files above this size are not hashed: the read is allowed untouched rather
/// than paying a large synchronous read inside the hook.
pub(super) const MAX_HASH_BYTES: u64 = 8 * 1024 * 1024;
/// Distinct frames remembered per session. Above [`SESSION_LIMIT`] so the
/// budget is never under-counted; the oldest go first.
const MAX_FRAMES: usize = 48;
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "image-view-guard";
const HASH_DOMAIN: &str = "clud-image-view-guard-v1\0";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const MAX_FILES: usize = 200;
/// Image paths tracked per session; the least recently viewed are dropped.
const MAX_TRACKED: usize = 16;

/// Extensions a vision model decodes. Anything else is a text or data read and
/// belongs to `block_bad_cmd_reread`.
const IMAGE_EXTENSIONS: [&str; 9] = [
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "heic",
];

/// One image path's view count in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct View {
    pub count: u32,
    pub last: u64,
    /// The one refusal has been spent: the agent chose to keep looking.
    #[serde(default)]
    pub denied: bool,
}

/// What one image read contributes to the budget. A struct because the two
/// decisions already take the state, the clock and both limits.
#[derive(Debug, Clone, Copy)]
pub(super) struct Facts<'a> {
    pub path: &'a str,
    /// Content hash; `None` when the file could not be read or was too large.
    pub hash: Option<&'a str>,
    pub size: u64,
}

/// One image's bytes seen in a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Frame {
    /// When these bytes were first returned.
    pub first: u64,
    /// Their payload size on disk: what the frame costs on every later request.
    #[serde(default)]
    pub size: u64,
}

/// The persisted per-session map: resolved path -> view count, and image
/// content hash -> first sighting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    #[serde(default)]
    pub views: BTreeMap<String, View>,
    #[serde(default)]
    pub frames: BTreeMap<String, Frame>,
    /// The session-wide refusal has been spent.
    #[serde(default)]
    pub session_denied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    /// Allowed with an advisory note: the retained frames now cost real bytes.
    Nudge {
        count: u32,
    },
    /// The one refusal for this file in this session.
    DenyOnce {
        count: u32,
    },
    /// This session now retains `frames` frames between them `kb` KB.
    SessionNudge {
        frames: u32,
        kb: u64,
    },
    /// The one session-wide refusal.
    SessionDeny {
        frames: u32,
        kb: u64,
    },
}

/// The decision table over injected facts only.
///
/// `limit == 0` disables the guard, and a view arriving more than
/// [`RESET_SECS`] after the previous one starts a new count.
pub(super) fn decide(
    previous: Option<&State>,
    path: &str,
    now: u64,
    limit: u32,
) -> (Verdict, State) {
    let mut state = previous.cloned().unwrap_or_default();
    if limit == 0 {
        return (Verdict::Allow, state);
    }
    let view = state
        .views
        .get(path)
        .filter(|view| now.saturating_sub(view.last) <= RESET_SECS)
        .cloned()
        .unwrap_or(View {
            count: 0,
            last: now,
            denied: false,
        });
    let count = view.count.saturating_add(1);
    let verdict = if count > limit {
        if view.denied {
            Verdict::Allow
        } else {
            Verdict::DenyOnce { count }
        }
    } else if count > limit.div_ceil(2) {
        Verdict::Nudge { count }
    } else {
        Verdict::Allow
    };
    let denied = view.denied || matches!(verdict, Verdict::DenyOnce { .. });
    state.views.insert(
        path.to_string(),
        View {
            count,
            last: now,
            denied,
        },
    );
    trim(&mut state);
    (verdict, state)
}

/// The decision table including the session-wide frame budget.
///
/// A frame's bytes are recorded once per session (content hash, so a re-read or
/// a copy is not a new frame). Past [`SESSION_NUDGE`] distinct frames the allow
/// carries a note naming the retained payload; past [`SESSION_LIMIT`] the next
/// frame is refused **once**, then the retry runs and the refusal is spent.
/// The per-path [`decide`] budget still applies underneath and wins when it
/// fires first. `session_limit == 0` disables only this budget.
pub(super) fn decide_frame(
    previous: Option<&State>,
    facts: &Facts<'_>,
    now: u64,
    limit: u32,
    session_limit: u32,
) -> (Verdict, State) {
    let (path, hash, size) = (facts.path, facts.hash, facts.size);
    let mut state = previous.cloned().unwrap_or_default();
    if limit == 0 {
        return (Verdict::Allow, state);
    }
    if let Some(hash) = hash.filter(|hash| !hash.is_empty()) {
        let known = state
            .frames
            .get(hash)
            .is_some_and(|frame| now.saturating_sub(frame.first) <= RESET_SECS);
        if !known {
            state
                .frames
                .insert(hash.to_string(), Frame { first: now, size });
            trim_frames(&mut state);
        }
    }
    let frames = u32::try_from(state.frames.len()).unwrap_or(u32::MAX);
    let kb = state.frames.values().map(|frame| frame.size).sum::<u64>() / 1024;
    if session_limit > 0 && frames > session_limit && !state.session_denied {
        state.session_denied = true;
        return (Verdict::SessionDeny { frames, kb }, state);
    }
    let (verdict, state) = decide(Some(&state), path, now, limit);
    if !matches!(verdict, Verdict::Allow) {
        return (verdict, state);
    }
    if session_limit > 0 && frames > SESSION_NUDGE {
        return (Verdict::SessionNudge { frames, kb }, state);
    }
    (verdict, state)
}

/// Drop the oldest remembered image bytes so one long session cannot grow the
/// state file without bound.
fn trim_frames(state: &mut State) {
    if state.frames.len() <= MAX_FRAMES {
        return;
    }
    let mut entries: Vec<(String, u64)> = state
        .frames
        .iter()
        .map(|(hash, frame)| (hash.clone(), frame.first))
        .collect();
    entries.sort_by_key(|(_, first)| *first);
    for (hash, _) in entries.into_iter().take(state.frames.len() - MAX_FRAMES) {
        state.frames.remove(&hash);
    }
}

/// Drop the least recently viewed entries so one long session cannot grow the
/// state file without bound.
fn trim(state: &mut State) {
    if state.views.len() <= MAX_TRACKED {
        return;
    }
    let mut entries: Vec<(String, u64)> = state
        .views
        .iter()
        .map(|(path, view)| (path.clone(), view.last))
        .collect();
    entries.sort_by_key(|(_, last)| *last);
    for (path, _) in entries.into_iter().take(state.views.len() - MAX_TRACKED) {
        state.views.remove(&path);
    }
}

/// `Read` as Claude, Codex and the shell-shaped backends spell it.
fn applies_to(tool_name: &str) -> bool {
    matches!(
        super::block_bad_cmd_gate::tool_basename(tool_name).as_str(),
        "read" | "read_file" | "readfile"
    )
}

fn path_from(tool_input: Option<&Value>) -> Option<PathBuf> {
    let input = tool_input?;
    ["file_path", "path", "notebook_path"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Resolve the way the harness would: absolute as-is, `~/…` against home,
/// everything else against the tool's cwd, with `.` components dropped so
/// `a.png` and `./a.png` share one count.
fn resolve(path: &Path, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let resolved = if let Some(rest) = path.to_str().and_then(|text| text.strip_prefix("~/")) {
        match home {
            Some(home) => home.join(rest),
            None => path.to_path_buf(),
        }
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    resolved
        .components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect()
}

/// SHA-256 of the file's bytes, truncated to 16 hex bytes.
///
/// `None` when the file cannot be read or is above [`MAX_HASH_BYTES`]: the hook
/// does not pay a large synchronous read to answer a question it can skip.
fn content_hash(path: &Path, size: u64) -> Option<String> {
    if size > MAX_HASH_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&bytes);
    Some(
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// [`SESSION_ENV`] wins, else [`SESSION_LIMIT`]. Unparsable values fall back to
/// the default; `0` disables only the session-wide budget.
pub(super) fn session_limit_from(env: Option<&str>) -> u32 {
    env.and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(SESSION_LIMIT)
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

/// Record this view against the session's map in `state_dir` and return the
/// verdict. Every failure allows.
pub(super) fn record(
    state_dir: &Path,
    session_id: &str,
    facts: &Facts<'_>,
    now: u64,
    limit: u32,
    session_limit: u32,
) -> Verdict {
    if limit == 0 || session_id.is_empty() {
        return Verdict::Allow;
    }
    let file = session_file(state_dir, session_id);
    let existing = std::fs::read(&file).ok();
    let previous = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<State>(bytes).ok());
    let (verdict, state) = decide_frame(previous.as_ref(), facts, now, limit, session_limit);
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

/// What the hook should do with this call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Allow,
    Note(String),
    Deny(String),
}

/// Hook entry: allow, allow with a note, or the one refusal.
pub(super) fn outcome(
    tool_name: &str,
    tool_input: Option<&Value>,
    session_id: Option<&str>,
    cwd: &Path,
) -> Outcome {
    if !applies_to(tool_name) {
        return Outcome::Allow;
    }
    let limit = limit_from(std::env::var(LIMIT_ENV).ok().as_deref());
    if limit == 0 {
        return Outcome::Allow;
    }
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Outcome::Allow;
    };
    let Some(raw_path) = path_from(tool_input) else {
        return Outcome::Allow;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = resolve(&raw_path, cwd, home.as_deref());
    if !is_image(&path) {
        return Outcome::Allow;
    }
    let Some(size) = std::fs::metadata(&path)
        .ok()
        .filter(|meta| meta.is_file() && meta.len() >= MIN_BYTES)
        .map(|meta| meta.len())
    else {
        return Outcome::Allow;
    };
    let Ok(state_dir) = crate::daemon::default_state_dir() else {
        return Outcome::Allow;
    };
    let session_limit = session_limit_from(std::env::var(SESSION_ENV).ok().as_deref());
    let hash = content_hash(&path, size);
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let kb = size / 1024;
    let facts = Facts {
        path: &path.to_string_lossy(),
        hash: hash.as_deref(),
        size,
    };
    match record(
        &state_dir,
        session_id,
        &facts,
        now_secs(),
        limit,
        session_limit,
    ) {
        Verdict::Allow => Outcome::Allow,
        Verdict::Nudge { count } => Outcome::Note(nudge_message(&name, count, limit, kb)),
        Verdict::DenyOnce { count } => Outcome::Deny(deny_message(&name, count, limit, kb)),
        Verdict::SessionNudge { frames, kb } => {
            Outcome::Note(session_nudge_message(&name, frames, kb))
        }
        Verdict::SessionDeny { frames, kb } => {
            Outcome::Deny(session_deny_message(&name, frames, kb))
        }
    }
}

pub(super) fn nudge_message(name: &str, count: u32, limit: u32, kb: u64) -> String {
    let remaining = limit.saturating_sub(count);
    let closer = if remaining == 0 {
        "The next view of this file asks once before it runs.".to_string()
    } else {
        format!("{remaining} more views before clud asks once.")
    };
    format!(
        "[clud image-view] frame {count} of `{name}` this session. Every earlier frame is still \
         in the conversation: each is {kb} KB of image payload and every later request pays for \
         all of them again. For an iteration loop capture less — clip or scale the capture \
         (quarter-scale, region-clipped) or crop the area that changed — and view the full frame \
         only when the change should be visible. {closer}"
    )
}

/// The session-wide advisory: the count and the payload the agent keeps buying.
pub(super) fn session_nudge_message(name: &str, frames: u32, kb: u64) -> String {
    format!(
        "[clud image-view] this session now holds {frames} image frames (~{kb} KB of payload) and \
         `{name}` is the newest. Every frame stays in the conversation, so every later request \
         re-sends all of them. The per-file budget cannot see this: it counts one path, and these \
         frames are spread over many. Review what you already have before capturing more, capture \
         at reduced scale, and crop to the region that changed. Past {SESSION_LIMIT} frames the next \
         read is refused once. Set {SESSION_ENV}=0 to disable the session budget.",
    )
}

/// The session-wide refusal: once per session, then the retry runs.
pub(super) fn session_deny_message(name: &str, frames: u32, kb: u64) -> String {
    format!(
        "[clud image-view] `{name}` would be image frame {frames} of this session: ~{kb} KB of \
         image payload already retained, re-sent with every later request. A per-file count never \
         sees this because the frames sit on different paths. Stop and work from what you have — \
         compare the frames already read, or capture the smaller thing you actually need (clipped \
         or quarter-scale, the changed region only). This refusal happens once per session: repeat \
         the read to go ahead. Set {SESSION_ENV}=0 to disable the session budget, {LIMIT_ENV}=0 to \
         disable both.",
    )
}

pub(super) fn deny_message(name: &str, count: u32, limit: u32, kb: u64) -> String {
    format!(
        "[clud image-view] `{name}` has now been opened {count} times this session (limit {limit}). \
         The earlier frames are still in context — about {retained} KB of image payload the agent \
         already holds and every later request pays for again. Capture the smaller thing you \
         actually need: a clipped or quarter-scale screenshot, or a crop of the region that \
         changed, and read the image only when the change should be visible. This refusal happens \
         once per file per session: repeat the read to go ahead. Set {LIMIT_ENV}=0 to disable.",
        retained = u64::from(count.saturating_sub(1)).saturating_mul(kb),
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_imgview_tests.rs"]
mod tests;

//! Raw `git status` probe budget (#2233).
//!
//! `git status` is the fleet's most repeated repo probe and its most expensive
//! single shape once amplification is counted: the result is re-sent on every
//! later request of the session, and a status read lands early in the session,
//! where "later" is longest. Measured on 24 h of local transcripts
//! (`reports/rows-2026-11-07.jsonl`): git is the largest result category
//! (15.8 % of exposure) and bare `git status` alone was ~550 standalone calls
//! producing ~1.6 MB of text at a ~2.9 kB median — the same picture
//! `git/state.py` prints in one capped call.
//!
//! The bundled reader has been on `origin/main` since #2676 and the session
//! instruction already names it, but a doc never converts a habit: on the same
//! 24 h the fleet still typed raw `git status` in essentially every session
//! that probed a checkout at all. So the guard converts it at the hook: past
//! [`DEFAULT_LIMIT`] raw `git status` reads in one checkout, the next one is
//! refused ONCE with the exact `git/state.py` invocation. A retry of the same
//! command passes, so the guard can never wedge a session.
//!
//! Deliberately narrow:
//! * shell tools only, and only a command that is a single plain `git status`
//!   invocation — a chain (`;`, `&&`, `|`), a redirect, a substitution or an
//!   env prefix passes untouched, because those are someone's script;
//! * `--porcelain`, any pathspec (`status -- <path>`) and any flag outside a
//!   short allowlist pass untouched: machine-readable or targeted output is
//!   already small;
//! * the working directory (or `git -C` target) must be inside a checkout;
//! * **fail open**: no session id, unreadable state, unparsable command, or an
//!   unknown flag allows the call. This is a nudged behaviour, not a security
//!   boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Raw `git status` reads allowed in one checkout before the single refusal.
pub(super) const DEFAULT_LIMIT: u32 = 2;
/// A probe arriving after this long starts a new count: a later pass over the
/// checkout is not the same one.
pub(super) const RESET_SECS: u64 = 60 * 60;
/// Per-call override, written into the command: `CLUD_ALLOW_GIT_STATE=1`.
pub(super) const ALLOW_TOKEN: &str = "CLUD_ALLOW_GIT_STATE=1";
/// Session-wide limit override; `0` disables the guard.
pub(super) const LIMIT_ENV: &str = "CLUD_GIT_STATE_LIMIT";
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "git-state-guard";
/// The bundled reader this guard routes to.
pub(super) const TOOL: &str = "git/state.py";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const MAX_FILES: usize = 500;
/// Checkouts remembered per session; the oldest is forgotten first.
const MAX_REPOS: usize = 24;

/// Flags that keep `git status` a plain, tabular repo probe. Anything else —
/// porcelain, ignored lists, a pathspec — is either small or targeted and
/// passes.
const PLAIN_FLAGS: &[&str] = &[
    "-s",
    "--short",
    "-b",
    "--branch",
    "--long",
    "-sb",
    "-bs",
    "--no-renames",
    "--renames",
    "--untracked-files",
    "--untracked-files=normal",
    "-unormal",
];

/// The per-checkout counter the guard keeps for one session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Probe {
    pub calls: u32,
    pub refused: bool,
    pub last: u64,
}

/// The persisted per-session state: one entry per checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    pub repos: BTreeMap<String, Probe>,
}

/// What the guard says about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    Deny { root: PathBuf, count: u32 },
}

/// A raw `git status` probe, resolved to its checkout root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bare {
    pub root: PathBuf,
}

impl Bare {
    fn key(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
}

/// A command that is one plain `git status` invocation: no chain, no pipe, no
/// redirect, no substitution, no env prefix.
fn is_plain_clause(command: &str) -> bool {
    !command.contains([';', '\n', '&', '|', '`', '$', '>', '<', '('])
}

/// Split off `git`'s global options (`-C DIR`, `-c k=v`, `--no-pager`) and
/// return the remaining words, or `None` for a global option this guard does
/// not model (fail open).
fn after_global_options<'a>(words: &'a [&'a str]) -> Option<(&'a [&'a str], Option<PathBuf>)> {
    let mut index = 1;
    let mut dir: Option<PathBuf> = None;
    while let Some(word) = words.get(index) {
        match *word {
            "-C" => {
                let value = words.get(index + 1)?;
                dir = Some(PathBuf::from(value.trim_matches(&['\'', '"'][..])));
                index += 2;
            }
            "--no-pager" | "-P" => index += 1,
            _ if word.starts_with("-c") => index += if *word == "-c" { 2 } else { 1 },
            _ => break,
        }
    }
    Some((words.get(index..)?, dir))
}

/// The checkout root this probe runs in, if [`Bare::detect`] can prove one.
fn find_root(start: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(start).ok()?;
    for candidate in absolute.ancestors() {
        if candidate.join(".git").exists() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

/// The probe this command is, or `None` when it does not qualify.
pub(super) fn detect(command: &str, cwd: &Path) -> Option<Bare> {
    if !is_plain_clause(command) {
        return None;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    if words.first().is_some_and(|word| word.contains('=')) {
        return None;
    }
    if *words.first()? != "git" {
        return None;
    }
    let (rest, dir) = after_global_options(&words)?;
    if rest.first().map(|word| word.trim_matches(&['\'', '"'][..])) != Some("status") {
        return None;
    }
    for word in &rest[1..] {
        let bare = word.trim_matches(&['\'', '"'][..]);
        if !PLAIN_FLAGS.contains(&bare) {
            return None;
        }
    }
    let start = match dir {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => cwd.join(dir),
        None => cwd.to_path_buf(),
    };
    Some(Bare {
        root: find_root(&start)?,
    })
}

/// Fold one probe into the state: allow up to [`DEFAULT_LIMIT`] per checkout,
/// then refuse the next one once.
pub(super) fn decide(
    previous: Option<&State>,
    bare: &Bare,
    now: u64,
    limit: u32,
) -> (Verdict, State) {
    let mut state = previous.cloned().unwrap_or_default();
    let key = bare.key();
    let entry = state.repos.get(&key).copied();
    let stale = entry.is_some_and(|probe| now.saturating_sub(probe.last) > RESET_SECS);
    let mut probe = if stale {
        Probe::default()
    } else {
        entry.unwrap_or_default()
    };
    probe.calls = probe.calls.saturating_add(1);
    probe.last = now;
    let mut verdict = Verdict::Allow;
    if limit > 0 && probe.calls > limit && !probe.refused {
        probe.refused = true;
        verdict = Verdict::Deny {
            root: bare.root.clone(),
            count: probe.calls,
        };
    }
    state.repos.insert(key, probe);
    trim(&mut state);
    (verdict, state)
}

/// Keep the newest [`MAX_REPOS`] checkouts so one long session cannot grow the
/// state file without bound.
fn trim(state: &mut State) {
    while state.repos.len() > MAX_REPOS {
        let oldest = state
            .repos
            .iter()
            .min_by_key(|(_, probe)| probe.last)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => {
                state.repos.remove(&key);
            }
            None => break,
        }
    }
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
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    state_dir.join(DIR_NAME).join(format!("{safe}.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Drop state files no session has touched in [`MAX_AGE`].
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

/// Read-modify-write one session's state; any failure allows the call.
pub(super) fn check_at(
    state_dir: &Path,
    session_id: &str,
    bare: &Bare,
    now: u64,
    limit: u32,
) -> Verdict {
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<State>(&text).ok());
    let (verdict, state) = decide(existing.as_ref(), bare, now, limit);
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
    let bare = detect(command, cwd)?;
    let state_dir = crate::daemon::default_state_dir().ok()?;
    match check_at(&state_dir, session_id, &bare, now_secs(), limit) {
        Verdict::Allow => None,
        Verdict::Deny { root, count } => Some(deny_message(&root, count, limit)),
    }
}

/// The one message an agent sees after [`reason`] fired.
pub(super) fn deny_message(root: &Path, count: u32, limit: u32) -> String {
    format!(
        "[clud git-state] `git status` has run {count} times in {root} this session (limit \
         {limit}). Its output is kept in the conversation and paid for on every later request, \
         and it prints only part of the checkout's state. One bounded call gives all of it — \
         branch, upstream ahead/behind, changed paths, recent commits, stashes and worktrees, \
         every list capped with an explicit `+N more`:\n  cd {root} && \"$CLUD_EXE\" tool run \
         {TOOL}\nThis refusal happens once per checkout: re-run the command to go ahead, or set \
         {LIMIT_ENV}=0 to disable the guard.",
        root = root.display(),
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_gitstate_tests.rs"]
mod tests;

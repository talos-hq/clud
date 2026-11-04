//! Per-file shell read-hop guard (#2233).
//!
//! Agents `sed -n`/`cat`-slice a file they have not mapped, miss the symbol,
//! and slice it again. Measured on 24 h of local transcripts: of every byte a
//! shell read produced, **55% came from file-sessions that read the same file
//! four or more times**, and 562 file-sessions/day read one file eight or more
//! times (11.5 MB of results, each re-sent on every later request in that
//! session). The bytes per slice are small — the *hops* are the cost.
//!
//! So this guard counts shell read-hops per `(session, file)` rather than per
//! command: a different range of the same file is still a hop. `N + 1` hops on
//! one file are denied with a message naming `code/outline.py`, which prints
//! every definition with its line range (~2.7 KB for a 2,100-line file) so the
//! next read can be one targeted range instead of a fifth guess.
//!
//! Deliberately narrow:
//! * shell tools only (`sed`, `cat`, `head`, `tail`, `nl`, `less`, `bat`,
//!   `tac`, `awk`, `xxd`); `grep`/`git`/`gh` are searches, not reads, `wc` only
//!   counts lines, and `sed -i` / `cat > file` write instead of reading;
//! * only files that exist and are at least [`MIN_BYTES`] — hops on a small
//!   file cost nothing;
//! * the counter for a file expires after [`RESET_SECS`] of no hops, so
//!   revisiting a file later in the session is not a hunt;
//! * **fail open**: no session id, unreadable state, or an unparsable
//!   command allows the call. This is a nudged behaviour, not a boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Hops on one file that are still allowed; hop `DEFAULT_LIMIT + 1` is denied.
pub(super) const DEFAULT_LIMIT: u32 = 6;
/// A hop arriving more than this long after the file's previous hop starts a
/// new count: coming back to a file later is not the same hunt.
pub(super) const RESET_SECS: u64 = 15 * 60;
/// Files smaller than this are cheap to re-read; they are never counted.
pub(super) const MIN_BYTES: u64 = 8 * 1024;
/// Per-call override, written into the command: `CLUD_ALLOW_FILE_HOPS=1`.
pub(super) const ALLOW_TOKEN: &str = "CLUD_ALLOW_FILE_HOPS=1";
/// Session-wide limit override; `0` disables the guard.
pub(super) const LIMIT_ENV: &str = "CLUD_FILE_HOP_LIMIT";
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "file-hop-guard";
const HASH_DOMAIN: &str = "clud-file-hop-guard-v1\0";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const MAX_FILES: usize = 500;
/// Entries kept per session; the least recently used are dropped.
const MAX_TRACKED: usize = 32;

/// Shell verbs that read a file's contents into the transcript.
///
/// `wc` is deliberately absent: it emits a count per file, not content, so a
/// denial telling the agent to outline the file would not answer the question.
/// `tee`/`cp`/`mv` never read into context and are absent for the same reason.
const READ_VERBS: [&str; 10] = [
    "sed", "cat", "head", "tail", "nl", "less", "bat", "tac", "awk", "xxd",
];

/// Extensions worth mapping before reading. An unknown extension is not
/// counted, so data files and logs never trip the guard.
const SOURCE_EXTENSIONS: [&str; 27] = [
    "rs", "py", "toml", "md", "json", "jsonl", "ts", "tsx", "js", "jsx", "nix", "yml", "yaml", "c",
    "h", "cc", "cpp", "hpp", "go", "java", "kt", "rb", "sh", "css", "html", "sql", "proto",
];

/// One file's hop count in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Hop {
    pub count: u32,
    pub last: u64,
}

/// The persisted per-session map: resolved path -> hop count.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    #[serde(default)]
    pub files: BTreeMap<String, Hop>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    Deny { path: String, count: u32 },
}

/// The decision table over injected facts only: which hop target to deny, and
/// the state to persist. `limit == 0` disables the guard.
pub(super) fn decide(
    previous: Option<&State>,
    targets: &[String],
    now: u64,
    limit: u32,
) -> (Verdict, State) {
    let mut state = previous.cloned().unwrap_or_default();
    let mut verdict = Verdict::Allow;
    if limit > 0 {
        for path in targets {
            let count = state
                .files
                .get(path)
                .filter(|hop| now.saturating_sub(hop.last) <= RESET_SECS)
                .map_or(1, |hop| hop.count.saturating_add(1));
            state.files.insert(path.clone(), Hop { count, last: now });
            if count > limit && verdict == Verdict::Allow {
                verdict = Verdict::Deny {
                    path: path.clone(),
                    count,
                };
            }
        }
    }
    trim(&mut state);
    (verdict, state)
}

/// Drop the least recently hopped entries so one long session cannot grow the
/// state file without bound. Expired entries go first.
fn trim(state: &mut State) {
    if state.files.len() <= MAX_TRACKED {
        return;
    }
    let mut entries: Vec<(String, u64)> = state
        .files
        .iter()
        .map(|(path, hop)| (path.clone(), hop.last))
        .collect();
    entries.sort_by_key(|(_, last)| *last);
    for (path, _) in entries.into_iter().take(state.files.len() - MAX_TRACKED) {
        state.files.remove(&path);
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

/// Whether a token can name a file we would hop: no shell metacharacter, not
/// a flag, and a known source extension.
fn plausible_path(token: &str) -> bool {
    let token = token.trim_matches(|c| c == '"' || c == '\'');
    !token.is_empty()
        && !token.starts_with('-')
        && !token.contains(['>', '<', '*', '?', '$', '(', ')', '`', '=', '|', ';', '&'])
        && is_source_file(token)
}

fn is_read_verb(token: &str) -> bool {
    let base = token.rsplit('/').next().unwrap_or(token);
    READ_VERBS.contains(&base)
}

/// Candidate hop targets named by read verbs in a shell command.
///
/// Segments split on `;`, `&&`, `||`, `|` and newlines, so `sed -n 1,40p a.rs
/// && grep foo b.rs` counts only `a.rs`. A segment that names no read verb
/// contributes nothing, which keeps `git add a.rs` and `cargo build` out.
pub(super) fn hop_targets(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for segment in command.split([';', '|', '&', '\n']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        if !tokens.iter().any(|token| is_read_verb(token)) {
            continue;
        }
        // `sed -i` rewrites the file in place: nothing lands in the transcript,
        // so it is a write, not a hop. Counting it would deny the seventh
        // *edit* of a fix loop and answer it with "outline the file".
        if is_in_place_write(&tokens) {
            continue;
        }
        // A redirect ends the inputs: `cat > notes.md <<'EOF'` names a
        // destination, while `cat a.rs > b.py` still reads `a.rs`.
        for token in tokens.iter().take_while(|token| !is_redirect(token)) {
            if plausible_path(token) {
                let path = token.trim_matches(|c| c == '"' || c == '\'').to_string();
                if !out.contains(&path) {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// Whether this segment writes its named file in place (`sed -i`, `sed -ni`,
/// `sed --in-place[=SUFFIX]`). Only sed has an in-place mode among
/// [`READ_VERBS`], and an unrecognised flag cluster fails open (the segment is
/// then treated as a read).
fn is_in_place_write(tokens: &[&str]) -> bool {
    // Locate `sed` anywhere in the segment so `sudo sed -i` / `env X=1 sed -i`
    // are recognised too.
    let Some(at) = tokens
        .iter()
        .position(|token| token.rsplit('/').next() == Some("sed"))
    else {
        return false;
    };
    tokens[at + 1..].iter().any(|token| {
        if *token == "--in-place" || token.starts_with("--in-place=") {
            return true;
        }
        // Short cluster: `-i`, `-ibak`, `-ni`. `i` is sed's only in-place flag.
        token.starts_with('-') && !token.starts_with("--") && token.len() > 1 && token.contains('i')
    })
}

/// Whether this token starts an output redirect (`>`, `>>`, `2>`, `1>>`).
fn is_redirect(token: &str) -> bool {
    token.contains('>') && !token.contains('<')
}

/// Existing hop targets at or above [`MIN_BYTES`], resolved against `cwd`, in
/// the form they are stored and reported.
pub(super) fn tracked_targets(command: &str, cwd: &Path) -> Vec<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    hop_targets(command)
        .into_iter()
        .filter_map(|target| resolve_target(&target, cwd, home.as_deref()))
        .filter(|path| is_large_file(path))
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// Resolve a hop target the way the shell would: absolute paths as-is, `~/…`
/// against the home directory, everything else against the tool's cwd. The
/// stored form is always absolute, so `~/dev/x.rs` and `/home/u/dev/x.rs` in
/// the same session share one count.
fn resolve_target(target: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(expanded) = expand_tilde(target, home) {
        return Some(expanded);
    }
    let path = Path::new(target);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    Some(normalize(&resolved))
}

/// Drop `.` components so `a.rs`, `./a.rs` and `src/../a.rs` share one count.
fn normalize(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect()
}

/// Expand a leading `~/` against `home`. `~user` and a bare `~` are left to the
/// caller (they are uncommon in agent commands and must not be guessed).
fn expand_tilde(target: &str, home: Option<&Path>) -> Option<PathBuf> {
    let rest = target.strip_prefix("~/")?;
    Some(home?.join(rest))
}

fn is_large_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() >= MIN_BYTES)
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

/// Record these hops against the session's map in `state_dir` and return the
/// verdict. Every failure allows.
pub(super) fn check_at(
    state_dir: &Path,
    session_id: &str,
    targets: &[String],
    now: u64,
    limit: u32,
) -> Verdict {
    if limit == 0 || session_id.is_empty() || targets.is_empty() {
        return Verdict::Allow;
    }
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read(&path).ok();
    let previous = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<State>(bytes).ok());
    let (verdict, state) = decide(previous.as_ref(), targets, now, limit);
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

/// Hook entry: the denial reason for this call, or `None` to allow.
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
    let targets = tracked_targets(command, cwd);
    if targets.is_empty() {
        return None;
    }
    let state_dir = crate::daemon::default_state_dir().ok()?;
    match check_at(&state_dir, session_id, &targets, now_secs(), limit) {
        Verdict::Allow => None,
        Verdict::Deny { path, count } => Some(deny_message(&path, count, limit)),
    }
}

pub(super) fn deny_message(path: &str, count: u32, limit: u32) -> String {
    format!(
        "[clud file-hop-guard] this session has now read `{path}` through the shell {count} \
         times (limit {limit} per file). You are hunting a file you have not mapped: run \
         `clud tool run code/outline.py -- {path}` once (every definition and heading with its \
         line range, ~2.7 KB for 2,100 lines) and then read only the range you need in one \
         call. To allow one more slice deliberately, prefix the command with `{ALLOW_TOKEN}`."
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_hop_tests.rs"]
mod tests;

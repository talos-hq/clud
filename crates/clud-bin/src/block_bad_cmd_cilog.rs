//! Raw CI-log read budget (#2233).
//!
//! `gh run view --log-failed` prints every line of a failed job into the
//! transcript, and every later request in the session re-sends it. Measured on
//! 24 h of local transcripts (`reports/rows-2026-11-03.jsonl`): 244 `gh run
//! view` calls produced 283 kB of tool text but **10.8 MB of exposure (4.5 %)**
//! — the second largest unguarded category — and the widest single call was
//! 27.3 kB landing at request 200 of its session, so it alone cost 1 MB.
//!
//! clud already ships the bounded replacement: `github/ci_failure_report.py`
//! prints the failing diagnostic in at most 8 KiB, saves the full log to a
//! private artifact and names the `--artifact` continuation. Measured on the
//! same failed job here: `gh run view … --log-failed` = **119,184 B** versus
//! **995 B** from the bundled reader. A landed tool changes nothing on its own
//! — agents keep typing `gh` — so the guard converts the habit at the hook:
//! past [`DEFAULT_LIMIT`] distinct runs in one session the next distinct run's
//! raw log is refused ONCE with the exact `ci_failure_report.py` invocation.
//!
//! A refusal is keyed on `(repo, run)`, not on the command text: re-running the
//! same run's log in any shape passes immediately, so the guard cannot wedge a
//! session, and only a *different* run is ever asked about again.
//!
//! Deliberately narrow:
//! * shell tools only, and only `gh run view … --log`/`--log-failed` stages and
//!   `gh api repos/O/R/actions/runs/<id>/logs`;
//! * a command that redirects to a file is already keeping the log out of the
//!   transcript and is never counted;
//! * `--json`, `gh run list`, `gh run download` and job-level `gh api` paths
//!   (which carry no run id) are not counted;
//! * **fail open**: no session id, unreadable state, or an unparsable command
//!   allows the call. This is a nudged behaviour, not a security boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Distinct runs whose raw log may be read before the single refusal.
pub(super) const DEFAULT_LIMIT: u32 = 2;
/// A read arriving after this long starts a new count: a later investigation is
/// not the same one.
pub(super) const RESET_SECS: u64 = 60 * 60;
/// Per-call override, written into the command: `CLUD_ALLOW_CI_LOG=1`.
pub(super) const ALLOW_TOKEN: &str = "CLUD_ALLOW_CI_LOG=1";
/// Session-wide limit override; `0` disables the guard.
pub(super) const LIMIT_ENV: &str = "CLUD_CI_LOG_LIMIT";
/// Directory under the clud state dir.
pub(super) const DIR_NAME: &str = "ci-log-guard";
/// The bundled bounded reader this guard routes to.
pub(super) const TOOL: &str = "github/ci_failure_report.py";
const HASH_DOMAIN: &str = "clud-ci-log-guard-v1\0";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const MAX_FILES: usize = 500;
/// Distinct runs remembered per session.
const MAX_RUNS: usize = 24;

/// A raw CI log read: which run (and job, when the command names one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RawLog {
    pub repo: Option<String>,
    pub run: Option<String>,
    pub job: Option<String>,
}

impl RawLog {
    /// Stable identity for "this run's log", so a retry in any shape passes.
    pub(super) fn signature(&self) -> String {
        format!(
            "{}\0{}",
            self.repo.as_deref().unwrap_or("-"),
            self.run.as_deref().unwrap_or("-")
        )
    }
}

/// The persisted per-session state: which runs were read raw, most recently
/// used last.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    #[serde(default)]
    pub runs: BTreeMap<String, u64>,
    #[serde(default)]
    pub last: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    /// Allowed with an advisory note: naming the bounded reader one call early.
    Nudge {
        count: u32,
    },
    /// The one refusal for this run in this session.
    Deny {
        count: u32,
    },
}

/// What the hook should do with this call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Allow,
    Note(String),
    Deny(String),
}

/// The decision table over injected facts only.
///
/// The count is *distinct runs*, so re-reading one run's log never inflates it
/// (the repeat guards own that) and the first refusal of a run is spent for
/// good: retrying the same run in any shape passes. `limit == 0` disables the
/// guard, and a read arriving more than [`RESET_SECS`] after the previous one
/// starts a new count.
pub(super) fn decide(
    previous: Option<&State>,
    log: &RawLog,
    now: u64,
    limit: u32,
) -> (Verdict, State) {
    let mut state = previous.cloned().unwrap_or_default();
    if limit == 0 {
        return (Verdict::Allow, state);
    }
    if now.saturating_sub(state.last) > RESET_SECS {
        state = State::default();
    }
    state.last = now;
    // A repeat of a run already read is a repeat, not a new read: the repeat
    // guards own that shape, so it neither counts nor is refused.
    if state.runs.insert(log.signature(), now).is_some() {
        return (Verdict::Allow, state);
    }
    let count = u32::try_from(state.runs.len()).unwrap_or(u32::MAX);
    let verdict = if count > limit {
        Verdict::Deny { count }
    } else if count > limit.div_ceil(2) {
        Verdict::Nudge { count }
    } else {
        Verdict::Allow
    };
    trim(&mut state, now);
    (verdict, state)
}

/// Keep the most recently read runs so one long session cannot grow the state
/// file without bound.
fn trim(state: &mut State, now: u64) {
    state
        .runs
        .retain(|_, at| now.saturating_sub(*at) <= RESET_SECS);
    while state.runs.len() > MAX_RUNS {
        let oldest = state
            .runs
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => {
                state.runs.remove(&key);
            }
            None => break,
        }
    }
}

fn program(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Whether the command writes its output to a file. `2>&1` / `>&2` are file
/// descriptor duplication, not redirects, and do not exempt the command.
fn redirects_to_file(command: &str) -> bool {
    let bytes = command.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'>' {
            continue;
        }
        let next = bytes.get(index + 1).copied();
        if next == Some(b'&') {
            continue;
        }
        return true;
    }
    false
}

fn flag_value(args: &[&str], flag: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(token) = iter.next() {
        let token = token.trim_matches(|c| c == '"' || c == '\'');
        if token == flag {
            return iter
                .next()
                .map(|value| value.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
        if let Some(value) = token.strip_prefix(&format!("{flag}=")) {
            return Some(value.to_string());
        }
    }
    None
}

fn digits(token: &str) -> Option<&str> {
    (!token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit())).then_some(token)
}

/// Flags whose next token is a value, not a positional.
const VALUE_FLAGS: [&str; 8] = [
    "--job",
    "-j",
    "--attempt",
    "-R",
    "--repo",
    "--workflow",
    "-w",
    "--branch",
];

/// Advance past a flag's value. Returns whether this token was consumed as a
/// flag or as a value, so only real positionals are examined.
fn take_flag_value(skip_value: &mut bool, token: &str) -> bool {
    if *skip_value {
        *skip_value = false;
        return true;
    }
    if VALUE_FLAGS.contains(&token) {
        *skip_value = true;
        return true;
    }
    token.starts_with('-')
}

/// The run id is the first bare positional after `view`; flag values are
/// skipped so `--job 125931081356` is never mistaken for the run.
fn run_positional(args: &[&str]) -> Option<String> {
    let mut skip_value = false;
    for token in args {
        if take_flag_value(&mut skip_value, token) {
            continue;
        }
        let clean = token.trim_matches(|c| c == '"' || c == '\'');
        if digits(clean).is_some() {
            return Some(clean.to_string());
        }
    }
    None
}

/// `gh run view … --log[-failed]` and `gh api repos/O/R/actions/runs/<id>/logs`.
fn parse_gh(args: &[&str]) -> Option<RawLog> {
    match args.first().copied()? {
        "run" if args.get(1).copied() == Some("view") => {
            if !args
                .iter()
                .any(|token| matches!(*token, "--log" | "--log-failed"))
            {
                return None;
            }
            let run = run_positional(&args[2..]);
            Some(RawLog {
                repo: flag_value(args, "-R").or_else(|| flag_value(args, "--repo")),
                run,
                job: flag_value(args, "--job"),
            })
        }
        "api" => args.iter().find_map(|token| parse_api_path(token)),
        _ => None,
    }
}

/// A run-level Actions log API path. Job-level paths carry no run id, so they
/// are left alone rather than answered with a half-built replacement.
fn parse_api_path(token: &str) -> Option<RawLog> {
    let clean = token.trim_matches(|c| c == '"' || c == '\'');
    let rest = clean.strip_suffix("/logs")?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 6 || parts[0] != "repos" || parts[3] != "actions" || parts[4] != "runs" {
        return None;
    }
    Some(RawLog {
        repo: Some(format!("{}/{}", parts[1], parts[2])),
        run: Some(parts[5].to_string()),
        job: None,
    })
}

/// The raw log read this command performs, or `None`. Segments split on `;`,
/// `&` and newlines so a stage inside a pipeline is still visible.
pub(super) fn detect(command: &str) -> Option<RawLog> {
    if redirects_to_file(command) {
        return None;
    }
    command.split([';', '&', '\n']).find_map(|segment| {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let at = tokens.iter().position(|token| program(token) == "gh")?;
        parse_gh(&tokens[at + 1..])
    })
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

/// Compare-and-decide against the persisted session state, then store the new
/// one. Any filesystem error allows the call.
pub(super) fn check_at(
    state_dir: &Path,
    session_id: &str,
    log: &RawLog,
    now: u64,
    limit: u32,
) -> Verdict {
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read(&path).ok();
    let previous = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<State>(bytes).ok());
    let (verdict, state) = decide(previous.as_ref(), log, now, limit);
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

/// Hook entry: allow, allow with a note, or the one refusal.
pub(super) fn outcome(
    tool_name: &str,
    tool_input: Option<&Value>,
    session_id: Option<&str>,
) -> Outcome {
    if !shell_tool(tool_name) {
        return Outcome::Allow;
    }
    let limit = limit_from(std::env::var(LIMIT_ENV).ok().as_deref());
    if limit == 0 {
        return Outcome::Allow;
    }
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Outcome::Allow;
    };
    let Some(command) = tool_input
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)
    else {
        return Outcome::Allow;
    };
    if opts_out(command) {
        return Outcome::Allow;
    }
    let Some(log) = detect(command) else {
        return Outcome::Allow;
    };
    let Ok(state_dir) = crate::daemon::default_state_dir() else {
        return Outcome::Allow;
    };
    match check_at(&state_dir, session_id, &log, now_secs(), limit) {
        Verdict::Allow => Outcome::Allow,
        Verdict::Nudge { count } => Outcome::Note(nudge_message(&log, count, limit)),
        Verdict::Deny { count } => Outcome::Deny(deny_message(&log, count, limit)),
    }
}

/// The exact bounded command for the run the agent just named.
pub(super) fn replacement(log: &RawLog) -> String {
    let repo = log.repo.as_deref().unwrap_or("OWNER/REPO");
    let run = log.run.as_deref().unwrap_or("<run-id>");
    let job = log.job.as_deref().unwrap_or("<job-id>");
    format!("\"$CLUD_EXE\" tool run {TOOL} -- --repo {repo} --run {run} --job {job}")
}

/// How to find the job id when the command did not name one.
fn job_hint(log: &RawLog) -> String {
    if log.job.is_some() {
        return String::new();
    }
    let repo = log.repo.as_deref().unwrap_or("OWNER/REPO");
    let run = log.run.as_deref().unwrap_or("<run-id>");
    format!(
        "\nJob id: `gh run view {run} -R {repo} --json jobs -q \
         '.jobs[]|select(.conclusion==\"failure\")|[.databaseId,.name]'`"
    )
}

fn target(log: &RawLog) -> String {
    format!(
        "{} run {}",
        log.repo.as_deref().unwrap_or("this repo"),
        log.run.as_deref().unwrap_or("(unknown)")
    )
}

pub(super) fn nudge_message(log: &RawLog, count: u32, limit: u32) -> String {
    let remaining = limit.saturating_sub(count);
    let closer = if remaining == 0 {
        "The next distinct run's raw log asks once before it runs.".to_string()
    } else {
        format!("{remaining} more raw log reads run freely before the next distinct run asks.")
    };
    format!(
        "[clud ci-log] raw CI log read #{count} this session ({target}). `gh run view --log` \
         prints every line of the job into the transcript and every later request re-sends it. \
         The bundled reader prints the failing diagnostic in at most 8 KiB and saves the full log \
         to a private file: {replacement} {closer}",
        target = target(log),
        replacement = replacement(log),
    )
}

pub(super) fn deny_message(log: &RawLog, count: u32, limit: u32) -> String {
    format!(
        "[clud ci-log] {target} has had {count} raw log reads this session (limit {limit}). The \
         job's whole log is retained in the conversation and paid for on every later request — \
         measured on this host, one failed job was 119,184 B raw versus 995 B from the bounded \
         reader, which also names the artifact to grep locally. Use:\n  {replacement}{hint}\n\
         This refusal happens once per run: re-run the read to go ahead, or set {LIMIT_ENV}=0 to \
         disable the guard.",
        target = target(log),
        replacement = replacement(log),
        hint = job_hint(log),
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_cilog_tests.rs"]
mod tests;

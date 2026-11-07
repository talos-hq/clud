//! Repeated Stop-hook continuations (#1597, #2233).
//!
//! A `/goal` condition the sandbox cannot satisfy keeps the harness's Stop
//! evaluator re-prompting: the agent answers, the judge refuses the stop, the
//! agent answers again. The harness documents its own breakers (an
//! "impossible" verdict, a no-progress stop, a cap of 8 consecutive blocks) and
//! in practice they do not always fire — measured over 24 h of local
//! transcripts, 658 Stop-hook refusals landed in six sessions, 440 of them in
//! a single session whose judge kept repeating the same unmet pre-condition
//! ("still blocked on your sign-in"). Every refusal costs one full request at
//! the session's live context (200 kB of cache-read per request in that
//! session, ~88 M tokens across the 440 rounds) and appends another verdict —
//! 1.8 MB of judge text in a day — to a history that every later request
//! re-sends. None of the 440 rounds could ever succeed without the human.
//!
//! So clud does two things, both bounded, both fail-open:
//!
//! * when the same final message ends [`REPEATS`] consecutive hook-driven
//!   continuations, the user is told once how to break the loop;
//! * past [`DEFAULT_BUDGET`] hook-driven continuations in one session — far
//!   beyond the harness's documented cap of 8 — clud ends the turn itself with
//!   one `{"continue": false, "stopReason": …}` whose text names the exact
//!   ways to continue. That is the only lever a Stop hook has over a loop the
//!   harness owns, and it is emitted once per session, so a harness that
//!   ignores `continue: false` degrades to a single line rather than a new
//!   loop.
//!
//! A user-driven stop (the human typed something) resets the whole state, so
//! `CLUD_STOP_LOOP_BUDGET` and a fresh prompt both restore normal behaviour.
//! A budget of `0` disables the halt entirely.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Identical consecutive continuations before the user is told.
pub const REPEATS: u32 = 2;

/// Hook-driven continuations in one session before clud ends the turn.
///
/// The harness caps itself at 8; a run that passes that is already wedged, and
/// the measured wedges ran to 26, 61, 95 and 440 rounds.
pub const DEFAULT_BUDGET: u32 = 12;

/// Env: hook-driven continuations allowed in one session. `0` disables.
pub const BUDGET_ENV: &str = "CLUD_STOP_LOOP_BUDGET";

/// What the user sees once a repeat is detected.
pub const MESSAGE: &str = "clud: the agent has given the same answer to consecutive Stop-hook \
     continuations. If a /goal condition cannot be satisfied in this sandbox (a refused path or \
     command), resolve that step yourself, relax the condition, or run /goal clear.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    pub hash: u64,
    pub count: u32,
    /// Every hook-driven continuation in this session, identical or not.
    pub total: u32,
    /// The halt was already emitted; never emit it twice.
    pub halted: bool,
}

/// The next state for one Stop event, and whether to tell the user now.
/// `message` is `None` when the stop was not hook-driven: that resets.
#[must_use]
pub fn step(previous: State, message: Option<&str>) -> (State, bool) {
    let Some(text) = message.map(str::trim).filter(|t| !t.is_empty()) else {
        return (State::default(), false);
    };
    let hash = fnv1a(text);
    let count = if previous.count > 0 && previous.hash == hash {
        previous.count + 1
    } else {
        1
    };
    (
        State {
            hash,
            count,
            total: previous.total.saturating_add(1),
            halted: previous.halted,
        },
        count == REPEATS,
    )
}

/// The `stopReason` for a session that has burned its budget; bounded to one
/// line and naming every way out.
#[must_use]
pub fn halt_reason(total: u32, budget: u32) -> String {
    format!(
        "clud: {total} Stop-hook continuations in one session and the condition still is not \
         satisfied. Ending this turn so the judge loop stops re-sending the whole session. \
         Continue with `clud --claude resume`, `/goal clear`, a goal this sandbox can satisfy, \
         or {BUDGET_ENV}=<n> ({budget} here) to allow more rounds."
    )
}

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The configured budget: `0` disables, an unparsable value falls back to
/// [`DEFAULT_BUDGET`].
#[must_use]
pub fn budget_from(value: Option<&str>) -> u32 {
    value
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_BUDGET)
}

/// Handle one Stop payload; prints a `systemMessage` the first time a repeat
/// is seen and the single halt decision once the budget is spent. Best
/// effort: any failure is silent.
/// `SessionEnd` drops the session's state file.
pub fn observe(event: &str, stdin: &str) {
    if event != "Stop" && event != "SessionEnd" {
        return;
    }
    let Ok(payload) = serde_json::from_str::<Value>(stdin) else {
        return;
    };
    let Ok(state_dir) = crate::daemon::default_state_dir() else {
        return;
    };
    if event == "SessionEnd" {
        if let Some(session) = session_of(&payload) {
            let _ = std::fs::remove_file(state_path(&state_dir, session));
        }
        return;
    }
    let budget = budget_from(std::env::var(BUDGET_ENV).ok().as_deref());
    if let Some(line) = observe_in_with_budget(&state_dir, &payload, budget) {
        println!("{line}");
    }
}

/// The payload's session id, when it is safe to use as a file name.
fn session_of(payload: &Value) -> Option<&str> {
    let session = payload.get("session_id")?.as_str()?;
    let safe = !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    safe.then_some(session)
}

/// [`observe`] against `state_dir` with [`DEFAULT_BUDGET`].
pub fn observe_in(state_dir: &Path, payload: &Value) -> Option<String> {
    observe_in_with_budget(state_dir, payload, DEFAULT_BUDGET)
}

/// [`observe`] against `state_dir` and an explicit budget; returns the line to
/// print, if any.
pub fn observe_in_with_budget(state_dir: &Path, payload: &Value, budget: u32) -> Option<String> {
    let session = session_of(payload)?;
    let message = (payload.get("stop_hook_active").and_then(Value::as_bool) == Some(true))
        .then(|| {
            payload
                .get("last_assistant_message")
                .and_then(Value::as_str)
        })
        .flatten();
    let path = state_path(state_dir, session);
    let (mut next, tell) = step(load(&path), message);
    if budget > 0 && !next.halted && next.total >= budget {
        next.halted = true;
        store(&path, next);
        return Some(
            serde_json::json!({
                "continue": false,
                "stopReason": halt_reason(next.total, budget),
            })
            .to_string(),
        );
    }
    store(&path, next);
    tell.then(|| serde_json::json!({ "systemMessage": MESSAGE }).to_string())
}

fn state_path(state_dir: &Path, session: &str) -> PathBuf {
    state_dir
        .join("stop-repeat")
        .join(format!("{session}.json"))
}

fn load(path: &Path) -> State {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|v| State {
            hash: v["hash"].as_u64().unwrap_or(0),
            count: v["count"]
                .as_u64()
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(0),
            total: v["total"]
                .as_u64()
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(0),
            halted: v["halted"].as_bool().unwrap_or(false),
        })
        .unwrap_or_default()
}

fn store(path: &Path, state: State) {
    if state == State::default() {
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = serde_json::json!({
        "hash": state.hash,
        "count": state.count,
        "total": state.total,
        "halted": state.halted,
    });
    let _ = std::fs::write(path, body.to_string());
}

#[cfg(test)]
#[path = "stop_repeat_tests.rs"]
mod tests;

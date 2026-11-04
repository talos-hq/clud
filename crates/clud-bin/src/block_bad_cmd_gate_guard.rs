//! Direct lint/test nudge (#2603). No persisted guard state: replay the harness
//! transcript in memory. Missing, oversized, malformed or unsupported history
//! fails open, as do parser errors and unknown shell shapes. Like the read-hunt
//! guard, identities are domain-separated hashes and retries always pass.
use std::collections::BTreeSet;
use std::io::Read;
use std::time::SystemTime;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::HookPayloadView;
use crate::shell_ast::{self, BaseEnv, Dialect};

const RESET_SECS: u64 = 60 * 60;
const MAX_HISTORY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 32 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Silent,
    Advisory(String),
    Deny(String),
}

#[derive(Default)]
struct State {
    seen: BTreeSet<String>,
    last: u64,
    denied: bool,
}

impl State {
    fn observe(&mut self, signature: String, lane: &str, now: u64) -> Verdict {
        if now.saturating_sub(self.last) > RESET_SECS {
            *self = Self::default();
        }
        self.last = now;
        if self.denied || !self.seen.insert(signature) {
            return Verdict::Silent;
        }
        if self.seen.len() == 1 {
            Verdict::Advisory(format!("prefer: clud gate {lane}"))
        } else {
            self.denied = true;
            Verdict::Deny(format!("clud gate {lane}"))
        }
    }
}

fn bypass(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|word| matches!(word, "CLUD_GATE_BYPASS=1" | "CLUD_READ=1"))
}

/// Only literal, top-level simple commands; shells, expansions and heredocs
/// deliberately escape this behavioural nudge. Quoted prose is never scanned.
fn detect(command: &str) -> Option<(String, &'static str)> {
    if command.len() > MAX_COMMAND_BYTES || command.contains("clud gate") || bypass(command) {
        return None;
    }
    let script = shell_ast::parse(command, Dialect::Posix, &BaseEnv::new("."));
    if script.has_error || script.depth_capped || !script.constructs.is_empty() {
        return None;
    }
    let mut shapes = Vec::new();
    let (mut lint, mut test) = (false, false);
    for call in script.invocations {
        if call.depth != 0 || call.heredoc.is_some() || call.in_error {
            return None;
        }
        let words: Vec<_> = call
            .words
            .iter()
            .map(shell_ast::Value::as_literal)
            .collect::<Option<_>>()?;
        if call.env.iter().any(|(key, value)| {
            matches!(key.as_str(), "CLUD_GATE_BYPASS" | "CLUD_READ")
                && value.as_literal() == Some("1")
        }) {
            return None;
        }
        let words = words.strip_prefix(&["soldr"]).unwrap_or(&words);
        let Some((head, lane)) = runner(words) else {
            continue;
        };
        // A quoted command name/subcommand is an unknown shape, not a match.
        if call
            .raw
            .iter()
            .take(head + usize::from(call.program().as_deref() == Some("soldr")))
            .any(|word| word.contains(['\'', '"', '$', '`', '\\']))
        {
            return None;
        }
        lint |= lane == "lint";
        test |= lane == "test";
        let mut flags: Vec<_> = words[head..]
            .iter()
            .filter(|word| word.starts_with('-'))
            .map(|word| word.split('=').next().unwrap_or(word))
            .collect();
        flags.sort_unstable();
        flags.dedup();
        shapes.push(format!("{}\0{}", words[..head].join(" "), flags.join(" ")));
    }
    if shapes.is_empty() {
        return None;
    }
    let lane = match (lint, test) {
        (true, true) => "both",
        (true, false) => "lint",
        _ => "test",
    };
    let digest = Sha256::digest(format!("clud-gate-guard-v1\0{}", shapes.join("\n")));
    let signature = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some((signature, lane))
}

fn runner(words: &[&str]) -> Option<(usize, &'static str)> {
    match words {
        ["cargo", "test", ..] => Some((2, "test")),
        ["cargo", "nextest", "run", ..] => Some((3, "test")),
        ["cargo", "clippy", ..] => Some((2, "lint")),
        ["cargo", "fmt", rest @ ..] if rest.contains(&"--check") => Some((2, "lint")),
        ["cargo", "xtask", "lint", ..] => Some((3, "lint")),
        ["ruff", "check", ..] => Some((2, "lint")),
        ["eslint" | "./lint", ..] => Some((1, "lint")),
        ["pytest" | "vitest", ..] => Some((1, "test")),
        ["npm" | "pnpm" | "yarn", "test", ..] => Some((2, "test")),
        _ => None,
    }
}

fn timestamp(value: &Value) -> Option<u64> {
    let text = value.as_str()?;
    // Claude emits UTC with optional fractional seconds. Unknown formats allow.
    let suffix = text.get(19..)?;
    if suffix != "Z"
        && !(suffix.len() > 2
            && suffix.starts_with('.')
            && suffix.ends_with('Z')
            && suffix[1..suffix.len() - 1]
                .bytes()
                .all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    let seconds = crate::gh_broker::collection::parse_ts(&format!("{}Z", text.get(..19)?))?;
    seconds.try_into().ok()
}

fn replay(text: &str, session: &str, current_id: Option<&str>, now: u64) -> Option<State> {
    let mut state = State::default();
    let mut ids = BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).ok()?;
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if record.get("type")?.as_str()? != "assistant" {
            continue;
        }
        if record.get("sessionId")?.as_str()? != session {
            continue;
        }
        let content = record.get("message")?.get("content")?.as_array()?;
        for block in content {
            if block.get("type")?.as_str()? != "tool_use" || block.get("name")?.as_str()? != "Bash"
            {
                continue;
            }
            let id = block.get("id")?.as_str()?;
            if Some(id) == current_id || !ids.insert(id.to_owned()) {
                continue;
            }
            let command = block.get("input")?.get("command")?.as_str()?;
            let Some((signature, lane)) = detect(command) else {
                continue;
            };
            let at = timestamp(record.get("timestamp")?)?;
            if at > now || at < state.last {
                return None;
            }
            state.observe(signature, lane, at);
        }
    }
    Some(state)
}

fn check(
    payload: &HookPayloadView,
    raw: &Value,
    env: impl Fn(&str) -> Option<String>,
    now: u64,
) -> Option<Verdict> {
    if payload.tool_name != "Bash" {
        return None;
    }
    for name in ["CLUD_READ", "CLUD_GATE_BYPASS"] {
        if env(name).as_deref() == Some("1") {
            return None;
        }
    }
    let (signature, lane) = detect(payload.tool_input.as_ref()?.get("command")?.as_str()?)?;
    // Resolve only: never execute clud (or any other process) in this path.
    crate::shim_resolve::which("clud", &env("PATH")?)?;
    let session = payload.session_id.as_deref().filter(|id| !id.is_empty())?;
    let path = payload.transcript_path.as_deref()?;
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_HISTORY_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_HISTORY_BYTES {
        return None;
    }
    let current_id = raw.get("tool_use_id").and_then(Value::as_str);
    let mut state = replay(&text, session, current_id, now)?;
    Some(state.observe(signature, lane, now))
}

pub(super) fn verdict(payload: &HookPayloadView, raw: &Value) -> Verdict {
    std::panic::catch_unwind(|| {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs();
        check(payload, raw, |name| std::env::var(name).ok(), now)
    })
    .ok()
    .flatten()
    .unwrap_or(Verdict::Silent)
}

#[cfg(test)]
#[path = "block_bad_cmd_gate_guard_tests.rs"]
mod tests;

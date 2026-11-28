//! Non-blocking project-hook failures, delivered to the agent (#1739).
//!
//! A `.clud/hooks.json` hook that cannot start, times out or exits non-2 is
//! let through (fail open, `clud_hooks_run`). Its stderr used to go only to
//! clud's own stderr, which the harness shows the user and never the agent,
//! so an agent whose every call tripped a broken hook could not see why.
//!
//! Each call's failures are collected here while the hooks run, collapsed
//! into one line per distinct message with a count, filtered to the messages
//! this session has not seen yet, and added to the call's final output as
//! `additionalContext`.
//!
//! **Fail open**, like the other session ledgers: a missing session id or a
//! state error means the notice is shown, never hidden.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use kernal_api::hash::Sha256Hasher as Sha256;
use serde_json::Value;

use super::block_bad_cmd_repeat::{prune, write_fast};

const DIR_NAME: &str = "hook-notices";
const HASH_DOMAIN: &str = "clud-hook-notice-v1\0";
/// Distinct messages remembered per session; the oldest are forgotten first.
const MAX_SEEN: usize = 64;

thread_local! {
    static PENDING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Queue this call's failure notices for [`take`].
pub(super) fn stash(notices: Vec<String>) {
    PENDING.with(|pending| pending.borrow_mut().extend(notices));
}

/// One line per distinct message, in first-seen order, with a repeat count.
pub(super) fn collapse(notices: &[String]) -> Vec<String> {
    let mut order: Vec<(&str, usize)> = Vec::new();
    for notice in notices {
        match order.iter_mut().find(|(text, _)| *text == notice.as_str()) {
            Some((_, count)) => *count += 1,
            None => order.push((notice.as_str(), 1)),
        }
    }
    order
        .into_iter()
        .map(|(text, count)| match count {
            1 => text.to_string(),
            n => format!("{text} (x{n})"),
        })
        .collect()
}

fn digest(text: &str) -> String {
    let digest = Sha256::digest(format!("{HASH_DOMAIN}{text}").as_bytes());
    digest.as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn session_file(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join(DIR_NAME).join(format!(
        "{}.json",
        digest(&format!("session\0{session_id}"))
    ))
}

/// The messages in `messages` this session has not been shown, recording
/// them as shown. Any state error returns every message.
pub(super) fn unseen_at(state_dir: &Path, session_id: &str, messages: Vec<String>) -> Vec<String> {
    if session_id.is_empty() || messages.is_empty() {
        return messages;
    }
    let path = session_file(state_dir, session_id);
    let existing = std::fs::read(&path).ok();
    let mut seen: Vec<String> = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .unwrap_or_default();
    let fresh: Vec<String> = messages
        .into_iter()
        .filter(|message| !seen.contains(&digest(message)))
        .collect();
    if fresh.is_empty() {
        return fresh;
    }
    seen.extend(fresh.iter().map(|message| digest(message)));
    let excess = seen.len().saturating_sub(MAX_SEEN);
    seen.drain(..excess);
    if let Ok(bytes) = serde_json::to_vec(&seen) {
        let _ = write_fast(&path, &bytes, existing.is_none());
    }
    if existing.is_none() {
        if let Some(dir) = path.parent() {
            prune(dir, SystemTime::now());
        }
    }
    fresh
}

pub(super) fn note_text(lines: &[String]) -> String {
    format!(
        "[clud] A project hook failed without blocking this call; fix it or its \
         declaration in .clud/hooks.json:\n- {}",
        lines.join("\n- ")
    )
}

/// The pending notice for this call, once per distinct message per session.
pub(super) fn take(session_id: Option<&str>) -> Option<String> {
    let pending = PENDING.with(|pending| std::mem::take(&mut *pending.borrow_mut()));
    let lines = collapse(&pending);
    let lines = match (session_id, crate::daemon::default_state_dir()) {
        (Some(session_id), Ok(state_dir)) => unseen_at(&state_dir, session_id, lines),
        _ => lines,
    };
    (!lines.is_empty()).then(|| note_text(&lines))
}

/// `output` with `note` added to its `additionalContext` for `event`,
/// keeping any decision or context it already carries.
pub(super) fn merge(output: Value, event: &str, note: &str) -> Value {
    let mut object = match output {
        Value::Object(object) => object,
        _ => serde_json::Map::new(),
    };
    let specific = object
        .entry("hookSpecificOutput")
        .or_insert_with(|| serde_json::json!({ "hookEventName": event }));
    if let Some(specific) = specific.as_object_mut() {
        let context = match specific.get("additionalContext").and_then(Value::as_str) {
            Some(existing) if !existing.is_empty() => format!("{existing}\n\n{note}"),
            _ => note.to_string(),
        };
        specific.insert("additionalContext".to_string(), Value::String(context));
    }
    Value::Object(object)
}

#[cfg(test)]
#[path = "block_bad_cmd_hook_notice_tests.rs"]
mod tests;

//! One native check attempt per gate agent; model claims cannot manufacture success.
//!
//! Sidecars live in the harness session directory, beside its workflow metadata,
//! and share that directory's lifecycle and trust boundary. Missing, interrupted,
//! incomplete or unreadable evidence never authorizes a ready report.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::file_lock::FileExt;
use serde_json::{json, Value};

use super::{authorize, bound_check, native_label, HookPayloadView};

const MAX_BYTES: u64 = 16384;

fn path(payload: &HookPayloadView) -> Result<PathBuf, String> {
    // Resolve trusted metadata first, including the session/agent id checks.
    native_label(payload)?;
    let transcript = payload
        .transcript_path
        .as_ref()
        .ok_or("missing transcript")?;
    let agent = payload.agent_id.as_deref().ok_or("missing agent")?;
    Ok(transcript
        .with_extension("")
        .join(format!("gate-{agent}.json")))
}

fn binding(payload: &HookPayloadView, facts: &Value) -> Result<Value, String> {
    Ok(json!({
        "check": bound_check(payload, facts)?,
        "label": native_label(payload)?,
        "cwd": payload.cwd,
    }))
}

fn encode(value: &Value) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("gate evidence exceeds its size limit".into());
    }
    Ok(bytes)
}

pub(super) fn claim(payload: &HookPayloadView, facts: &Value) -> Result<(), String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|e| e.to_string())?;
    let nonce: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let bytes = encode(&json!({
        "binding": binding(payload, facts)?, "outcome": "pending", "nonce": nonce,
    }))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path(payload)?)
        .map_err(|e| format!("one check attempt per gate agent: {e}"))?;
    file.lock_exclusive().map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())
}

fn wrapped(check: &str, nonce: &str) -> String {
    format!(
        "(\n{check}\n)\n__clud_gate_{nonce}=$?\nprintf '\\nCLUD_GATE_EXIT_{nonce}:%s\\n' \
         \"$__clud_gate_{nonce}\"\n(exit \"$__clud_gate_{nonce}\")"
    )
}

pub(super) fn rewrite(payload: &HookPayloadView, facts: &Value) -> Result<String, String> {
    let mut file = open(payload, false)?;
    FileExt::lock_shared(&file).map_err(|e| e.to_string())?;
    let state = read(&mut file)?;
    if state.get("binding") != Some(&binding(payload, facts)?)
        || state.get("outcome").and_then(Value::as_str) != Some("pending")
    {
        return Err("gate has no matching pending execution".into());
    }
    Ok(wrapped(&bound_check(payload, facts)?, nonce(&state)?))
}

fn nonce(state: &Value) -> Result<&str, String> {
    state
        .get("nonce")
        .and_then(Value::as_str)
        .filter(|nonce| nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| "invalid gate completion receipt".into())
}

fn completed_success(raw: &Value, state: &Value) -> bool {
    let Some(response) = raw.get("tool_response") else {
        return false;
    };
    if response.get("interrupted").and_then(Value::as_bool) != Some(false)
        || [
            "backgroundTaskId",
            "backgroundOutputPath",
            "timedOutAfterMs",
        ]
        .iter()
        .any(|key| response.get(key).is_some_and(|value| !value.is_null()))
        || [
            "backgroundedByUser",
            "backgroundedByTurnAbort",
            "backgroundedToDeliverMessage",
            "noExitStatus",
        ]
        .iter()
        .any(|key| {
            response
                .get(key)
                .is_some_and(|value| value != &Value::Bool(false))
        })
    {
        return false;
    }
    let Ok(nonce) = nonce(state) else {
        return false;
    };
    let expected = format!("CLUD_GATE_EXIT_{nonce}:0");
    response
        .get("stdout")
        .and_then(Value::as_str)
        .and_then(|stdout| stdout.lines().last())
        == Some(expected.as_str())
}

fn open(payload: &HookPayloadView, write: bool) -> Result<File, String> {
    let path = path(payload)?;
    if !std::fs::symlink_metadata(&path)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_file()
    {
        return Err("gate evidence is not a regular file".into());
    }
    OpenOptions::new()
        .read(true)
        .write(write)
        .open(path)
        .map_err(|e| e.to_string())
}

fn read(file: &mut File) -> Result<Value, String> {
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("gate evidence exceeds its size limit".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

pub(super) fn observe(
    event: &str,
    raw: &Value,
    payload: &HookPayloadView,
    facts: &Value,
) -> Result<(), String> {
    if !matches!(event, "PostToolUse" | "PostToolUseFailure") {
        return Ok(());
    }
    let mut file = open(payload, true)?;
    file.lock_exclusive().map_err(|e| e.to_string())?;
    let mut state = read(&mut file)?;
    if state.get("binding") != Some(&binding(payload, facts)?)
        || state.get("outcome").and_then(Value::as_str) != Some("pending")
    {
        return Err("gate outcome does not match its pending check".into());
    }
    let check = bound_check(payload, facts)?;
    let mut original = payload.clone();
    original.command.clone_from(&check);
    authorize(&original, facts)?;
    if payload.command != check && payload.command != wrapped(&check, nonce(&state)?) {
        return Err("tool outcome does not identify the approved execution".into());
    }
    let success = event == "PostToolUse" && completed_success(raw, &state);
    state["outcome"] = json!(if success { "success" } else { "failure" });
    let bytes = encode(&state)?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.set_len(0).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())
}

pub(super) fn report(payload: &HookPayloadView, facts: &Value) -> Result<(), String> {
    let ready = payload
        .tool_input
        .as_ref()
        .and_then(|input| input.get("ready"))
        .and_then(Value::as_bool)
        .ok_or("gate report requires a boolean ready field")?;
    let expected = binding(payload, facts)?;
    // An explicit not-ready report cannot release dependent work.
    if !ready {
        return Ok(());
    }
    let mut file = open(payload, false)?;
    FileExt::lock_shared(&file).map_err(|e| e.to_string())?;
    let state = read(&mut file)?;
    if state.get("binding") != Some(&expected)
        || state.get("outcome").and_then(Value::as_str) != Some("success")
    {
        return Err("ready requires successful native execution of this approved check".into());
    }
    Ok(())
}

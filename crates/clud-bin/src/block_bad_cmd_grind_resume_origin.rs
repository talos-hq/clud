//! Native PostToolUse receipts bind resume admission to the originating cache.

use std::io::{Read, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use super::{HookPayloadView, MIN_VERSION};

const MAX_RECEIPT: u64 = 8192;

fn path(payload: &HookPayloadView, run: &str) -> Option<PathBuf> {
    if !crate::grind_facts::valid_session_id(run) {
        return None;
    }
    let directory = payload.transcript_path.as_ref()?.with_extension("");
    if !std::fs::symlink_metadata(&directory)
        .ok()?
        .file_type()
        .is_dir()
    {
        return None;
    }
    Some(directory.join(format!("grind-resume-{run}.json")))
}

pub(super) fn record(payload: &HookPayloadView, raw: &Value, version: [u64; 3]) -> Option<()> {
    if version < MIN_VERSION {
        return None;
    }
    let response = raw.get("tool_response")?;
    let run = response.get("runId")?.as_str()?;
    let script = response.get("scriptPath")?.as_str()?;
    let receipt = json!({"schema": "grind-native-cache/v1", "run": run,
        "session": payload.session_id, "script": script, "version": version});
    let bytes = serde_json::to_vec(&receipt).ok()?;
    if bytes.len() as u64 > MAX_RECEIPT {
        return None;
    }
    // Native Workflow persistence creates this directory asynchronously after
    // returning its launch references. The Post hook may win that race. Create
    // only the session directory beneath the verified transcript parent; never
    // create ancestors or accept an existing symlink/non-directory.
    let directory = payload.transcript_path.as_ref()?.with_extension("");
    match std::fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path(payload, run)?)
        .ok()?;
    file.write_all(&bytes).ok()?;
    Some(())
}

pub(super) fn allows(payload: &HookPayloadView) -> bool {
    read(payload).is_some_and(|receipt| {
        let input = payload.tool_input.as_ref();
        let version: Option<[u64; 3]> = receipt
            .get("version")
            .and_then(|version| serde_json::from_value(version.clone()).ok());
        receipt.get("schema").and_then(Value::as_str) == Some("grind-native-cache/v1")
            && receipt.get("session").and_then(Value::as_str) == payload.session_id.as_deref()
            && receipt.get("run") == input.and_then(|input| input.get("resumeFromRunId"))
            && receipt.get("script") == input.and_then(|input| input.get("scriptPath"))
            && version.is_some_and(|version| version >= MIN_VERSION)
            && approved_next_round(payload)
    })
}

fn read(payload: &HookPayloadView) -> Option<Value> {
    let run = payload
        .tool_input
        .as_ref()?
        .get("resumeFromRunId")?
        .as_str()?;
    let path = path(payload, run)?;
    if !std::fs::symlink_metadata(&path).ok()?.file_type().is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_RECEIPT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_RECEIPT {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

// Epoch maps are control input, not model assertions of completed work. Bind
// them to the native completed run's own result and preserve all original args.
fn approved_next_round(payload: &HookPayloadView) -> bool {
    let Some(snapshot) = snapshot(payload) else {
        return false;
    };
    let Some(input) = payload.tool_input.as_ref() else {
        return false;
    };
    if snapshot.get("runId") == input.get("resumeFromRunId")
        && snapshot.get("scriptPath") == input.get("scriptPath")
        && snapshot.get("args").is_some_and(Value::is_object)
        && input.get("args") == snapshot.get("args")
    {
        // #2470: identical launch arguments advance no epoch, so the native
        // cache replays exactly what it recorded; this covers a stopped run.
        return true;
    }
    let Some(resume) = snapshot.pointer("/result/resume") else {
        return false;
    };
    if snapshot.get("status").and_then(Value::as_str) != Some("completed")
        || snapshot.get("runId") != input.get("resumeFromRunId")
        || snapshot.get("scriptPath") != input.get("scriptPath")
        || resume.get("schema").and_then(Value::as_str) != Some("grind-publication-resume/v1")
        || resume.get("exhausted").and_then(Value::as_bool) == Some(true)
    {
        return false;
    }
    let Some(mut expected) = snapshot.get("args").and_then(Value::as_object).cloned() else {
        return false;
    };
    let Some(epochs) = resume.get("gateEpochs").filter(|epochs| epochs.is_object()) else {
        return false;
    };
    expected.insert("gateEpochs".into(), epochs.clone());
    input.get("args") == Some(&Value::Object(expected))
}

fn snapshot(payload: &HookPayloadView) -> Option<Value> {
    let run = payload
        .tool_input
        .as_ref()?
        .get("resumeFromRunId")?
        .as_str()?;
    let directory = path(payload, run)?.parent()?.join("workflows");
    if !std::fs::symlink_metadata(&directory)
        .ok()?
        .file_type()
        .is_dir()
    {
        return None;
    }
    let file = directory.join(format!("{run}.json"));
    if !std::fs::symlink_metadata(&file).ok()?.file_type().is_file() {
        return None;
    }
    const MAX_SNAPSHOT: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(MAX_SNAPSHOT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_SNAPSHOT {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

//! Refuse graph resume when the native runtime cannot keep branch-local caches.

use std::io::{Read, Seek, SeekFrom};

use serde_json::Value;

use super::super::HookPayloadView;

const MAX_TRANSCRIPT_TAIL: u64 = 256 * 1024;
const MIN_VERSION: [u64; 3] = [2, 1, 306];

#[path = "block_bad_cmd_grind_resume_origin.rs"]
mod origin;

pub(super) fn reason(payload: &HookPayloadView) -> Option<String> {
    if payload.tool_name != "Workflow" {
        return None;
    }
    let facts = crate::grind_facts::lookup(payload.session_id.as_deref());
    reason_for(payload, facts.facts())
}

fn graph_call(payload: &HookPayloadView, facts: Option<&Value>) -> bool {
    payload.tool_input.as_ref().is_some_and(|input| {
        input
            .pointer("/args/orchestration")
            .is_some_and(Value::is_object)
    }) || facts.is_some_and(|facts| facts.get("orchestration").is_some_and(Value::is_object))
}

pub(super) fn observe(event: &str, raw: &Value, payload: &HookPayloadView) {
    if event != "PostToolUse" || payload.tool_name != "Workflow" {
        return;
    }
    let facts = crate::grind_facts::lookup(payload.session_id.as_deref());
    if graph_call(payload, facts.facts()) && reason_for(payload, facts.facts()).is_none() {
        if let Some(version) = transcript_version(payload) {
            let _ = origin::record(payload, raw, version);
        }
    }
}

fn reason_for(payload: &HookPayloadView, facts: Option<&Value>) -> Option<String> {
    if !graph_call(payload, facts) {
        return None;
    }
    let version = verified_version(payload);
    if version.is_some_and(|version| version >= MIN_VERSION) {
        let input = payload.tool_input.as_ref()?;
        if input.get("resumeFromRunId").is_none_or(Value::is_null) {
            let base = input
                .pointer("/args/gateEpoch")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let initial = input
                .pointer("/args/gateEpochs")
                .and_then(Value::as_object)
                .is_none_or(|epochs| epochs.values().all(|epoch| epoch.as_u64() == Some(base)));
            return (!initial).then(|| {
                "Initial graph launches cannot advance gateEpochs. \
                Resume the completed native run using its own deferred manifest."
                    .into()
            });
        }
        if origin::allows(payload) {
            return None;
        }
        return Some(
            "Graph resume refused: the originating Workflow cache has no verified \
            supported-runtime receipt or the arguments do not match its native deferred manifest. A runtime upgrade does not migrate old cache keys. \
            Keep orchestration_resume and its manifest; report unsupported cache provenance. \
            Do not run Finish, bypass the refusal, or restart completed goals."
                .into(),
        );
    }
    Some(
        "Graph resumption requires Claude Code >= 2.1.306 with branch-local native caches; \
          this session's runtime is older or could not be verified. Keep orchestration_resume \
          and its deferred manifest, report the missing capability, and do not run Finish or \
          replay completed work. Updating the harness does not migrate an old run's cache."
            .into(),
    )
}

// Native hook groups run concurrently with asynchronous transcript persistence.
// Give absent metadata a small bounded flush window; a known old version is
// refused immediately, and an unverified version never authorizes execution.
fn verified_version(payload: &HookPayloadView) -> Option<[u64; 3]> {
    let mut version = transcript_version(payload);
    for delay in [10, 20, 40, 80, 160] {
        if version.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(delay));
        version = transcript_version(payload);
    }
    version
}

fn transcript_version(payload: &HookPayloadView) -> Option<[u64; 3]> {
    let session = payload.session_id.as_deref()?;
    if !crate::grind_facts::valid_session_id(session) {
        return None;
    }
    let path = payload.transcript_path.as_deref()?;
    if !path.is_absolute() || path.file_name()? != std::ffi::OsStr::new(&format!("{session}.jsonl"))
    {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let offset = metadata.len().saturating_sub(MAX_TRANSCRIPT_TAIL);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_TRANSCRIPT_TAIL)
        .read_to_end(&mut bytes)
        .ok()?;
    // Discard a partial first record before decoding, including a partial UTF-8 character.
    let complete = if offset > 0 {
        &bytes[bytes.iter().position(|byte| *byte == b'\n')? + 1..]
    } else {
        &bytes
    };
    complete
        .split(|byte| *byte == b'\n')
        .rev()
        .find_map(|line| {
            let row: Value = serde_json::from_slice(line).ok()?;
            if row.get("sessionId").and_then(Value::as_str) != Some(session) {
                return None;
            }
            row.get("version")
                .map(|version| version.as_str().and_then(parse_version))
        })?
}

fn parse_version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let version = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    parts.next().is_none().then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(path: &std::path::Path) -> HookPayloadView {
        HookPayloadView {
            tool_name: "Workflow".into(),
            command: String::new(),
            cwd: path.parent().unwrap().into(),
            tool_input: Some(json!({"resumeFromRunId": "wf-prior", "args": {
                "orchestration": {"goals": []}
            }})),
            agent_id: None,
            agent_type: None,
            session_id: Some("session".into()),
            transcript_path: Some(path.into()),
        }
    }

    fn record(version: Value, session: &str) -> String {
        json!({"type": "assistant", "sessionId": session, "version": version}).to_string() + "\n"
    }

    fn completed_snapshot(path: &std::path::Path) {
        let directory = path.with_extension("").join("workflows");
        std::fs::create_dir_all(&directory).unwrap();
        let snapshot = json!({"runId": "wf-new", "scriptPath": "/native/script.js",
            "status": "completed", "args": {"orchestration": {}},
            "result": {"resume": {"schema": "grind-publication-resume/v1", "gateEpochs": {}}}});
        std::fs::write(directory.join("wf-new.json"), snapshot.to_string()).unwrap();
    }

    #[test]
    fn rejects_older_unknown_and_malformed_versions_without_changing_manifest() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let payload = payload(&path);
        let manifest = json!({"orchestration_resume": {"remaining_goals": ["b", "c"]}});
        for version in [
            json!("2.1.282"),
            json!("2.1.305"),
            json!("unknown"),
            Value::Null,
        ] {
            std::fs::write(&path, record(version, "session")).unwrap();
            let denial = reason_for(&payload, Some(&manifest)).unwrap();
            assert!(denial.contains("2.1.306"));
            assert!(denial.contains("Keep orchestration_resume"));
            assert_eq!(
                manifest["orchestration_resume"]["remaining_goals"],
                json!(["b", "c"])
            );
        }
        std::fs::write(&path, "{}\n").unwrap();
        assert!(reason_for(&payload, None).is_some());
    }

    #[test]
    fn uses_latest_matching_session_version_in_bounded_transcript_tail() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let payload = payload(&path);
        for version in ["2.1.306", "2.2.0", "3.0.0"] {
            let content = "x".repeat(MAX_TRANSCRIPT_TAIL as usize + 1)
                + "\n"
                + &record(json!("2.1.282"), "session")
                + &record(json!(version), "session")
                + &record(json!("0.0.0"), "another-session");
            std::fs::write(&path, content).unwrap();
            assert_eq!(transcript_version(&payload), parse_version(version));
            assert!(reason_for(&payload, None).is_some()); // Unknown origin is unsafe after upgrade.
        }
        std::fs::write(
            &path,
            record(json!("2.1.306"), "session") + &record(json!("unknown"), "session"),
        )
        .unwrap();
        assert!(reason_for(&payload, None).is_some());
    }

    #[test]
    fn only_native_supported_origin_receipts_allow_resume_after_upgrade() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        std::fs::create_dir(root.path().join("session")).unwrap();
        std::fs::write(&path, record(json!("2.1.306"), "session")).unwrap();
        let mut payload = payload(&path);
        payload.tool_input = Some(json!({"name": "grind-run", "args": {"orchestration": {}}}));
        assert!(reason_for(&payload, None).is_none());
        let response =
            json!({"tool_response": {"runId": "wf-new", "scriptPath": "/native/script.js"}});
        observe("PreToolUse", &response, &payload);
        payload.tool_input = Some(
            json!({"resumeFromRunId": "wf-new", "scriptPath": "/native/script.js",
            "args": {"orchestration": {}, "gateEpochs": {}}}),
        );
        assert!(reason_for(&payload, None).is_some());
        payload
            .tool_input
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("resumeFromRunId");
        observe("PostToolUse", &response, &payload);
        payload.tool_input.as_mut().unwrap()["resumeFromRunId"] = json!("wf-new");
        payload.tool_input.as_mut().unwrap()["args"]["gateEpochs"] = json!({});
        completed_snapshot(&path);
        assert!(reason_for(&payload, None).is_none());
        payload.tool_input.as_mut().unwrap()["scriptPath"] = json!("/another/script.js");
        assert!(reason_for(&payload, None).is_some());
        payload.tool_input.as_mut().unwrap()["scriptPath"] = json!("/native/script.js");
        let receipt = root.path().join("session/grind-resume-wf-new.json");
        let mut old: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
        old["version"] = json!([2, 1, 282]);
        std::fs::write(receipt, old.to_string()).unwrap();
        assert!(reason_for(&payload, None)
            .unwrap()
            .contains("does not migrate"));
    }

    #[test]
    fn launch_receipt_does_not_race_native_session_directory_creation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        std::fs::write(&path, record(json!("2.1.306"), "session")).unwrap();
        let mut payload = payload(&path);
        payload.tool_input = Some(json!({"name": "grind-run", "args": {"orchestration": {}}}));
        assert!(!path.with_extension("").exists());
        let response =
            json!({"tool_response": {"runId": "wf-new", "scriptPath": "/native/script.js"}});
        observe("PostToolUse", &response, &payload);
        payload.tool_input = Some(
            json!({"resumeFromRunId": "wf-new", "scriptPath": "/native/script.js",
            "args": {"orchestration": {}, "gateEpochs": {}}}),
        );
        completed_snapshot(&path);
        assert!(reason_for(&payload, None).is_none());
        assert!(path
            .with_extension("")
            .join("grind-resume-wf-new.json")
            .is_file());
    }

    #[test]
    fn initial_admission_waits_for_native_transcript_metadata_to_flush() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let mut payload = payload(&path);
        payload.tool_input = Some(json!({"name": "grind-run", "args": {"orchestration": {}}}));
        assert!(transcript_version(&payload).is_none());
        let target = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            std::fs::write(target, record(json!("2.1.306"), "session")).unwrap();
        });
        assert!(reason_for(&payload, None).is_none());
        writer.join().unwrap();
    }

    #[test]
    fn native_manifest_binds_epochs_and_every_other_resume_argument() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        std::fs::write(&path, record(json!("2.1.306"), "session")).unwrap();
        let mut payload = payload(&path);
        payload.tool_input = Some(json!({"name": "grind-run", "args": {"orchestration": {}}}));
        let response =
            json!({"tool_response": {"runId": "wf-new", "scriptPath": "/native/script.js"}});
        observe("PostToolUse", &response, &payload);
        completed_snapshot(&path);
        payload.tool_input = Some(
            json!({"resumeFromRunId": "wf-new", "scriptPath": "/native/script.js",
            "args": {"orchestration": {}, "gateEpochs": {}}}),
        );
        assert!(reason_for(&payload, None).is_none());
        payload.tool_input.as_mut().unwrap()["args"]["gateEpochs"] = json!({"completed": 1});
        assert!(reason_for(&payload, None).is_some());
        payload.tool_input.as_mut().unwrap()["args"] = json!({"orchestration": {}, "gateEpochs": {},
            "goals": [{"id": "completed"}]});
        assert!(reason_for(&payload, None).is_some());
        payload.tool_input = Some(json!({"name": "grind-run", "args": {"orchestration": {},
            "gateEpochs": {"completed": 1}}}));
        assert!(reason_for(&payload, None)
            .unwrap()
            .contains("Initial graph"));
    }

    #[test]
    fn stopped_run_resumes_with_identical_launch_args_2470() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        std::fs::write(&path, record(json!("2.1.306"), "session")).unwrap();
        let mut payload = payload(&path);
        let args = json!({"orchestration": {"goals": ["a"]}, "gateEpoch": 0});
        payload.tool_input = Some(json!({"scriptPath": "/native/script.js", "args": args}));
        let response =
            json!({"tool_response": {"runId": "wf-new", "scriptPath": "/native/script.js"}});
        observe("PostToolUse", &response, &payload);
        let directory = path.with_extension("").join("workflows");
        std::fs::create_dir_all(&directory).unwrap();
        let snapshot = json!({"runId": "wf-new", "scriptPath": "/native/script.js",
            "status": "stopped", "args": args});
        std::fs::write(directory.join("wf-new.json"), snapshot.to_string()).unwrap();
        payload.tool_input = Some(json!({"resumeFromRunId": "wf-new",
            "scriptPath": "/native/script.js", "args": args}));
        assert!(reason_for(&payload, None).is_none());
        payload.tool_input.as_mut().unwrap()["args"]["gateEpochs"] = json!({"a": 1});
        assert!(reason_for(&payload, None).is_some());
    }

    #[test]
    fn non_graph_resumes_are_unchanged_and_facts_cannot_hide_a_graph() {
        let root = tempfile::tempdir().unwrap();
        let mut payload = payload(&root.path().join("session.jsonl"));
        payload.tool_input = Some(json!({"resumeFromRunId": "wf-prior"}));
        assert!(reason_for(&payload, None).is_none());
        assert!(reason_for(&payload, Some(&json!({"orchestration": {}}))).is_some());
        payload.tool_name = "Bash".into();
        assert!(reason(&payload).is_none());
        payload.tool_name = "Workflow".into();
        payload.tool_input = Some(json!({"name": "grind-run"}));
        assert!(reason(&payload).is_none());
    }
}

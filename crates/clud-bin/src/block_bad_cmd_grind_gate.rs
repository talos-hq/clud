//! Bind one gate shell command to native, session-local workflow metadata.
//!
//! Readiness additionally requires the native tool outcome recorded by `evidence`.

use std::io::Read;
use std::path::Path;

use serde_json::Value;

use super::{block_bad_cmd_gate, block_bad_cmd_grind_caps, HookPayloadView};

const MAX_METADATA_BYTES: u64 = 8192;
const MAX_GOALS: usize = 256;
const MAX_BLOCKERS: usize = 64;

#[path = "block_bad_cmd_grind_gate_evidence.rs"]
mod evidence;

#[path = "block_bad_cmd_grind_resume.rs"]
mod resume;

pub(super) fn resume_reason(payload: &HookPayloadView) -> Option<String> {
    resume::reason(payload)
}

pub(super) fn guards_report_tool(tool: &str) -> bool {
    tool == "StructuredOutput"
}

pub(super) fn rewrite(payload: &HookPayloadView) -> Result<String, String> {
    let facts = crate::grind_facts::lookup(payload.session_id.as_deref());
    evidence::rewrite(payload, facts.facts().ok_or("missing valid run facts")?)
}

pub(super) fn observe(event: &str, raw: &Value, payload: &HookPayloadView) {
    resume::observe(event, raw, payload);
    if payload.agent_type.as_deref() != Some(block_bad_cmd_grind_caps::GATE)
        || payload.tool_name != "Bash"
    {
        return;
    }
    let facts = crate::grind_facts::lookup(payload.session_id.as_deref());
    if let Some(facts) = facts.facts() {
        let _ = evidence::observe(event, raw, payload, facts);
    }
}

pub(super) fn reason(payload: &HookPayloadView) -> Option<String> {
    let role = block_bad_cmd_grind_caps::GATE;
    if let Some(reason) = block_bad_cmd_grind_caps::tool_reason(
        role,
        &payload.tool_name,
        &block_bad_cmd_grind_caps::RunFacts::default(),
    ) {
        return Some(reason);
    }
    let facts = crate::grind_facts::lookup(payload.session_id.as_deref());
    let result = facts
        .facts()
        .ok_or_else(|| "missing valid run facts".to_string())
        .and_then(|facts| {
            if guards_report_tool(&payload.tool_name) {
                evidence::report(payload, facts)
            } else if block_bad_cmd_gate::gates_tool(&payload.tool_name) {
                authorize(payload, facts)?;
                evidence::claim(payload, facts)
            } else {
                Ok(())
            }
        });
    result.err().map(|error| format!("{role}: {error}"))
}

fn authorize(payload: &HookPayloadView, facts: &Value) -> Result<(), String> {
    if payload.tool_name != "Bash" {
        return Err("approved gate checks require the native Bash tool".into());
    }
    if payload
        .tool_input
        .as_ref()
        .and_then(|input| input.get("run_in_background"))
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err("gate checks must complete in the foreground".into());
    }
    let check = bound_check(payload, facts)?;
    if check != payload.command {
        return Err("command differs from this agent's approved check".into());
    }
    block_bad_cmd_grind_caps::orchestrator_shell_reason(&check).map_or(Ok(()), Err)
}

fn bound_check(payload: &HookPayloadView, facts: &Value) -> Result<String, String> {
    if facts.get("phase").and_then(Value::as_str) == Some("plan") {
        return Err("checks cannot run during plan-only inspection".into());
    }
    let graph = facts.get("orchestration").ok_or("missing approved graph")?;
    let root = graph.get("session_root").and_then(Value::as_str);
    if root.map(Path::new) != Some(payload.cwd.as_path()) {
        return Err("gate cwd differs from the approved session root".into());
    }
    let label = native_label(payload)?;
    approved_check(graph, &label)
}

fn directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.file_type().is_dir() {
        return Err("workflow identity directory is not a regular directory".into());
    }
    Ok(())
}

fn native_label(payload: &HookPayloadView) -> Result<String, String> {
    let session = payload.session_id.as_deref().ok_or("missing session id")?;
    let agent = payload
        .agent_id
        .as_deref()
        .ok_or("missing native agent id")?;
    if !crate::grind_facts::valid_session_id(session)
        || !crate::grind_facts::valid_session_id(agent)
    {
        return Err("invalid native identity".into());
    }
    let transcript = payload
        .transcript_path
        .as_deref()
        .ok_or("missing transcript path")?;
    if !transcript.is_absolute()
        || transcript.file_name() != Some(std::ffi::OsStr::new(&format!("{session}.jsonl")))
    {
        return Err("transcript does not identify this parent session".into());
    }
    if !std::fs::symlink_metadata(transcript)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_file()
    {
        return Err("session transcript is not a regular file".into());
    }
    let session_dir = transcript.with_extension("");
    let agents_dir = session_dir.join("subagents");
    let workflows = agents_dir.join("workflows");
    for path in [&session_dir, &agents_dir, &workflows] {
        directory(path)?;
    }
    // A recurring native /loop keeps prior runs in this session directory.
    // Stream entries with constant memory rather than limiting session lifetime.
    let entries = std::fs::read_dir(&workflows).map_err(|e| e.to_string())?;
    let mut label = None;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        directory(&entry.path())?;
        let path = entry.path().join(format!("agent-{agent}.meta.json"));
        if !path.try_exists().map_err(|e| e.to_string())? {
            continue;
        }
        if label.is_some() {
            return Err("ambiguous native agent metadata".into());
        }
        label = Some(read_label(&path)?);
    }
    label.ok_or_else(|| "missing native agent metadata".into())
}

fn read_label(path: &Path) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("agent metadata is not a regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err("agent metadata exceeds size limit".into());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if value.get("agentType").and_then(Value::as_str) != Some(block_bad_cmd_grind_caps::GATE) {
        return Err("native metadata role differs from grind-gate".into());
    }
    value
        .get("description")
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "missing native task label".into())
}

fn approved_check(graph: &Value, label: &str) -> Result<String, String> {
    let repo = graph
        .get("session_repo")
        .and_then(Value::as_str)
        .filter(|repo| !repo.is_empty())
        .ok_or("missing approved session repository")?;
    let goals = graph
        .get("goals")
        .and_then(Value::as_array)
        .ok_or("missing approved goals")?;
    if goals.len() > MAX_GOALS {
        return Err("approved graph exceeds gate lookup limit".into());
    }
    let mut check = None;
    for goal in goals {
        let id = goal
            .get("id")
            .and_then(Value::as_str)
            .ok_or("missing approved goal id")?;
        let blockers = goal
            .get("blockers")
            .and_then(Value::as_array)
            .ok_or("missing blockers")?;
        if blockers.len() > MAX_BLOCKERS {
            return Err("approved goal exceeds blocker lookup limit".into());
        }
        for (index, blocker) in blockers.iter().enumerate() {
            if label != format!("gate:{id}:{index}") {
                continue;
            }
            if check.is_some() {
                return Err("ambiguous approved gate label".into());
            }
            if goal.get("repo").and_then(Value::as_str) != Some(repo) {
                return Err("gate belongs to a different repository session".into());
            }
            if !matches!(
                blocker.get("when").and_then(Value::as_str),
                Some("released" | "tag" | "published")
            ) {
                return Err("gate is not an approved publication condition".into());
            }
            check = Some(
                blocker
                    .get("check")
                    .and_then(Value::as_str)
                    .filter(|command| !command.trim().is_empty())
                    .ok_or("missing approved check")?
                    .to_owned(),
            );
        }
    }
    check.ok_or_else(|| "native task label has no approved gate".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        root: tempfile::TempDir,
        payload: HookPayloadView,
        facts: Value,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let transcript = root.path().join("session.jsonl");
            std::fs::write(&transcript, "{}").unwrap();
            let payload = HookPayloadView {
                tool_name: "Bash".into(),
                command: "echo CHECK_LEFT".into(),
                cwd: root.path().into(),
                tool_input: None,
                agent_id: Some("left-agent".into()),
                agent_type: Some(block_bad_cmd_grind_caps::GATE.into()),
                session_id: Some("session".into()),
                transcript_path: Some(transcript),
            };
            let facts = json!({"orchestration": {
                "session_repo": "o/r", "session_root": root.path(),
                "goals": [{"id": "downstream", "repo": "o/r", "blockers": [
                    {"goal": "left", "when": "released", "check": "echo CHECK_LEFT"},
                    {"goal": "right", "when": "published", "check": "echo CHECK_RIGHT"}
                ]}]
            }});
            let fixture = Self {
                root,
                payload,
                facts,
            };
            fixture.metadata("wf-one", "left-agent", "gate:downstream:0");
            fixture.metadata("wf-one", "right-agent", "gate:downstream:1");
            fixture
        }

        fn metadata(&self, workflow: &str, agent: &str, label: &str) -> std::path::PathBuf {
            let directory = self
                .root
                .path()
                .join("session/subagents/workflows")
                .join(workflow);
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join(format!("agent-{agent}.meta.json"));
            std::fs::write(
                &path,
                json!({
                    "agentType": block_bad_cmd_grind_caps::GATE, "description": label
                })
                .to_string(),
            )
            .unwrap();
            path
        }

        fn allowed(&self) -> bool {
            authorize(&self.payload, &self.facts).is_ok()
        }
    }

    #[test]
    fn readiness_requires_a_successful_native_outcome_for_this_binding() {
        let mut fixture = Fixture::new();
        let mut report = fixture.payload.clone();
        report.tool_name = "StructuredOutput".into();
        report.tool_input = Some(json!({"ready": true}));
        assert!(evidence::report(&report, &fixture.facts).is_err());
        evidence::claim(&fixture.payload, &fixture.facts).unwrap();
        assert!(evidence::claim(&fixture.payload, &fixture.facts).is_err());
        assert!(evidence::report(&report, &fixture.facts).is_err());
        let state: Value = serde_json::from_slice(
            &std::fs::read(fixture.root.path().join("session/gate-left-agent.json")).unwrap(),
        )
        .unwrap();
        let nonce = state["nonce"].as_str().unwrap();
        let response = json!({"tool_response": {"interrupted": false,
            "stdout": format!("actual output\nCLUD_GATE_EXIT_{nonce}:0\n")}});
        evidence::observe("PostToolUse", &response, &fixture.payload, &fixture.facts).unwrap();
        assert!(evidence::report(&report, &fixture.facts).is_ok());
        fixture.facts["orchestration"]["goals"][0]["blockers"][0]["check"] = json!("echo CHANGED");
        assert!(evidence::report(&report, &fixture.facts).is_err());
    }

    #[test]
    fn failed_interrupted_and_unknown_outcomes_cannot_release_a_gate() {
        for (event, response) in [
            ("PostToolUseFailure", json!({})),
            (
                "PostToolUse",
                json!({"tool_response": {"interrupted": true}}),
            ),
            ("PostToolUse", json!({})),
        ] {
            let fixture = Fixture::new();
            evidence::claim(&fixture.payload, &fixture.facts).unwrap();
            evidence::observe(event, &response, &fixture.payload, &fixture.facts).unwrap();
            let mut report = fixture.payload.clone();
            report.tool_name = "StructuredOutput".into();
            report.tool_input = Some(json!({"ready": true}));
            assert!(evidence::report(&report, &fixture.facts).is_err());
            report.tool_input = Some(json!({"ready": false}));
            assert!(evidence::report(&report, &fixture.facts).is_ok());
        }
    }

    #[test]
    fn background_requests_cannot_claim_an_execution() {
        let mut fixture = Fixture::new();
        fixture.payload.tool_input = Some(json!({"run_in_background": true}));
        assert!(authorize(&fixture.payload, &fixture.facts).is_err());
    }

    #[test]
    fn nonzero_and_unfinished_receipts_cannot_report_ready() {
        for (status, extra) in [
            (1, json!({})),
            (0, json!({"backgroundTaskId": "still-running"})),
            (0, json!({"timedOutAfterMs": 1000})),
            (0, json!({"backgroundedByUser": true})),
            (0, json!({"noExitStatus": true})),
        ] {
            let fixture = Fixture::new();
            evidence::claim(&fixture.payload, &fixture.facts).unwrap();
            let path = fixture.root.path().join("session/gate-left-agent.json");
            let state: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let nonce = state["nonce"].as_str().unwrap();
            let mut response = json!({"interrupted": false,
                "stdout": format!("CLUD_GATE_EXIT_{nonce}:{status}\n")});
            response
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            evidence::observe(
                "PostToolUse",
                &json!({"tool_response": response}),
                &fixture.payload,
                &fixture.facts,
            )
            .unwrap();
            let mut report = fixture.payload.clone();
            report.tool_name = "StructuredOutput".into();
            report.tool_input = Some(json!({"ready": true}));
            assert!(evidence::report(&report, &fixture.facts).is_err());
        }
    }

    #[test]
    fn each_agent_may_execute_only_its_own_approved_check() {
        let mut fixture = Fixture::new();
        assert!(fixture.allowed());
        fixture.payload.command = "echo CHECK_RIGHT".into();
        assert!(!fixture.allowed());
        fixture.payload.agent_id = Some("right-agent".into());
        assert!(fixture.allowed());
        fixture.payload.command = "echo CHECK_LEFT".into();
        assert!(!fixture.allowed());
    }

    #[test]
    fn approved_posix_text_is_not_authorized_in_other_shells() {
        let mut fixture = Fixture::new();
        fixture.payload.command = "echo %CANARY%".into();
        fixture.facts["orchestration"]["goals"][0]["blockers"][0]["check"] =
            json!(fixture.payload.command);
        assert!(fixture.allowed());
        for tool in [
            "cmd",
            "PowerShell",
            "powershell",
            "pwsh",
            "shell",
            "exec_command",
            "bash",
        ] {
            fixture.payload.tool_name = tool.into();
            assert!(!fixture.allowed(), "{tool}");
            assert!(reason(&fixture.payload).is_some(), "{tool}");
        }
    }

    #[test]
    fn check_text_is_exact_and_remains_read_only() {
        let mut fixture = Fixture::new();
        for command in [
            "echo CHECK_LEFT; true",
            "echo CHECK_LEFT > output",
            " echo CHECK_LEFT",
        ] {
            fixture.payload.command = command.into();
            assert!(!fixture.allowed(), "{command}");
        }
        fixture.payload.command = "gh release create v1 --repo o/r".into();
        fixture.facts["orchestration"]["goals"][0]["blockers"][0]["check"] =
            json!(fixture.payload.command);
        assert!(!fixture.allowed());
    }

    #[test]
    fn missing_or_mismatched_native_identity_is_denied() {
        let fixture = Fixture::new();
        for (session, agent) in [
            (None, Some("left-agent")),
            (Some("session"), None),
            (Some("../session"), Some("left-agent")),
            (Some("session"), Some("../left-agent")),
            (Some("other-session"), Some("left-agent")),
            (Some("session"), Some("unknown")),
        ] {
            let mut payload = fixture.payload.clone();
            payload.session_id = session.map(str::to_owned);
            payload.agent_id = agent.map(str::to_owned);
            assert!(authorize(&payload, &fixture.facts).is_err());
        }
        let mut payload = fixture.payload.clone();
        payload.transcript_path = None;
        assert!(authorize(&payload, &fixture.facts).is_err());
    }

    #[test]
    fn duplicate_metadata_and_unapproved_labels_fail_closed() {
        let fixture = Fixture::new();
        fixture.metadata("wf-two", "left-agent", "gate:downstream:1");
        assert!(!fixture.allowed());
        let fixture = Fixture::new();
        fixture.metadata("wf-one", "left-agent", "gate:unapproved:0");
        assert!(!fixture.allowed());
    }

    #[test]
    fn malformed_role_and_oversized_metadata_fail_closed() {
        for data in [
            "{}".into(),
            "not json".into(),
            "x".repeat(8193),
            json!({"agentType": "grind-editor", "description": "gate:downstream:0"}).to_string(),
        ] {
            let fixture = Fixture::new();
            let path = fixture.metadata("wf-one", "left-agent", "gate:downstream:0");
            std::fs::write(path, data).unwrap();
            assert!(!fixture.allowed());
        }
    }

    #[test]
    fn session_root_repository_phase_and_condition_must_match() {
        for change in [
            json!({"root": "/other"}),
            json!({"repo": "o/other"}),
            json!({"phase": "plan"}),
            json!({"condition": "merged"}),
        ] {
            let mut fixture = Fixture::new();
            if let Some(root) = change.get("root") {
                fixture.facts["orchestration"]["session_root"] = root.clone();
            }
            if let Some(repo) = change.get("repo") {
                fixture.facts["orchestration"]["goals"][0]["repo"] = repo.clone();
            }
            if let Some(phase) = change.get("phase") {
                fixture.facts["phase"] = phase.clone();
            }
            if let Some(condition) = change.get("condition") {
                fixture.facts["orchestration"]["goals"][0]["blockers"][0]["when"] =
                    condition.clone();
            }
            assert!(!fixture.allowed(), "{change}");
        }
    }

    #[test]
    fn gate_identity_survives_more_than_sixty_four_prior_workflows() {
        let fixture = Fixture::new();
        for index in 0..128 {
            fixture.metadata(&format!("wf-prior-{index}"), "unrelated", "unused");
        }
        assert!(fixture.allowed());
        fixture.metadata("wf-duplicate", "left-agent", "gate:downstream:0");
        assert!(!fixture.allowed());
    }

    #[test]
    fn duplicate_approved_labels_are_denied() {
        let mut fixture = Fixture::new();
        let goal = fixture.facts["orchestration"]["goals"][0].clone();
        fixture.facts["orchestration"]["goals"]
            .as_array_mut()
            .unwrap()
            .push(goal);
        assert!(!fixture.allowed());
    }
}

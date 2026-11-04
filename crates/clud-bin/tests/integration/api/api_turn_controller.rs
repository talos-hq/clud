//! Mock-backed coverage for durable API turn capture.
//!
//! The controller must drain provider JSONL without an HTTP consumer, persist
//! the earliest provider identity before sealing the turn, and retain raw
//! provider evidence independently of the bounded event cursor window.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clud::backend::{Backend, LaunchMode, RoutingMode};
use clud::command::LaunchPlan;
use clud::daemon::api_sessions::{
    ApiSessionBackend, ApiSessionState, ApiSessionStore, ApiTurnState, CreateApiSession,
    ResolvedApiSessionSettings,
};
use clud::daemon::api_turn_controller::launch_captured_turn;
use clud::graphics::GraphicsConfig;

use crate::common::{mock_agent_path, wait_until};

fn plan(
    executable: PathBuf,
    backend: Backend,
    cwd: &Path,
    script: &Path,
    exit_code: i32,
) -> LaunchPlan {
    LaunchPlan {
        unsafe_mode: false,
        allow_plan_mode: false,
        command: vec![
            executable.to_string_lossy().into_owned(),
            "--mock-stream-json".to_string(),
            script.to_string_lossy().into_owned(),
            "--mock-exit-code".to_string(),
            exit_code.to_string(),
        ],
        iterations: 1,
        backend,
        routing_mode: RoutingMode::Direct,
        model_provider: None,
        requested_harness: None,
        effective_harness: None,
        provider_source: None,
        harness_source: None,
        launch_mode: LaunchMode::Subprocess,
        cwd: Some(cwd.canonicalize().unwrap().to_string_lossy().into_owned()),
        graphics: GraphicsConfig::default(),
        repeat_schedule: None,
        task_summary: None,
        loop_markers: None,
        stream_json_progress: false,
        prompt_summary: false,
        codex_model: None,
        model_selection: None,
        failover: None,
        failover_allow_metered: false,
        allowed_models: Vec::new(),
        provider_only: Vec::new(),
        zdr: false,
        no_data_collection: false,
        redact_secrets: None,
        pinned_from_previous_selection: false,
        coauthor: clud::attribution::Coauthor::default(),
        route: None,
    }
}

fn create_session(store: &ApiSessionStore, backend: ApiSessionBackend, cwd: &Path) -> String {
    store
        .create(CreateApiSession {
            backend,
            cwd: cwd.to_path_buf(),
            name: None,
            origin: clud::session_origin::SessionOrigin::Test,
            resolved_settings: ResolvedApiSessionSettings {
                model: None,
                safe: true,
                model_provider: None,
                harness: None,
                routing_mode: None,
            },
        })
        .unwrap()
        .id
}

#[test]
fn mock_claude_capture_persists_identity_raw_unknown_and_malformed_before_idle() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("claude.jsonl");
    std::fs::write(
        &script,
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"claude-provider\"}\n{\"type\":\"future.event\"}\nnot json\n",
    )
    .unwrap();
    let store = ApiSessionStore::new(temp.path());
    let id = create_session(&store, ApiSessionBackend::Claude, temp.path());

    launch_captured_turn(
        store.clone(),
        &id,
        plan(mock_agent_path(), Backend::Claude, temp.path(), &script, 0),
    )
    .unwrap();

    assert!(wait_until(Duration::from_secs(10), || {
        store.get(&id).unwrap().state == ApiSessionState::Idle
    }));
    let record = store.get(&id).unwrap();
    assert_eq!(
        record.provider_session_id.as_deref(),
        Some("claude-provider")
    );
    assert_eq!(record.turns[0].state, ApiTurnState::Completed);
    assert!(record
        .events
        .iter()
        .any(|event| event.kind == "backend_event"));
    assert!(record
        .events
        .iter()
        .any(|event| event.kind == "backend_malformed"));
    let raw = temp
        .path()
        .join("logs")
        .join("api")
        .join(&id)
        .join("1.jsonl");
    assert!(std::fs::read_to_string(raw)
        .unwrap()
        .contains("claude-provider"));
    let resumed = store
        .begin_turn(&id, "resume-after-capture".to_string())
        .unwrap();
    assert_eq!(resumed.generation, 2);
    assert_eq!(
        store.get(&id).unwrap().provider_session_id.as_deref(),
        Some("claude-provider")
    );
}

#[test]
fn mock_codex_capture_persists_identity_then_records_nonzero_failure() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex.jsonl");
    std::fs::write(
        &script,
        "{\"type\":\"thread.started\",\"thread_id\":\"codex-thread\"}\n",
    )
    .unwrap();
    let store = ApiSessionStore::new(temp.path());
    let id = create_session(&store, ApiSessionBackend::Codex, temp.path());

    launch_captured_turn(
        store.clone(),
        &id,
        plan(mock_agent_path(), Backend::Codex, temp.path(), &script, 7),
    )
    .unwrap();

    assert!(wait_until(Duration::from_secs(10), || {
        store.get(&id).unwrap().state == ApiSessionState::Failed
    }));
    let record = store.get(&id).unwrap();
    assert_eq!(record.provider_session_id.as_deref(), Some("codex-thread"));
    assert_eq!(record.turns[0].state, ApiTurnState::Failed);
    assert_eq!(record.turns[0].disposition.as_deref(), Some("exit_7"));
}

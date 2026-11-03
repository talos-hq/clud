//! Mock-backed lifecycle serialization coverage for #1043.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clud::backend::{Backend, LaunchMode, RoutingMode};
use clud::command::LaunchPlan;
use clud::daemon::api_session_lifecycle::{ApiSessionLifecycle, LifecycleError, LifecycleReply};
use clud::daemon::api_sessions::{
    ApiSessionBackend, ApiSessionState, ApiSessionStore, CreateApiSession,
    ResolvedApiSessionSettings,
};
use clud::graphics::GraphicsConfig;

use crate::common::{mock_agent_path, wait_until};

fn plan(executable: PathBuf, cwd: &Path, args: Vec<String>) -> LaunchPlan {
    let mut command = vec![executable.to_string_lossy().into_owned()];
    command.extend(args);
    LaunchPlan {
        unsafe_mode: false,
        allow_plan_mode: false,
        command,
        iterations: 1,
        backend: Backend::Claude,
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

fn session(store: &ApiSessionStore, cwd: &Path) -> String {
    store
        .create(CreateApiSession {
            backend: ApiSessionBackend::Claude,
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
fn running_submit_is_busy_while_duplicate_replays_after_controller_restart() {
    let temp = tempfile::tempdir().unwrap();
    let store = ApiSessionStore::new(temp.path());
    let id = session(&store, temp.path());
    let lifecycle = ApiSessionLifecycle::new(store.clone());
    let slow = plan(
        mock_agent_path(),
        temp.path(),
        // Keep the child alive through replay/conflict assertions so `kill`
        // exercises terminal process cleanup rather than accepting a natural
        // completion race.
        vec!["--mock-sleep-ms".to_string(), "30000".to_string()],
    );
    let started = lifecycle
        .submit(
            &id,
            slow.clone(),
            Some("request-a".to_string()),
            "fingerprint-a".to_string(),
            false,
        )
        .unwrap();
    let turn_id = match started {
        LifecycleReply::Started { turn_id, .. } => turn_id,
        other => panic!("expected start, got {other:?}"),
    };
    assert!(wait_until(Duration::from_secs(3), || {
        store
            .get(&id)
            .map(|record| record.state == ApiSessionState::Running)
            .unwrap_or(false)
    }));
    assert_eq!(
        {
            let (sent, received) = std::sync::mpsc::sync_channel(1);
            let lifecycle = lifecycle.clone();
            let id = id.clone();
            let busy_plan = slow.clone();
            std::thread::spawn(move || {
                let _ = sent.send(lifecycle.submit(
                    &id,
                    busy_plan,
                    Some("request-b".to_string()),
                    "fingerprint-b".to_string(),
                    false,
                ));
            });
            received
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("known active handle must return busy without a process probe")
                .unwrap()
        },
        LifecycleReply::SessionBusy
    );
    assert_eq!(
        ApiSessionLifecycle::new(store.clone())
            .submit(
                &id,
                slow,
                Some("request-a".to_string()),
                "fingerprint-a".to_string(),
                false
            )
            .unwrap(),
        LifecycleReply::Replayed {
            turn_id: turn_id.clone()
        }
    );
    assert_eq!(
        lifecycle.submit(
            &id,
            plan(mock_agent_path(), temp.path(), vec![]),
            Some("request-a".to_string()),
            "different".to_string(),
            false
        ),
        Err(LifecycleError::IdempotencyConflict)
    );
    let kill_started = std::time::Instant::now();
    let killed = lifecycle.kill(&id);
    assert!(
        matches!(
            killed,
            Ok(LifecycleReply::Terminated) | Err(LifecycleError::KillTimeout)
        ),
        "terminal cleanup must be bounded, got {killed:?}"
    );
    assert!(kill_started.elapsed() < std::time::Duration::from_secs(5));
    let terminal = store.get(&id).unwrap();
    assert_eq!(
        terminal.state,
        ApiSessionState::Terminated,
        "terminal kill must be durable before the waiter observes child exit: {terminal:?}"
    );
    assert!(terminal.turns.iter().any(|turn| {
        turn.id == turn_id
            && matches!(
                turn.disposition.as_deref(),
                Some("terminal_kill") | Some("terminal_kill_timeout")
            )
    }));
}

#[test]
fn identity_makes_interrupt_resumable_replace_drains_then_kill_is_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("provider.jsonl");
    std::fs::write(&script, "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"provider-a\"}\n{\"type\":\"future\"}\n").unwrap();
    let store = ApiSessionStore::new(temp.path());
    let id = session(&store, temp.path());
    let lifecycle = ApiSessionLifecycle::new(store.clone());
    let streaming = plan(
        mock_agent_path(),
        temp.path(),
        vec![
            "--mock-stream-json".to_string(),
            script.to_string_lossy().into_owned(),
            "--mock-stream-delay-ms".to_string(),
            "1500".to_string(),
        ],
    );
    lifecycle
        .submit(&id, streaming.clone(), None, "first".to_string(), false)
        .unwrap();
    assert!(wait_until(Duration::from_secs(3), || store
        .get(&id)
        .unwrap()
        .provider_session_id
        .is_some()));
    let replacement = lifecycle
        .submit(
            &id,
            streaming,
            Some("replace".to_string()),
            "second".to_string(),
            true,
        )
        .unwrap();
    assert!(matches!(
        replacement,
        LifecycleReply::Started { generation: 2, .. }
    ));
    let interrupted = store.get(&id).unwrap().turns[0].clone();
    assert_eq!(
        interrupted.disposition.as_deref(),
        Some("graceful_interrupt")
    );
    lifecycle.kill(&id).unwrap();
    assert_eq!(store.get(&id).unwrap().state, ApiSessionState::Terminated);
    assert_eq!(
        lifecycle.submit(
            &id,
            plan(mock_agent_path(), temp.path(), vec![]),
            None,
            "later".to_string(),
            false
        ),
        Err(LifecycleError::Terminated)
    );
}

#[test]
fn completed_turn_without_provider_identity_cannot_be_resumed() {
    let temp = tempfile::tempdir().unwrap();
    let store = ApiSessionStore::new(temp.path());
    let id = session(&store, temp.path());
    let lifecycle = ApiSessionLifecycle::new(store.clone());
    lifecycle
        .submit(
            &id,
            plan(mock_agent_path(), temp.path(), vec![]),
            None,
            "first".to_string(),
            false,
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(10), || store
        .get(&id)
        .unwrap()
        .state
        == ApiSessionState::Idle));
    assert_eq!(
        lifecycle.submit(
            &id,
            plan(mock_agent_path(), temp.path(), vec![]),
            None,
            "resume".to_string(),
            false
        ),
        Err(LifecycleError::ResumeUnavailable)
    );
}

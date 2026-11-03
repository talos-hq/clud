use super::*;
use crate::backend::{
    Backend, HarnessSelection, LaunchMode, ModelProvider, PreferenceSource, RoutingMode,
};
use crate::command::LaunchPlan;
use crate::daemon::types::SessionKind;
use crate::graphics::GraphicsConfig;
use crate::process_identity::ProcessIdentity;
use serde::Serialize;

use super::session::to_json_vec;

fn sample_launch_spec() -> WorkerLaunchSpec {
    WorkerLaunchSpec {
        plan: LaunchPlan {
            unsafe_mode: true,
            allow_plan_mode: false,
            command: vec!["claude".to_string()],
            iterations: 1,
            backend: Backend::Claude,
            routing_mode: RoutingMode::Direct,
            model_provider: Some(ModelProvider::Codex),
            requested_harness: Some(HarnessSelection::Claude),
            effective_harness: Some(Backend::Claude),
            provider_source: Some(PreferenceSource::GlobalSetting),
            harness_source: Some(PreferenceSource::GlobalSetting),
            launch_mode: LaunchMode::Subprocess,
            cwd: Some("C:/work/repo".to_string()),
            graphics: GraphicsConfig::default(),
            repeat_schedule: None,
            task_summary: Some("wire test".to_string()),
            loop_markers: None,
            stream_json_progress: false,
            prompt_summary: false,
            codex_model: None,
            model_selection: Some(crate::provider_catalog::ResolvedModelSelection {
                provider: ModelProvider::Codex,
                model: Some("codex-sol".to_string()),
                wire_model: Some("gpt-5.6-sol".to_string()),
                effort: Some(crate::provider_catalog::EffortLevel::Low),
                context_window: None,
                model_source: Some(crate::provider_catalog::SelectionSource::CatalogDefault),
                effort_source: Some(crate::provider_catalog::SelectionSource::CatalogDefault),
                context_window_source: None,
            }),
            failover: None,
            failover_allow_metered: false,
            allowed_models: Vec::new(),
            provider_only: Vec::new(),
            zdr: false,
            no_data_collection: false,
            redact_secrets: None,
            pinned_from_previous_selection: false,
            coauthor: crate::attribution::Coauthor::default(),
            route: None,
        },
        kind: SessionKind::Subprocess,
        name: Some("sample".to_string()),
        detachable: true,
        background_on_launch: false,
        attachable: true,
        rows: 24,
        cols: 80,
        repeat_interval_secs: None,
        repeat_run_command: None,
        backlog_bytes: Some(256 * 1024),
        transcript_path: None,
        client_env: Vec::new(),
        login_env: vec![("PATH".to_string(), "C:\\Windows\\System32".to_string())],
    }
}

fn sample_snapshot() -> SessionSnapshot {
    SessionSnapshot {
        id: "sess-test".to_string(),
        kind: SessionKind::Subprocess,
        backend: Some("codex".to_string()),
        launch_mode: Some("subprocess".to_string()),
        repo_root: Some("C:/work/repo".to_string()),
        command: vec!["codex".to_string(), "exec".to_string()],
        cwd: Some("C:/work/repo".to_string()),
        name: Some("sample".to_string()),
        created_at: Some(42),
        detachable: true,
        background: true,
        attachable: true,
        repeat_interval_secs: None,
        repeat_next_run_at: None,
        repeat_running: false,
        daemon_pid: 100,
        worker_pid: 101,
        worker_port: 9020,
        root_pid: Some(102),
        daemon_pid_start: 0,
        worker_pid_start: 0,
        root_pid_start: 0,
        exit_code: None,
        exited_at: None,
        ctrl_c: Some(sample_profile()),
    }
}

fn sample_profile() -> CtrlCProfile {
    CtrlCProfile {
        cli_pid: Some(10),
        cli_observed_at_ms: Some(20),
        cli_handoff_at_ms: Some(30),
        cli_return_ready_at_ms: Some(31),
        cli_handoff_ms: Some(1),
        daemon_received_at_ms: Some(35),
        daemon_kill_started_at_ms: Some(36),
        daemon_kill_finished_at_ms: Some(37),
        daemon_kill_ms: Some(1),
        fast_path: true,
    }
}

fn sample_proc_snapshot() -> ProcTreeSnapshot {
    let mut snapshot = ProcTreeSnapshot::empty(2_000);
    snapshot.sampled_at_ms = 123_456;
    snapshot.rows.push(ProcRow {
        pid: 42,
        ppid: Some(10),
        originator: "CLUD:10".to_string(),
        originator_pid: Some(10),
        session_id: Some("sess-test".to_string()),
        session_name: Some("sample".to_string()),
        cpu_pct: 7.5,
        cpu_ewma_pct: 3.2,
        rss_bytes: 128 * 1024 * 1024,
        age_secs: 30,
        start_time: 0,
        command: "codex exec".to_string(),
        depth: 1,
        tier: ProcTier::Hot,
        live: true,
        exited_at_ms: None,
    });
    snapshot.recompute_summary();
    snapshot
}

fn assert_json_parity<T>(original: &T, decoded: &T)
where
    T: Serialize,
{
    let original_json = serde_json::to_value(original).unwrap();
    let decoded_json = serde_json::to_value(decoded).unwrap();
    assert_eq!(decoded_json, original_json);
}

#[test]
fn payload_protocol_constants_are_ascii_discriminators() {
    assert_eq!(CLUD_PROST_PAYLOAD_PROTOCOL.to_be_bytes(), *b"CLUD");
    assert_eq!(CLUD_JSON_PAYLOAD_PROTOCOL.to_be_bytes(), *b"CLJS");
}

#[test]
fn daemon_wire_format_env_values_default_to_prost() {
    assert_eq!(
        DaemonWireFormat::from_env_value(None).unwrap(),
        DaemonWireFormat::Prost
    );
    assert_eq!(
        DaemonWireFormat::from_env_value(Some("")).unwrap(),
        DaemonWireFormat::Prost
    );
    assert_eq!(
        DaemonWireFormat::from_env_value(Some("legacy-json")).unwrap(),
        DaemonWireFormat::Json
    );
    assert_eq!(
        DaemonWireFormat::from_env_value(Some("prost")).unwrap(),
        DaemonWireFormat::Prost
    );
    assert!(matches!(
        DaemonWireFormat::from_env_value(Some("bincode")),
        Err(WireError::InvalidDaemonWire(_))
    ));
}

#[test]
fn daemon_request_line_json_preserves_legacy_shape() {
    let request = DaemonRequest::Shutdown {
        client_version: None,
        expected_daemon: None,
    };
    let line = encode_daemon_request_line(&request, DaemonWireFormat::Json).unwrap();
    assert!(line.starts_with(br#"{"op":"shutdown"}"#));

    let line = String::from_utf8(line).unwrap();
    let (decoded, format) = decode_daemon_request_line(&line).unwrap();
    assert_eq!(format, DaemonWireFormat::Json);
    assert_json_parity(&request, &decoded);
}

#[test]
fn daemon_request_line_prost_carries_frame_envelope() {
    let request = DaemonRequest::Shutdown {
        client_version: None,
        expected_daemon: None,
    };
    let line = encode_daemon_request_line(&request, DaemonWireFormat::Prost).unwrap();
    let line = String::from_utf8(line).unwrap();
    assert!(line.starts_with("CLUD-FRAME/1 434c5544 "));

    let (decoded, format) = decode_daemon_request_line(&line).unwrap();
    assert_eq!(format, DaemonWireFormat::Prost);
    assert_json_parity(&request, &decoded);
}

#[test]
fn daemon_create_roundtrip_preserves_resolved_launch_metadata() {
    let request = DaemonRequest::Create {
        spec: Box::new(sample_launch_spec()),
    };
    let line = encode_daemon_request_line(&request, DaemonWireFormat::Prost).unwrap();
    let (decoded, format) = decode_daemon_request_line(&String::from_utf8(line).unwrap()).unwrap();
    assert_eq!(format, DaemonWireFormat::Prost);

    let DaemonRequest::Create { spec } = decoded else {
        panic!("expected create request");
    };
    assert_eq!(spec.plan.model_provider, Some(ModelProvider::Codex));
    assert_eq!(spec.plan.requested_harness, Some(HarnessSelection::Claude));
    assert_eq!(spec.plan.effective_harness, Some(Backend::Claude));
    assert!(spec.plan.unsafe_mode);
    assert_eq!(
        spec.login_env,
        vec![("PATH".to_string(), "C:\\Windows\\System32".to_string())]
    );
    assert_eq!(
        spec.plan.provider_source,
        Some(PreferenceSource::GlobalSetting)
    );
    assert_eq!(
        spec.plan.harness_source,
        Some(PreferenceSource::GlobalSetting)
    );
    let selection = spec.plan.model_selection.expect("model selection");
    assert_eq!(selection.model.as_deref(), Some("codex-sol"));
    assert_eq!(selection.wire_model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(
        selection.effort,
        Some(crate::provider_catalog::EffortLevel::Low)
    );
    assert_eq!(
        selection.model_source,
        Some(crate::provider_catalog::SelectionSource::CatalogDefault)
    );
    assert_eq!(
        selection.effort_source,
        Some(crate::provider_catalog::SelectionSource::CatalogDefault)
    );
}

/// Issue #881: only provider/harness metadata may cross the daemon wire --
/// the DeepSeek credential never does, because `LaunchPlan` has no field
/// that could carry it. This proves `ModelProvider::DeepSeek` itself
/// round-trips through the same wire protocol Codex already uses, and that
/// the encoded payload carries no secret-shaped value.
#[test]
fn daemon_create_roundtrip_preserves_deepseek_provider_with_no_secret_on_the_wire() {
    let mut spec = sample_launch_spec();
    spec.plan.model_provider = Some(ModelProvider::DeepSeek);
    spec.plan.requested_harness = Some(HarnessSelection::Default);
    spec.plan.effective_harness = Some(Backend::Claude);
    let request = DaemonRequest::Create {
        spec: Box::new(spec),
    };
    let line = encode_daemon_request_line(&request, DaemonWireFormat::Prost).unwrap();
    let encoded = String::from_utf8(line).unwrap();
    assert!(!encoded.contains("sk-"), "encoded wire payload: {encoded}");

    let (decoded, format) = decode_daemon_request_line(&encoded).unwrap();
    assert_eq!(format, DaemonWireFormat::Prost);
    let DaemonRequest::Create { spec } = decoded else {
        panic!("expected create request");
    };
    assert_eq!(spec.plan.model_provider, Some(ModelProvider::DeepSeek));
    assert_eq!(spec.plan.effective_harness, Some(Backend::Claude));
}

/// Kimi twin of `daemon_create_roundtrip_preserves_deepseek_provider_with_no_secret_on_the_wire`
/// (#937 Phase 3). Same proof: only provider/harness metadata crosses the
/// daemon wire, never the Kimi credential, and `ModelProvider::Kimi` itself
/// round-trips through the same wire protocol Codex/DeepSeek already use.
#[test]
fn daemon_create_roundtrip_preserves_kimi_provider_with_no_secret_on_the_wire() {
    let mut spec = sample_launch_spec();
    spec.plan.model_provider = Some(ModelProvider::Kimi);
    spec.plan.requested_harness = Some(HarnessSelection::Default);
    spec.plan.effective_harness = Some(Backend::Claude);
    let request = DaemonRequest::Create {
        spec: Box::new(spec),
    };
    let line = encode_daemon_request_line(&request, DaemonWireFormat::Prost).unwrap();
    let encoded = String::from_utf8(line).unwrap();
    assert!(!encoded.contains("sk-"), "encoded wire payload: {encoded}");

    let (decoded, format) = decode_daemon_request_line(&encoded).unwrap();
    assert_eq!(format, DaemonWireFormat::Prost);
    let DaemonRequest::Create { spec } = decoded else {
        panic!("expected create request");
    };
    assert_eq!(spec.plan.model_provider, Some(ModelProvider::Kimi));
    assert_eq!(spec.plan.effective_harness, Some(Backend::Claude));
}

#[test]
fn pre_625_daemon_repeat_fixture_defaults_launch_metadata() {
    let request: DaemonRequest = serde_json::from_str(
        r#"{
            "op":"create",
            "spec":{
                "plan":{
                    "command":["codex","exec","hello"],
                    "iterations":1,
                    "backend":"Codex",
                    "launch_mode":"subprocess",
                    "cwd":"C:/work/repo"
                },
                "kind":"subprocess",
                "rows":24,
                "cols":80,
                "repeat_interval_secs":30,
                "repeat_run_command":["clud","--codex","--repeat","30s"]
            }
        }"#,
    )
    .unwrap();

    let DaemonRequest::Create { spec } = request else {
        panic!("expected create request");
    };
    assert_eq!(spec.plan.model_provider, None);
    assert_eq!(spec.plan.routing_mode, RoutingMode::Direct);
    assert_eq!(spec.plan.requested_harness, None);
    assert_eq!(spec.plan.effective_harness, None);
    assert_eq!(spec.plan.provider_source, None);
    assert_eq!(spec.plan.harness_source, None);
    assert_eq!(spec.plan.model_provider(), ModelProvider::Codex);
    assert_eq!(spec.plan.effective_harness(), Backend::Codex);
    assert_eq!(spec.repeat_interval_secs, Some(30));
    assert_eq!(
        spec.repeat_run_command,
        Some(vec![
            "clud".to_string(),
            "--codex".to_string(),
            "--repeat".to_string(),
            "30s".to_string(),
        ])
    );
}

#[test]
fn daemon_response_line_prost_roundtrips() {
    let response = DaemonResponse::ShutdownAck { pid: 1234 };
    let line = encode_daemon_response_line(&response, DaemonWireFormat::Prost).unwrap();
    let decoded = decode_daemon_response_line(&String::from_utf8(line).unwrap()).unwrap();
    assert_json_parity(&response, &decoded);
}

#[test]
fn api_session_kill_is_additive_in_json_and_prost() {
    let request = DaemonRequest::ApiSessionKill {
        session_id: "api-sess-test".to_string(),
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["op"], "api_session_kill");
    assert_json_parity(
        &request,
        &serde_json::from_value::<DaemonRequest>(json).unwrap(),
    );
    let frame = encode_daemon_request_prost(&request, "api-kill").unwrap();
    assert_json_parity(&request, &decode_daemon_request(&frame).unwrap());

    let response = DaemonResponse::ApiSessionKilled {
        session_id: "api-sess-test".to_string(),
    };
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["op"], "api_session_killed");
    assert_json_parity(
        &response,
        &serde_json::from_value::<DaemonResponse>(json).unwrap(),
    );
    let frame = encode_daemon_response_prost(&response, "api-kill").unwrap();
    assert_json_parity(&response, &decode_daemon_response(&frame).unwrap());
}

#[test]
fn daemon_request_prost_roundtrips_json_shapes() {
    let cases = vec![
        DaemonRequest::Create {
            spec: Box::new(sample_launch_spec()),
        },
        DaemonRequest::Session {
            session_id: "sess-test".to_string(),
        },
        DaemonRequest::ListLiveCwds,
        DaemonRequest::Terminate {
            session_id: "sess-test".to_string(),
        },
        DaemonRequest::ApiSessionKill {
            session_id: "api-sess-test".to_string(),
        },
        DaemonRequest::Interrupt {
            session_id: "sess-test".to_string(),
            profile: sample_profile(),
        },
        DaemonRequest::AdoptKill {
            pids: vec![1, 2],
            reason: Some("ctrl_c_handoff".to_string()),
        },
        DaemonRequest::Gc {
            payload: GcOp::List {
                kind: Some("worktree".to_string()),
            },
        },
        DaemonRequest::Shutdown {
            client_version: None,
            expected_daemon: None,
        },
        DaemonRequest::ReapOrphans,
        DaemonRequest::Metrics,
        DaemonRequest::ProcSnapshot {
            include_dead_since_ms: 5_000,
        },
        DaemonRequest::AcquireClientLease {
            identity: ProcessIdentity::new(1234, 1_700_000_000),
        },
        DaemonRequest::ReleaseClientLease {
            identity: ProcessIdentity::new(1234, 1_700_000_000),
        },
    ];

    for request in cases {
        let frame = encode_daemon_request_prost(&request, "req-1").unwrap();
        assert_eq!(frame.payload_protocol, CLUD_PROST_PAYLOAD_PROTOCOL);
        let decoded = decode_daemon_request(&frame).unwrap();
        assert_json_parity(&request, &decoded);
    }
}

#[test]
fn daemon_response_prost_roundtrips_json_shapes() {
    let session = sample_snapshot();
    let cases = vec![
        DaemonResponse::Created {
            session: session.clone(),
        },
        DaemonResponse::Session {
            session: session.clone(),
        },
        DaemonResponse::LiveCwds {
            paths: vec![PathBuf::from("C:/work/repo"), PathBuf::from("D:/other")],
        },
        DaemonResponse::Terminated {
            session: session.clone(),
        },
        DaemonResponse::ApiSessionKilled {
            session_id: "api-sess-test".to_string(),
        },
        DaemonResponse::Interrupted { session },
        DaemonResponse::AdoptKillAck { accepted: 2 },
        DaemonResponse::Gc {
            reply: GcReply::ListOk { rows: Vec::new() },
        },
        DaemonResponse::ShutdownAck { pid: 1234 },
        DaemonResponse::ReapOrphansAck {
            found: 3,
            reaped: 3,
        },
        DaemonResponse::Metrics {
            pid: 1234,
            cpu_pct: 72.5,
        },
        DaemonResponse::ProcSnapshot {
            snapshot: sample_proc_snapshot(),
        },
        DaemonResponse::ClientLeaseAcquired { lease_count: 1 },
        DaemonResponse::ClientLeaseReleased { lease_count: 0 },
        DaemonResponse::Error {
            message: "failed".to_string(),
        },
    ];

    for response in cases {
        let frame = encode_daemon_response_prost(&response, "req-1").unwrap();
        assert_eq!(frame.payload_protocol, CLUD_PROST_PAYLOAD_PROTOCOL);
        let decoded = decode_daemon_response(&frame).unwrap();
        assert_json_parity(&response, &decoded);
    }
}

/// Mirror retired (zackees/running-process#385 item 3): encoders
/// emit ONLY the typed snapshot; `session_json` must stay empty.
#[test]
fn daemon_session_responses_encode_typed_snapshot_without_json_mirror() {
    let session = sample_snapshot();
    let cases = vec![
        DaemonResponse::Created {
            session: session.clone(),
        },
        DaemonResponse::Session {
            session: session.clone(),
        },
        DaemonResponse::Terminated {
            session: session.clone(),
        },
        DaemonResponse::Interrupted { session },
    ];

    for response in cases {
        let frame = encode_daemon_response_prost(&response, "req-typed").unwrap();
        let envelope = proto::DaemonToClient::decode(frame.payload.as_slice()).unwrap();
        let response = envelope.response.unwrap();
        match response {
            proto::daemon_to_client::Response::Created(created) => {
                assert!(created.session_json.is_empty(), "mirror is retired");
                assert_eq!(created.session.unwrap().id, "sess-test");
            }
            proto::daemon_to_client::Response::Session(session) => {
                assert!(session.session_json.is_empty(), "mirror is retired");
                assert_eq!(session.session.unwrap().id, "sess-test");
            }
            proto::daemon_to_client::Response::Terminated(terminated) => {
                assert!(terminated.session_json.is_empty(), "mirror is retired");
                assert_eq!(terminated.session.unwrap().id, "sess-test");
            }
            proto::daemon_to_client::Response::Interrupted(interrupted) => {
                assert!(interrupted.session_json.is_empty(), "mirror is retired");
                assert_eq!(interrupted.session.unwrap().id, "sess-test");
            }
            other => panic!("unexpected response payload: {other:?}"),
        }
    }
}

/// Mirror retired — see
/// [`daemon_session_responses_encode_typed_snapshot_without_json_mirror`].
#[test]
fn worker_attached_response_encodes_typed_snapshot_without_json_mirror() {
    let message = WorkerServerMessage::Attached {
        session: Box::new(sample_snapshot()),
    };
    let frame = encode_worker_server_prost(&message).unwrap();
    let envelope = proto::WorkerServerEnvelope::decode(frame.payload.as_slice()).unwrap();
    let proto::worker_server_envelope::Message::Attached(attached) = envelope.message.unwrap()
    else {
        panic!("expected attached worker payload");
    };
    assert!(attached.session_json.is_empty(), "mirror is retired");
    assert_eq!(attached.session.unwrap().id, "sess-test");
}

#[test]
fn daemon_session_responses_decode_legacy_json_only_snapshots() {
    let session = sample_snapshot();
    let session_json = to_json_vec(&session).unwrap();
    let cases = vec![
        proto::daemon_to_client::Response::Created(proto::CreatedResponse {
            session_json: session_json.clone(),
            session: None,
        }),
        proto::daemon_to_client::Response::Session(proto::SessionResponse {
            session_json: session_json.clone(),
            session: None,
        }),
        proto::daemon_to_client::Response::Terminated(proto::TerminatedResponse {
            session_json: session_json.clone(),
            session: None,
        }),
        proto::daemon_to_client::Response::Interrupted(proto::InterruptedResponse {
            session_json,
            session: None,
        }),
    ];

    for response in cases {
        let frame = prost_frame(
            proto::DaemonToClient {
                response: Some(response),
                request_id: "legacy-json-only".to_string(),
            }
            .encode_to_vec(),
        );
        let decoded = decode_daemon_response(&frame).unwrap();
        match decoded {
            DaemonResponse::Created { session }
            | DaemonResponse::Session { session }
            | DaemonResponse::Terminated { session }
            | DaemonResponse::Interrupted { session } => {
                assert_eq!(session.id, "sess-test");
                assert_eq!(session.worker_port, 9020);
            }
            other => panic!("unexpected decoded response: {other:?}"),
        }
    }
}

#[test]
fn worker_attached_response_decodes_legacy_json_only_snapshot() {
    let frame = prost_frame(
        proto::WorkerServerEnvelope {
            message: Some(proto::worker_server_envelope::Message::Attached(
                proto::WorkerAttachedResponse {
                    session_json: to_json_vec(&sample_snapshot()).unwrap(),
                    session: None,
                },
            )),
        }
        .encode_to_vec(),
    );
    let decoded = decode_worker_server(&frame).unwrap();
    let WorkerServerMessage::Attached { session } = decoded else {
        panic!("expected attached worker payload");
    };
    assert_eq!(session.id, "sess-test");
    assert_eq!(session.worker_port, 9020);
}

#[test]
fn worker_messages_prost_roundtrip_json_shapes() {
    let client_cases = vec![
        WorkerClientMessage::Attach {
            terminal: None,
            rows: Some(24),
            cols: Some(80),
        },
        WorkerClientMessage::Input {
            data_b64: "YWJj".to_string(),
            submit: true,
        },
        WorkerClientMessage::Resize {
            rows: 30,
            cols: 120,
        },
        WorkerClientMessage::Interrupt {
            profile: Some(sample_profile()),
        },
    ];

    for message in client_cases {
        let frame = encode_worker_client_prost(&message).unwrap();
        let decoded = decode_worker_client(&frame).unwrap();
        assert_json_parity(&message, &decoded);
    }

    let server_cases = vec![
        WorkerServerMessage::Attached {
            session: Box::new(sample_snapshot()),
        },
        WorkerServerMessage::Output {
            data_b64: "YWJj".to_string(),
        },
        WorkerServerMessage::Exited { exit_code: 130 },
        WorkerServerMessage::Error {
            message: "bad attach".to_string(),
        },
    ];

    for message in server_cases {
        let frame = encode_worker_server_prost(&message).unwrap();
        let decoded = decode_worker_server(&frame).unwrap();
        assert_json_parity(&message, &decoded);
    }
}

#[test]
fn worker_line_dispatch_preserves_json_and_prost_formats() {
    let attach = WorkerClientMessage::Attach {
        terminal: None,
        rows: Some(24),
        cols: Some(80),
    };
    let json_line = encode_worker_client_line(&attach, DaemonWireFormat::Json).unwrap();
    assert!(json_line.starts_with(br#"{"op":"attach""#));
    let (decoded_attach, format) =
        decode_worker_client_line(&String::from_utf8(json_line).unwrap()).unwrap();
    assert_eq!(format, DaemonWireFormat::Json);
    assert_json_parity(&attach, &decoded_attach);

    let attached = WorkerServerMessage::Attached {
        session: Box::new(sample_snapshot()),
    };
    let prost_line = encode_worker_server_line(&attached, DaemonWireFormat::Prost).unwrap();
    assert!(prost_line.starts_with(b"CLUD-FRAME/1 434c5544 "));
    let decoded_attached =
        decode_worker_server_line(&String::from_utf8(prost_line).unwrap()).unwrap();
    assert_json_parity(&attached, &decoded_attached);
}

#[test]
fn dispatcher_accepts_legacy_json_frames() {
    let request = DaemonRequest::Shutdown {
        client_version: None,
        expected_daemon: None,
    };
    let frame = encode_legacy_json_frame(&request).unwrap();
    let decoded = decode_daemon_request(&frame).unwrap();
    assert_json_parity(&request, &decoded);
}

#[test]
fn dispatcher_rejects_unknown_payload_protocol() {
    let err = decode_daemon_request(&WireFrame {
        payload_protocol: 0xFEED_BEEF,
        payload: Vec::new(),
    })
    .unwrap_err();
    assert!(matches!(
        err,
        WireError::UnknownPayloadProtocol(0xFEED_BEEF)
    ));
}

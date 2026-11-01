use super::*;
use crate::daemon::gc_service::spawn_registry_worker_with;
use crate::daemon::types::{CtrlCProfile, SessionKind, SessionSnapshot};
use crate::gc::Registry;
use crate::launch_log::LaunchRecord;
use std::io::Write;

/// Assert an HTTP status line carries `expected`, showing what it actually
/// carried when it does not (#1160).
///
/// `assert!(status.contains("200"))` prints nothing but the source line. On a
/// flaky lane that discards the one piece of evidence worth having: it cannot
/// distinguish "the response was empty or timed out" from "the response came
/// back with a different status", and those point at different bugs.
///
/// Observed twice on the windows-x64 unit lane in one evening, on two PRs that
/// touched neither this module nor Rust at all, and each occurrence produced
/// nothing to reason about beyond the assertion's own text.
#[track_caller]
fn assert_status(actual: &str, expected: &str) {
    assert!(
        actual.contains(expected),
        "expected status {expected}, got: {actual:?}"
    );
}

fn write_fake_session(state_dir: &Path, id: &str, snap: SessionSnapshot) {
    let dir = state_dir.join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&snap).unwrap()).unwrap();
}

fn fake_snapshot(id: &str, name: &str, cwd: &str) -> SessionSnapshot {
    SessionSnapshot {
        id: id.to_string(),
        kind: SessionKind::Pty,
        backend: Some("codex".to_string()),
        launch_mode: Some("pty".to_string()),
        repo_root: Some("/dev".to_string()),
        command: vec!["codex".to_string(), "exec".to_string()],
        cwd: Some(cwd.to_string()),
        name: Some(name.to_string()),
        created_at: Some(500),
        detachable: false,
        background: false,
        attachable: true,
        repeat_interval_secs: None,
        repeat_next_run_at: None,
        repeat_running: false,
        // Sensitive fields — the SessionView should drop these.
        daemon_pid: 1,
        // A PID this unlikely to be alive forces live=false in tests.
        worker_pid: 4_000_000_000,
        worker_port: 12345,
        root_pid: None,
        daemon_pid_start: 0,
        worker_pid_start: 0,
        root_pid_start: 0,
        exit_code: None,
        exited_at: None,
        ctrl_c: None,
    }
}

#[test]
fn dashboard_url_uses_a_one_time_capability_bootstrap() {
    assert_eq!(
        dashboard_url_from_info(54321, "test-capability"),
        "http://127.0.0.1:54321/?token=test-capability"
    );
}

#[test]
fn dashboard_access_requires_loopback_host_and_capability_token() {
    let access = DashboardAccess::new("test-capability".to_string());

    assert!(access.allows_host(Some("127.0.0.1:54321"), 54321));
    assert!(access.allows_host(Some("localhost:54321"), 54321));
    assert!(!access.allows_host(Some("attacker.example:54321"), 54321));
    assert!(!access.allows_host(None, 54321));

    assert!(access.allows_token(Some("test-capability"), None));
    assert!(access.allows_token(None, Some("clud_dashboard_token=test-capability")));
    assert!(!access.allows_token(Some("wrong"), None));
    assert!(!access.allows_token(None, None));
}

#[test]
fn api_discovery_routes_require_bearer_and_return_json() {
    let dir = tempfile::tempdir().unwrap();
    let port = spawn_dashboard_with_activity(
        dir.path().to_path_buf(),
        None,
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "test-capability".to_string(),
        "test-api-token".to_string(),
        None,
        None,
    )
    .expect("dashboard spawned");

    let unauthorized: serde_json::Value =
        serde_json::from_str(&fetch_api_path(port, "/v1/health", None).expect("fetch"))
            .expect("unauthorized JSON");
    assert_eq!(unauthorized["code"], "unauthorized");

    let health: serde_json::Value = serde_json::from_str(
        &fetch_api_path(port, "/v1/health", Some("Bearer test-api-token")).expect("fetch"),
    )
    .expect("health JSON");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["api_version"], "v1");

    let schema: serde_json::Value = serde_json::from_str(
        &fetch_api_path(port, "/v1/openapi.json", Some("Bearer test-api-token")).expect("fetch"),
    )
    .expect("OpenAPI JSON");
    assert_eq!(schema["openapi"], "3.1.0");
    assert!(schema["paths"].get("/v1/health").is_some());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn api_create_list_get_and_auth_boundary_are_request_level_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let port = spawn_dashboard_with_activity(
        dir.path().to_path_buf(),
        None,
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "dashboard-canary".to_string(),
        "api-canary".to_string(),
        None,
        None,
    )
    .unwrap();
    let cwd = dir.path().to_string_lossy().replace('\\', "\\\\");
    let create = format!(r#"{{"backend":"claude","cwd":"{cwd}","name":"request-level"}}"#);
    let (status, _, body) = fetch_api_request(
        port,
        "POST",
        "/v1/sessions",
        "localhost",
        Some("Bearer api-canary"),
        None,
        Some(&create),
    )
    .unwrap();
    assert_status(&status, "201");
    let record: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = record["id"].as_str().unwrap();
    assert!(record["cwd"]
        .as_str()
        .unwrap()
        .contains(dir.path().file_name().unwrap().to_str().unwrap()));
    let (status, _, list) = fetch_api_request(
        port,
        "GET",
        "/v1/sessions",
        "localhost",
        Some("Bearer api-canary"),
        None,
        None,
    )
    .unwrap();
    assert_status(&status, "200");
    assert!(list.contains(id));
    let (status, headers, body) = fetch_api_request(
        port,
        "GET",
        &format!("/v1/sessions/{id}"),
        "localhost",
        Some("Bearer api-canary"),
        None,
        None,
    )
    .unwrap();
    assert_status(&status, "200");
    assert!(!headers
        .to_ascii_lowercase()
        .contains("access-control-allow-origin"));
    assert!(!body.contains("api-canary"));
    let (status, _, body) = fetch_api_request(
        port,
        "POST",
        &format!("/v1/sessions/{id}/interrupt"),
        "localhost",
        Some("Bearer api-canary"),
        None,
        None,
    )
    .unwrap();
    assert_status(&status, "409");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
        "session_not_running"
    );
    let (status, _, body) = fetch_api_request(
        port,
        "POST",
        "/v1/sessions/missing/interrupt",
        "localhost",
        Some("Bearer api-canary"),
        None,
        None,
    )
    .unwrap();
    assert_status(&status, "404");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
        "not_found"
    );
    let store = super::super::api_sessions::ApiSessionStore::new(dir.path());
    for index in 0..=super::super::api_sessions::DEFAULT_EVENT_LIMIT {
        store
            .append_event(
                id,
                None,
                "source_test".to_string(),
                serde_json::json!({"index": index}),
            )
            .unwrap();
    }
    for path in [
        format!("/v1/sessions/{id}/events?after=bad"),
        format!("/v1/sessions/{id}/events?limit=0"),
        format!("/v1/sessions/{id}/events?limit=129"),
    ] {
        let (status, _, body) = fetch_api_request(
            port,
            "GET",
            &path,
            "localhost",
            Some("Bearer api-canary"),
            None,
            None,
        )
        .unwrap();
        assert_status(&status, "400");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
            "invalid_cursor"
        );
    }
    let (status, _, body) = fetch_api_request(
        port,
        "GET",
        &format!("/v1/sessions/{id}/events?after=0&limit=1"),
        "localhost",
        Some("Bearer api-canary"),
        None,
        None,
    )
    .unwrap();
    assert_status(&status, "200");
    let page: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["retention_gap"], true);
    for (host, authorization, cookie) in [
        ("attacker.invalid", Some("Bearer api-canary"), None),
        ("localhost", Some("Bearer wrong"), None),
        (
            "localhost",
            None,
            Some("clud_dashboard_token=dashboard-canary"),
        ),
    ] {
        let (status, _, body) = fetch_api_request(
            port,
            "GET",
            "/v1/sessions",
            host,
            authorization,
            cookie,
            None,
        )
        .unwrap();
        assert_status(&status, "401");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
            "unauthorized"
        );
        assert!(!body.contains("api-canary"));
    }
    let file = dir.path().join("not-a-directory");
    std::fs::write(&file, "x").unwrap();
    for cwd in [
        "relative".to_string(),
        dir.path().join("missing").to_string_lossy().into_owned(),
        file.to_string_lossy().into_owned(),
    ] {
        let body = serde_json::json!({"backend":"codex", "cwd": cwd}).to_string();
        let (status, _, body) = fetch_api_request(
            port,
            "POST",
            "/v1/sessions",
            "localhost",
            Some("Bearer api-canary"),
            None,
            Some(&body),
        )
        .unwrap();
        assert_status(&status, "400");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
            "invalid_request"
        );
    }
}

#[test]
fn api_openapi_contract_covers_each_implemented_session_route() {
    let schema = openapi_document();
    let paths = schema["paths"].as_object().unwrap();
    for path in [
        "/v1/sessions",
        "/v1/sessions/{id}",
        "/v1/sessions/{id}/turns",
        "/v1/sessions/{id}/interrupt",
        "/v1/sessions/{id}/events",
        "/v1/sessions/{id}/inject",
    ] {
        assert!(paths.contains_key(path), "missing route {path}");
    }
    assert_eq!(
        paths["/v1/sessions/{id}/events"]["get"]["parameters"][1]["name"],
        "limit"
    );
    for path in [
        "/v1/sessions/{id}/interrupt",
        "/v1/sessions/{id}/turns",
        "/v1/sessions/{id}/events",
        "/v1/sessions/{id}/inject",
    ] {
        assert_eq!(paths[path]["parameters"][0]["name"], "id");
    }
    assert_eq!(
        schema["components"]["securitySchemes"]["bearerAuth"]["scheme"],
        "bearer"
    );
    assert_eq!(schema["security"][0]["bearerAuth"], serde_json::json!([]));
    assert!(schema["components"]["schemas"]
        .get("EventsResponse")
        .is_some());
    assert!(schema["components"]["schemas"]
        .get("TurnResponse")
        .is_some());
    for schema_name in [
        "Session",
        "Event",
        "TurnResponse",
        "InterruptResponse",
        "Error",
    ] {
        assert!(
            schema["components"]["schemas"][schema_name]["properties"]
                .as_object()
                .is_some_and(|properties| !properties.is_empty()),
            "{schema_name} must be concrete"
        );
    }
    for (path, method) in [
        ("/v1/sessions", "get"),
        ("/v1/sessions", "post"),
        ("/v1/sessions/{id}", "get"),
        ("/v1/sessions/{id}", "delete"),
        ("/v1/sessions/{id}/turns", "post"),
        ("/v1/sessions/{id}/interrupt", "post"),
        ("/v1/sessions/{id}/events", "get"),
    ] {
        assert!(schema["paths"][path][method]["responses"]
            .get("401")
            .is_some());
    }
    for path in ["/v1/sessions/{id}", "/v1/sessions/{id}/events"] {
        assert!(schema["paths"][path]["get"]["responses"]
            .get("409")
            .is_some());
    }
}

#[test]
fn dashboard_state_surfaces_api_sessions_as_nonattachable_rows() {
    use crate::daemon::api_sessions::{
        ApiSessionBackend, CreateApiSession, ResolvedApiSessionSettings,
    };

    let temp = tempfile::tempdir().unwrap();
    let record = super::super::api_sessions::ApiSessionStore::new(temp.path())
        .create(CreateApiSession {
            backend: ApiSessionBackend::Codex,
            cwd: temp.path().to_path_buf(),
            name: Some("dashboard-api".to_string()),
            resolved_settings: ResolvedApiSessionSettings {
                model: None,
                safe: false,
                model_provider: Some("codex".to_string()),
                harness: Some("codex".to_string()),
                routing_mode: None,
            },
        })
        .unwrap();
    let state =
        super::http_dashboard_state::build_dashboard_state(temp.path(), None, 0, 0, Vec::new())
            .unwrap();
    let row = state
        .sessions
        .iter()
        .find(|session| session.id == record.id)
        .unwrap();
    assert_eq!(row.kind, "api");
    assert_eq!(row.source, "api");
    assert!(!row.attachable);
    assert!(!row.detachable);
    assert!(row.command.is_empty());
}

/// Needs the built `mock-agent`, so every caller is a `tier1_` test that
/// `bash test --host` skips (#2356).
fn http_test_mock_agent() -> std::path::PathBuf {
    let extension = if crate::platform::host::WINDOWS {
        ".exe"
    } else {
        ""
    };
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|path| path.parent())
                .unwrap()
                .join("target")
        });
    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.parent()?
                    .parent()
                    .map(|path| path.join(format!("mock-agent{extension}")))
            })
            .unwrap_or_default(),
        target.join("debug").join(format!("mock-agent{extension}")),
        target
            .join("x86_64-pc-windows-msvc")
            .join("debug")
            .join(format!("mock-agent{extension}")),
        target
            .join("x86_64-unknown-linux-gnu")
            .join("debug")
            .join(format!("mock-agent{extension}")),
        target
            .join("aarch64-unknown-linux-gnu")
            .join("debug")
            .join(format!("mock-agent{extension}")),
        target
            .join("aarch64-apple-darwin")
            .join("debug")
            .join(format!("mock-agent{extension}")),
        target
            .join("x86_64-apple-darwin")
            .join("debug")
            .join(format!("mock-agent{extension}")),
    ];
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .expect("mock-agent is built by the test bundle")
}

fn wait_for_api_state(
    store: &super::super::api_sessions::ApiSessionStore,
    id: &str,
    state: super::super::api_sessions::ApiSessionState,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if store
            .get(id)
            .map(|record| record.state == state)
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("API session {id} did not reach {state:?}");
}

fn wait_for_api_generation_to_finish(
    store: &super::super::api_sessions::ApiSessionStore,
    id: &str,
    generation: u64,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(record) = store.get(id) {
            if record.generation >= generation
                && record.state == super::super::api_sessions::ApiSessionState::Idle
            {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("API session {id} generation {generation} did not finish");
}

fn wait_for_idempotency_key(
    store: &super::super::api_sessions::ApiSessionStore,
    id: &str,
    key: &str,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if store
            .get(id)
            .map(|record| record.idempotency.iter().any(|entry| entry.key == key))
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("API session {id} did not record idempotency key {key}");
}

fn wait_for_provider_session_id(store: &super::super::api_sessions::ApiSessionStore, id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if store
            .get(id)
            .ok()
            .and_then(|record| record.provider_session_id)
            .is_some()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("API session {id} did not capture a provider session identity");
}

fn wait_for_no_activity(activity: &super::super::activity::DaemonActivity) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if activity.snapshot(std::time::Instant::now()).active_jobs == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("daemon activity did not return to idle");
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn tier1_authenticated_http_claude_turn_captures_identity_resumes_and_holds_activity() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let script = temp.path().join("claude.jsonl");
    std::fs::write(&script, "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"provider-http\"}\n{\"type\":\"future.event\"}\nnot json\n").unwrap();
    let _mock_guard = super::super::api_session_http::set_test_headless_executable(
        http_test_mock_agent().to_string_lossy().into_owned(),
        vec![
            "--mock-stream-json".to_string(),
            script.to_string_lossy().into_owned(),
            "--mock-stream-delay-ms".to_string(),
            "250".to_string(),
        ],
    );
    let activity = super::super::activity::DaemonActivity::new(std::time::Instant::now());
    let store = super::super::api_sessions::ApiSessionStore::new(temp.path());
    let lifecycle = std::sync::Arc::new(
        super::super::api_session_lifecycle::ApiSessionLifecycle::with_activity(
            store.clone(),
            activity.clone(),
        ),
    );
    let port = spawn_dashboard_with_activity_and_lifecycle(
        temp.path().to_path_buf(),
        None,
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "dashboard-token".to_string(),
        "api-token".to_string(),
        None,
        Some(activity.clone()),
        lifecycle,
    );
    let port = port.expect("dashboard spawned");
    let create = format!(
        r#"{{"backend":"claude","cwd":"{}","safe":true}}"#,
        cwd.to_string_lossy().replace('\\', "\\\\")
    );
    let (status, _, body) = fetch_api_request(
        port,
        "POST",
        "/v1/sessions",
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(&create),
    )
    .unwrap();
    assert_status(&status, "201");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        created["cwd"],
        cwd.canonicalize().unwrap().to_string_lossy().as_ref()
    );
    let id = created["id"].as_str().unwrap().to_string();
    let (status, _, _) = fetch_api_request(
        port,
        "POST",
        &format!("/v1/sessions/{id}/turns"),
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(r#"{"message":"first"}"#),
    )
    .unwrap();
    assert_status(&status, "202");
    assert!(activity.snapshot(std::time::Instant::now()).active_jobs > 0);
    wait_for_api_generation_to_finish(&store, &id, 1);
    wait_for_no_activity(&activity);
    let record = store.get(&id).unwrap();
    assert_eq!(record.provider_session_id.as_deref(), Some("provider-http"));
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
        .contains("provider-http"));
    let (status, _, _) = fetch_api_request(
        port,
        "POST",
        &format!("/v1/sessions/{id}/turns"),
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(r#"{"message":"resume"}"#),
    )
    .unwrap();
    assert_status(&status, "202");
    wait_for_api_generation_to_finish(&store, &id, 2);
    let resumed_raw = std::fs::read_to_string(
        temp.path()
            .join("logs")
            .join("api")
            .join(&id)
            .join("2.jsonl"),
    )
    .unwrap();
    assert!(resumed_raw.contains("provider-http"));
}

#[test]
fn tier1_authenticated_http_codex_turn_captures_thread_identity_and_resumes() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let script = temp.path().join("codex.jsonl");
    std::fs::write(
        &script,
        "{\"type\":\"thread.started\",\"thread_id\":\"codex-http-thread\"}\n",
    )
    .unwrap();
    let _mock_guard = super::super::api_session_http::set_test_headless_executable(
        http_test_mock_agent().to_string_lossy().into_owned(),
        vec![
            "--mock-stream-json".to_string(),
            script.to_string_lossy().into_owned(),
        ],
    );
    let store = super::super::api_sessions::ApiSessionStore::new(temp.path());
    let lifecycle = std::sync::Arc::new(
        super::super::api_session_lifecycle::ApiSessionLifecycle::new(store.clone()),
    );
    let port = spawn_dashboard_with_activity_and_lifecycle(
        temp.path().to_path_buf(),
        None,
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "dashboard-token".to_string(),
        "api-token".to_string(),
        None,
        None,
        lifecycle,
    )
    .unwrap();
    let create = format!(
        r#"{{"backend":"codex","cwd":"{}"}}"#,
        cwd.to_string_lossy().replace('\\', "\\\\")
    );
    let (_, _, body) = fetch_api_request(
        port,
        "POST",
        "/v1/sessions",
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(&create),
    )
    .unwrap();
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    for message in ["first", "resume"] {
        let request = format!(r#"{{"message":"{message}"}}"#);
        let (status, _, response_body) = fetch_api_request(
            port,
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            "127.0.0.1",
            Some("Bearer api-token"),
            None,
            Some(&request),
        )
        .unwrap();
        assert!(
            status.contains("202"),
            "unexpected Codex {message} response: {status}; body: {response_body}; record: {:?}",
            store.get(&id)
        );
        wait_for_api_state(
            &store,
            &id,
            super::super::api_sessions::ApiSessionState::Idle,
        );
    }
    assert_eq!(
        store.get(&id).unwrap().provider_session_id.as_deref(),
        Some("codex-http-thread")
    );
    assert!(std::fs::read_to_string(
        temp.path()
            .join("logs")
            .join("api")
            .join(&id)
            .join("2.jsonl")
    )
    .unwrap()
    .contains("codex-http-thread"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn tier1_authenticated_http_turn_idempotency_replay_conflict_replace_and_interrupt() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    // A controlled test-only slow child keeps the turn live long enough to
    // exercise request serialization; production still resolves installed
    // Claude/Codex executables through the typed launcher.
    let script = temp.path().join("slow-claude.jsonl");
    let mut stream = String::from(
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"provider-replace\"}\n",
    );
    for _ in 0..20 {
        stream.push_str("{\"type\":\"future.event\"}\n");
    }
    std::fs::write(&script, stream).unwrap();
    let _mock_guard = super::super::api_session_http::set_test_headless_executable(
        http_test_mock_agent().to_string_lossy().into_owned(),
        vec![
            "--mock-stream-json".to_string(),
            script.to_string_lossy().into_owned(),
            "--mock-stream-delay-ms".to_string(),
            "250".to_string(),
        ],
    );
    let store = super::super::api_sessions::ApiSessionStore::new(temp.path());
    let lifecycle = std::sync::Arc::new(
        super::super::api_session_lifecycle::ApiSessionLifecycle::new(store.clone()),
    );
    let port = spawn_dashboard_with_activity_and_lifecycle(
        temp.path().to_path_buf(),
        None,
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "dashboard-token".to_string(),
        "api-token".to_string(),
        None,
        None,
        lifecycle,
    )
    .unwrap();
    let create = format!(
        r#"{{"backend":"claude","cwd":"{}"}}"#,
        cwd.to_string_lossy().replace('\\', "\\\\")
    );
    let (_, _, body) = fetch_api_request(
        port,
        "POST",
        "/v1/sessions",
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(&create),
    )
    .unwrap();
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/v1/sessions/{id}/turns");
    let headers = [("Idempotency-Key", "replay-key")];
    let (first, _, first_body) = fetch_api_request_with_headers(
        port,
        "POST",
        &path,
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        &headers,
        Some(r#"{"message":"first"}"#),
    )
    .unwrap();
    assert_status(&first, "202");
    wait_for_idempotency_key(&store, &id, "replay-key");
    wait_for_provider_session_id(&store, &id);
    let (replayed, _, replay_body) = fetch_api_request_with_headers(
        port,
        "POST",
        &path,
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        &headers,
        Some(r#"{"message":"first"}"#),
    )
    .unwrap();
    assert_status(&replayed, "200");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first_body).unwrap()["turn_id"],
        serde_json::from_str::<serde_json::Value>(&replay_body).unwrap()["turn_id"]
    );
    let (conflict, _, _) = fetch_api_request_with_headers(
        port,
        "POST",
        &path,
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        &headers,
        Some(r#"{"message":"different"}"#),
    )
    .unwrap();
    assert!(
        conflict.contains("409"),
        "unexpected conflict response: {conflict}"
    );
    let (replaced, _, _) = fetch_api_request(
        port,
        "POST",
        &path,
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(r#"{"message":"replacement","interrupt_running":true}"#),
    )
    .unwrap();
    assert_status(&replaced, "202");
    wait_for_api_state(
        &store,
        &id,
        super::super::api_sessions::ApiSessionState::Running,
    );
    let (interrupted, _, interrupt_body) = fetch_api_request(
        port,
        "POST",
        &format!("/v1/sessions/{id}/interrupt"),
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        None,
    )
    .unwrap();
    assert!(
        interrupted.contains("200"),
        "unexpected interrupt response: {interrupted}; body: {interrupt_body}; record: {:?}",
        store.get(&id)
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&interrupt_body).unwrap()["status"],
        "interrupted"
    );
}

#[test]
fn purge_request_defaults_when_body_is_empty() {
    let parsed: PurgeRequest = serde_json::from_str("{}").unwrap();
    assert!(parsed.id.is_none());
    assert!(parsed.kind.is_none());
}

#[test]
fn purge_request_round_trips_kind_filter() {
    let json = r#"{"kind":"worktree"}"#;
    let parsed: PurgeRequest = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.kind.as_deref(), Some("worktree"));
}

#[test]
fn dashboard_html_asset_loads() {
    // Sanity check: the embedded asset compiled in. Tests pulled from
    // disk would mask a missing `include_str!`.
    assert!(DASHBOARD_HTML.contains("clud"));
}

#[test]
fn find_body_start_after_crlf_crlf() {
    let raw = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"x\":1}";
    let idx = find_body_start(raw).unwrap();
    assert_eq!(&raw[idx..], b"{\"x\":1}");
}

/// Shared "no live sessions" provider for the tests below that pre-date
/// issue #190 — they don't care about the registry merge and would
/// otherwise have to fight the global `CLUD_SESSION_DB` env-var.
fn empty_live_provider() -> super::LiveSessionsProvider {
    std::sync::Arc::new(Vec::new)
}

#[test]
fn build_state_with_empty_state_dir_returns_zeros() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_dashboard_state(dir.path(), None, 9999, 100, Vec::new()).expect("build");
    assert_eq!(state.stats.session_count, 0);
    assert_eq!(state.stats.live_session_count, 0);
    assert_eq!(state.stats.gc_count, 0);
    assert_eq!(state.stats.repo_count, 0);
    assert_eq!(state.daemon.ipc_port, 9999);
    assert_eq!(state.daemon.started_at_unix, 100);
    assert_eq!(state.daemon.version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn dashboard_state_does_not_wait_for_a_silent_daemon_snapshot_peer() {
    // #1465: /state.json is served by the daemon itself. A peer that accepts
    // its process-snapshot RPC but never replies must not hold the dashboard
    // HTTP thread past the caller's two-second response deadline.
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let info = crate::daemon::types::DaemonInfo {
        pid: std::process::id(),
        pid_start: 0,
        port,
        dashboard_port: None,
        dashboard_token: None,
        api_token: None,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };
    std::fs::write(
        dir.path().join("daemon.json"),
        serde_json::to_vec(&info).unwrap(),
    )
    .unwrap();

    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let peer = std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _stream = stream;
                let _ = release_rx.recv_timeout(Duration::from_secs(3));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if release_rx.try_recv().is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fake daemon accept failed: {error}"),
        }
    });

    let started = std::time::Instant::now();
    let state = build_dashboard_state(dir.path(), None, 9999, 100, Vec::new()).unwrap();
    let elapsed = started.elapsed();
    let _ = release_tx.send(());
    peer.join().unwrap();
    assert!(state.process_tree.is_null());
    assert!(
        elapsed < Duration::from_secs(1),
        "dashboard waited {elapsed:?} for a silent process-snapshot peer"
    );
}

#[test]
fn dashboard_state_does_not_wait_for_a_trickling_daemon_snapshot_peer() {
    // A per-read socket timeout is insufficient: each byte can arrive just
    // before that timeout and postpone the newline forever (#1465).
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let info = crate::daemon::types::DaemonInfo {
        pid: std::process::id(),
        pid_start: 0,
        port: listener.local_addr().unwrap().port(),
        dashboard_port: None,
        dashboard_token: None,
        api_token: None,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };
    std::fs::write(
        dir.path().join("daemon.json"),
        serde_json::to_vec(&info).unwrap(),
    )
    .unwrap();

    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        for _ in 0..20 {
            if release_rx.try_recv().is_ok() || stream.write_all(b" ").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });

    let started = std::time::Instant::now();
    let state = build_dashboard_state(dir.path(), None, 9999, 100, Vec::new()).unwrap();
    let elapsed = started.elapsed();
    let _ = release_tx.send(());
    peer.join().unwrap();
    assert!(state.process_tree.is_null());
    assert!(
        elapsed < Duration::from_secs(1),
        "dashboard waited {elapsed:?} for a trickling process-snapshot peer"
    );
}

#[test]
fn tool_telemetry_merges_start_finish_and_aggregates_recent_calls() {
    let store = ToolTelemetryStore::new();
    let now = 1_700_000_000_000;
    store.push_event(ToolEventIngest {
        event: "start".to_string(),
        id: "call-1".to_string(),
        name: "hooks/check.py".to_string(),
        start_time_ms: now - 5_000,
        end_time_ms: None,
        exit_code: None,
        stderr_tail: None,
    });
    store.push_event(ToolEventIngest {
        event: "finish".to_string(),
        id: "call-1".to_string(),
        name: "hooks/check.py".to_string(),
        start_time_ms: now - 5_000,
        end_time_ms: Some(now - 4_000),
        exit_code: Some(2),
        stderr_tail: Some("bad command".to_string()),
    });
    store.push_event(ToolEventIngest {
        event: "finish".to_string(),
        id: "call-2".to_string(),
        name: "tools/ok.py".to_string(),
        start_time_ms: now - 65_000,
        end_time_ms: Some(now - 64_000),
        exit_code: Some(0),
        stderr_tail: None,
    });

    let view = store.view_at(now);
    assert_eq!(view.entries.len(), 2);
    assert_eq!(view.entries[0].id, "call-1");
    assert_eq!(view.entries[0].exit_code, Some(2));
    assert_eq!(view.entries[0].stderr_tail.as_deref(), Some("bad command"));

    let last_10s = view
        .aggregate
        .iter()
        .find(|bucket| bucket.label == "last 10s")
        .unwrap();
    assert_eq!(last_10s.total, 1);
    assert_eq!(last_10s.failed, 1);

    let one_min = view
        .aggregate
        .iter()
        .find(|bucket| bucket.label == "1m")
        .unwrap();
    assert_eq!(one_min.total, 1);
    assert_eq!(one_min.success, 1);
}

/// Issue #190: direct-runner `clud` invocations only show up in the
/// redb session registry, not as JSON snapshots on disk. The dashboard
/// must merge those rows in so the Sessions tab isn't perpetually
/// empty for users who never use `--detach` / `--experimental-daemon-centralized`.
///
/// Inject a synthetic `LiveSession` directly so this test can run in
/// parallel with the rest of the suite — no env-var fiddling, no
/// cross-test races on `CLUD_SESSION_DB`.
#[test]
fn build_state_surfaces_direct_runner_registry_rows() {
    let dir = tempfile::tempdir().unwrap();
    let live = vec![LiveSession {
        pid: 4242,
        started_unix: 1_700_000_000,
        backend: Some("claude".to_string()),
        launch_mode: Some("subprocess".to_string()),
        cwd: Some("/dev/repo".to_string()),
    }];

    let state = build_dashboard_state(dir.path(), None, 9999, 100, live).expect("build");
    let direct: Vec<_> = state
        .sessions
        .iter()
        .filter(|s| s.kind == "direct")
        .collect();
    assert_eq!(
        direct.len(),
        1,
        "registry-backed direct session should appear; got {:?}",
        state.sessions
    );
    assert_eq!(direct[0].id, "direct-4242");
    assert_eq!(direct[0].name.as_deref(), Some("claude"));
    assert_eq!(direct[0].cwd.as_deref(), Some("/dev/repo"));
    assert!(direct[0].live);
    assert_eq!(direct[0].worker_port, 0);
    // The live-session count in the stats must include direct sessions
    // — that's what the dashboard header displays.
    assert_eq!(state.stats.live_session_count, 1);
}

#[test]
fn build_state_surfaces_completed_foreground_launch_records() {
    let dir = tempfile::tempdir().unwrap();
    let launches_dir = dir.path().join("launches");
    std::fs::create_dir_all(&launches_dir).unwrap();
    let record = LaunchRecord {
        id: "1700000000000-4242".to_string(),
        source: "direct".to_string(),
        clud_pid: 4242,
        backend: "codex".to_string(),
        launch_mode: "subprocess".to_string(),
        cwd: Some("/dev/fastled5".to_string()),
        repo_root: Some("/dev/fastled5".to_string()),
        command: vec!["codex".to_string(), "exec".to_string()],
        clud_argv: vec!["clud".to_string(), "--codex".to_string()],
        launched_at_ms: 1_700_000_000_000,
        exited_at_ms: Some(1_700_000_010_000),
        exit_code: Some(42),
        failure_reason: None,
    };
    std::fs::write(
        launches_dir.join("1700000000000-4242.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();

    let state = build_dashboard_state(dir.path(), None, 9999, 100, Vec::new()).expect("build");
    assert_eq!(state.sessions.len(), 1);
    let session = &state.sessions[0];
    assert_eq!(session.id, "launch-1700000000000-4242");
    assert_eq!(session.kind, "direct");
    assert_eq!(session.backend.as_deref(), Some("codex"));
    assert_eq!(session.launch_mode.as_deref(), Some("subprocess"));
    assert_eq!(session.cwd.as_deref(), Some("/dev/fastled5"));
    assert_eq!(session.repo_root.as_deref(), Some("/dev/fastled5"));
    assert_eq!(session.command, vec!["codex", "exec"]);
    assert_eq!(session.clud_argv, vec!["clud", "--codex"]);
    assert_eq!(session.created_at, Some(1_700_000_000_000));
    assert_eq!(session.exited_at, Some(1_700_000_010_000));
    assert_eq!(session.duration_ms, Some(10_000));
    assert_eq!(session.exit_code, Some(42));
    assert!(!session.live);
}

#[test]
fn build_state_includes_session_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    write_fake_session(
        dir.path(),
        "sess-a",
        fake_snapshot("sess-a", "test", "/dev/foo"),
    );

    let state = build_dashboard_state(dir.path(), None, 9999, 100, Vec::new()).expect("build");
    assert_eq!(state.sessions.len(), 1);
    assert_eq!(state.sessions[0].id, "sess-a");
    assert_eq!(state.sessions[0].name.as_deref(), Some("test"));
    assert_eq!(state.sessions[0].cwd.as_deref(), Some("/dev/foo"));
    assert_eq!(state.sessions[0].kind, "pty");
    // Unlikely-PID worker should be reported as not live.
    assert!(!state.sessions[0].live);

    // SessionView must not expose `daemon_pid` / `worker_pid` / `root_pid`.
    let json = serde_json::to_value(&state.sessions[0]).unwrap();
    assert!(json.get("daemon_pid").is_none());
    assert!(json.get("worker_pid").is_none());
    assert!(json.get("root_pid").is_none());
}

#[test]
fn build_state_includes_ctrl_c_events_when_present() {
    use crate::ctrl_c_track::{events_dir, CtrlCEvent, InvocationKind};
    let dir = tempfile::tempdir().unwrap();
    let edir = events_dir(dir.path());
    std::fs::create_dir_all(&edir).unwrap();
    for i in 0..3u64 {
        let event = CtrlCEvent {
            pid: 1_000 + i as u32,
            observed_at_ms: 1_700_000_000_000 + i * 1000,
            exit_at_ms: 1_700_000_000_500 + i * 1000,
            elapsed_ms: 500 + i,
            kind: InvocationKind::Direct,
            exit_code: 130,
            cwd: Some(format!("/tmp/a{i}")),
            handed_off: Some(i % 2 == 0),
            handoff_reason: Some(if i % 2 == 0 {
                "ctrl_c_subprocess".to_string()
            } else {
                "daemon_unreachable".to_string()
            }),
            ctrl_event_kind: None,
            forensics: None,
            press_kind: None,
            elapsed_since_prior_ms: None,
        };
        let path = edir.join(format!("{:013}-{}.json", event.exit_at_ms, event.pid));
        std::fs::write(&path, serde_json::to_vec(&event).unwrap()).unwrap();
    }
    let state = build_dashboard_state(dir.path(), None, 9999, 100, Vec::new()).expect("build");
    assert_eq!(state.ctrl_c_events.len(), 3);
    // Newest first
    assert_eq!(state.ctrl_c_events[0].exit_at_ms, 1_700_000_000_500 + 2_000);
    assert_eq!(state.ctrl_c_events[2].exit_at_ms, 1_700_000_000_500);
}

#[test]
fn build_state_includes_ctrl_c_profile() {
    let dir = tempfile::tempdir().unwrap();
    let mut snap = fake_snapshot("sess-ctrl-c", "interrupt", "/dev/ctrl-c");
    snap.ctrl_c = Some(CtrlCProfile {
        cli_pid: Some(777),
        cli_observed_at_ms: Some(10_000),
        cli_handoff_at_ms: Some(10_025),
        cli_return_ready_at_ms: Some(10_025),
        cli_handoff_ms: Some(25),
        daemon_received_at_ms: Some(10_026),
        daemon_kill_started_at_ms: Some(10_026),
        daemon_kill_finished_at_ms: Some(10_090),
        daemon_kill_ms: Some(64),
        fast_path: true,
    });
    write_fake_session(dir.path(), "sess-ctrl-c", snap);

    let state = build_dashboard_state(dir.path(), None, 9999, 100, Vec::new()).expect("build");
    let profile = state.sessions[0].ctrl_c.as_ref().expect("profile");
    assert_eq!(profile.cli_handoff_ms, Some(25));
    assert_eq!(profile.daemon_kill_ms, Some(64));
    assert!(profile.fast_path);
}

#[test]
fn end_to_end_state_endpoint_returns_all_three_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("e2e.redb");

    // Seed: one session.
    write_fake_session(
        dir.path(),
        "sess-x",
        fake_snapshot("sess-x", "fix", "/dev/repo"),
    );

    // Seed: one GC row + one repo visit.
    let registry = Registry::open_at(&db_path).expect("open registry");
    let gc_tx = spawn_registry_worker_with(registry).expect("worker");
    let (rx_t, rx) = mpsc::sync_channel::<GcReply>(1);
    gc_tx
        .send(RegistryMsg::Op(GcRequestMsg {
            op: GcOp::Insert {
                kind: "worktree".to_string(),
                path: "/tmp/wt-x".to_string(),
                repo_root: Some("/dev/repo".to_string()),
                branch: Some("feat/x".to_string()),
                agent_id: Some("agent-x".to_string()),
                created_unix: Some(1000),
            },
            reply_tx: rx_t,
        }))
        .unwrap();
    let _ = rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let (rx_t, rx) = mpsc::sync_channel::<GcReply>(1);
    gc_tx
        .send(RegistryMsg::Op(GcRequestMsg {
            op: GcOp::RecordRepoVisit {
                repo_root: "/dev/repo".to_string(),
                cwd: "/dev/repo".to_string(),
                now_unix: Some(2000),
            },
            reply_tx: rx_t,
        }))
        .unwrap();
    let _ = rx.recv_timeout(Duration::from_secs(2)).unwrap();

    // Spawn the actual HTTP server.
    let port = spawn_dashboard(
        dir.path().to_path_buf(),
        Some(gc_tx.clone()),
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "test-capability".to_string(),
    )
    .expect("dashboard spawned");

    // Hit /state.json.
    let body = fetch_state_json(port, "test-capability").expect("fetch state");
    let state: DashboardState = serde_json::from_str(&body).expect("parse");
    assert_eq!(state.stats.session_count, 1);
    assert_eq!(state.stats.gc_count, 1);
    assert_eq!(state.stats.repo_count, 1);
    assert_eq!(state.sessions[0].name.as_deref(), Some("fix"));
    assert_eq!(state.gc[0].kind, "worktree");
    assert_eq!(state.repos[0].repo_root, "/dev/repo");
    assert_eq!(state.repos[0].run_count, 1);

    // Hit GET / and confirm the HTML asset is served.
    let html_body = fetch_path(port, "GET", "/", None).expect("fetch root");
    assert!(html_body.contains("clud dashboard"));
}

#[test]
fn end_to_end_tool_event_endpoint_round_trips_summary() {
    let dir = tempfile::tempdir().unwrap();
    let port = spawn_dashboard(
        dir.path().to_path_buf(),
        None,
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "test-capability".to_string(),
    )
    .expect("dashboard spawned");

    let start = r#"{"event":"start","id":"call-http","name":"hooks/test.py","start_time_ms":1700000000000}"#;
    let body = fetch_path(port, "POST", "/tools/event", Some(start.to_string())).expect("post");
    assert_eq!(body, "{}");
    let finish = r#"{"event":"finish","id":"call-http","name":"hooks/test.py","start_time_ms":1700000000000,"end_time_ms":1700000000100,"exit_code":1,"stderr_tail":"failed"}"#;
    let body = fetch_path(port, "POST", "/tools/event", Some(finish.to_string())).expect("post");
    assert_eq!(body, "{}");

    let tools_body = fetch_path(port, "GET", "/tools", None).expect("tools");
    let view: ToolTelemetryView = serde_json::from_str(&tools_body).expect("parse tools");
    assert_eq!(view.entries.len(), 1);
    assert_eq!(view.entries[0].name, "hooks/test.py");
    assert_eq!(view.entries[0].exit_code, Some(1));
    assert_eq!(view.entries[0].stderr_tail.as_deref(), Some("failed"));
}

#[test]
fn end_to_end_purge_kind_round_trip_mutates_registry() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("purge.redb");

    let registry = Registry::open_at(&db_path).expect("open registry");
    let gc_tx = spawn_registry_worker_with(registry).expect("worker");
    for p in ["/tmp/p-a", "/tmp/p-b"] {
        let (rx_t, rx) = mpsc::sync_channel::<GcReply>(1);
        gc_tx
            .send(RegistryMsg::Op(GcRequestMsg {
                op: GcOp::Insert {
                    kind: "cache".to_string(),
                    path: p.to_string(),
                    repo_root: None,
                    branch: None,
                    agent_id: None,
                    created_unix: Some(1000),
                },
                reply_tx: rx_t,
            }))
            .unwrap();
        let _ = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    let port = spawn_dashboard(
        dir.path().to_path_buf(),
        Some(gc_tx.clone()),
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "test-capability".to_string(),
    )
    .expect("dashboard spawned");

    // POST /gc/purge {"kind":"cache"} — bulk async purge.
    let body = fetch_path(
        port,
        "POST",
        "/gc/purge",
        Some(r#"{"kind":"cache"}"#.to_string()),
    )
    .expect("purge");
    let resp: PurgeResponse = serde_json::from_str(&body).expect("parse");
    // Issue #268: bulk purge replies `dispatched`, not `removed`.
    // The two entries point at /tmp/p-a and /tmp/p-b, which do not
    // exist on disk → `remove_dir_all` short-circuits to Ok and the
    // worker drops the redb rows once the completions land.
    assert_eq!(resp.dispatched, Some(2));
    assert_eq!(resp.removed, None);
    assert_eq!(resp.skipped, 0);

    // Async deletes land slightly after the HTTP response — poll
    // until the registry shrinks rather than racing against it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state_body = fetch_state_json(port, "test-capability").expect("re-fetch state");
        let state: DashboardState = serde_json::from_str(&state_body).expect("parse state");
        if state.stats.gc_count == 0 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "registry never drained after bulk purge (gc_count={})",
                state.stats.gc_count
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Per-row Delete button on the dashboard: `POST /gc/purge {id: N}`
/// must remove exactly the targeted row even when other rows share
/// its `kind`. Replaces the earlier "single row of a kind" workaround
/// that returned a 500 in this case.
#[test]
fn end_to_end_per_row_delete_only_targets_requested_id() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("delete-by-id.redb");

    let registry = Registry::open_at(&db_path).expect("open registry");
    let gc_tx = spawn_registry_worker_with(registry).expect("worker");

    // Three siblings of the same kind in a tempdir.
    let paths: Vec<String> = ["e1", "e2", "e3"]
        .iter()
        .map(|name| {
            let p = dir.path().join(name);
            std::fs::create_dir_all(&p).unwrap();
            p.to_string_lossy().to_string()
        })
        .collect();
    for p in &paths {
        let (rx_t, rx) = mpsc::sync_channel::<GcReply>(1);
        gc_tx
            .send(RegistryMsg::Op(GcRequestMsg {
                op: GcOp::Insert {
                    kind: "cache".to_string(),
                    path: p.clone(),
                    repo_root: None,
                    branch: None,
                    agent_id: None,
                    created_unix: Some(1000),
                },
                reply_tx: rx_t,
            }))
            .unwrap();
        let _ = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    let port = spawn_dashboard(
        dir.path().to_path_buf(),
        Some(gc_tx.clone()),
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "test-capability".to_string(),
    )
    .expect("dashboard spawned");

    // Fetch /state.json to get the assigned ids.
    let state_body = fetch_state_json(port, "test-capability").expect("fetch state");
    let state: DashboardState = serde_json::from_str(&state_body).expect("parse");
    let middle = state
        .gc
        .iter()
        .find(|r| r.path == paths[1])
        .expect("middle row");

    // POST /gc/purge {"id": <middle.id>}
    let body = fetch_path(
        port,
        "POST",
        "/gc/purge",
        Some(format!(r#"{{"id":{}}}"#, middle.id)),
    )
    .expect("delete");
    let resp: PurgeResponse = serde_json::from_str(&body).expect("parse");
    // Per-row Delete uses the synchronous `DeleteById` path —
    // response shape stays `removed`, not `dispatched`.
    assert_eq!(resp.removed, Some(1));
    assert_eq!(resp.dispatched, None);
    assert_eq!(resp.skipped, 0);

    // The two siblings must survive.
    let after = fetch_state_json(port, "test-capability").expect("re-fetch state");
    let after_state: DashboardState = serde_json::from_str(&after).expect("parse");
    let surviving: Vec<&str> = after_state.gc.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(after_state.gc.len(), 2);
    assert!(surviving.contains(&paths[0].as_str()));
    assert!(surviving.contains(&paths[2].as_str()));
    assert!(!surviving.contains(&paths[1].as_str()));

    // On-disk deletion happened for the targeted row only.
    assert!(!std::path::Path::new(&paths[1]).exists());
    assert!(std::path::Path::new(&paths[0]).exists());
    assert!(std::path::Path::new(&paths[2]).exists());
}

/// Tiny HTTP/1.0 client for tests. Connect, send a request, read the
/// body. Avoids pulling in a real HTTP client dep just for tests.
fn fetch_path(port: u16, method: &str, path: &str, body: Option<String>) -> io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut req = format!(
        "{method} {path} HTTP/1.0\r\nHost: localhost:{port}\r\nCookie: clud_dashboard_token=test-capability\r\nConnection: close\r\n",
    );
    if let Some(b) = &body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            b.len(),
            b
        ));
    } else {
        req.push_str("\r\n");
    }
    stream.write_all(req.as_bytes())?;
    stream.flush()?;
    let mut buf = Vec::with_capacity(4096);
    stream.read_to_end(&mut buf)?;
    let body_start = find_body_start(&buf)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    String::from_utf8(buf[body_start..].to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

fn fetch_api_path(port: u16, path: &str, authorization: Option<&str>) -> io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request =
        format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(authorization) = authorization {
        request.push_str(&format!("Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut buf = Vec::with_capacity(4096);
    stream.read_to_end(&mut buf)?;
    let body_start = find_body_start(&buf)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    String::from_utf8(buf[body_start..].to_vec())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
}

/// Request-level API helper retaining status and headers so auth/CORS/token
/// redaction assertions do not accidentally test only a parsed JSON body.
fn fetch_api_request(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    authorization: Option<&str>,
    cookie: Option<&str>,
    body: Option<&str>,
) -> io::Result<(String, String, String)> {
    fetch_api_request_with_headers(port, method, path, host, authorization, cookie, &[], body)
}

/// Request-level helper for headers whose semantics are part of the API
/// contract (notably Idempotency-Key). It deliberately remains test-only: the
/// production boundary accepts typed DTOs and never exposes arbitrary launch
/// arguments or environment.
#[allow(clippy::too_many_arguments)]
fn fetch_api_request_with_headers(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    authorization: Option<&str>,
    cookie: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> io::Result<(String, String, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    // Replacement and interruption may intentionally wait through the
    // lifecycle's five-second graceful-stop window before forcing the tree.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut request =
        format!("{method} {path} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n");
    if let Some(value) = authorization {
        request.push_str(&format!("Authorization: {value}\r\n"));
    }
    if let Some(value) = cookie {
        request.push_str(&format!("Cookie: {value}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(value) = body {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{value}",
            value.len()
        ));
    } else {
        request.push_str("\r\n");
    }
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let body_start = find_body_start(&raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..body_start]).to_string();
    let status = head.lines().next().unwrap_or_default().to_string();
    let body = String::from_utf8(raw[body_start..].to_vec())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    Ok((status, head, body))
}

/// Session injection (docs/architecture/session-inject.md): POST one
/// injection to the running session and return the parsed reply.
fn post_inject(port: u16, id: &str, body: &str) -> (String, serde_json::Value) {
    let (status, _, reply) = fetch_api_request(
        port,
        "POST",
        &format!("/v1/sessions/{id}/inject"),
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(body),
    )
    .unwrap();
    (status, serde_json::from_str(&reply).unwrap_or_default())
}

fn spawn_inject_daemon(
    temp: &std::path::Path,
) -> (u16, super::super::api_sessions::ApiSessionStore) {
    let store = super::super::api_sessions::ApiSessionStore::new(temp);
    let lifecycle = std::sync::Arc::new(
        super::super::api_session_lifecycle::ApiSessionLifecycle::new(store.clone()),
    );
    let port = spawn_dashboard_with_activity_and_lifecycle(
        temp.to_path_buf(),
        None,
        9999,
        100,
        empty_live_provider(),
        TelemetryStore::new(),
        ToolTelemetryStore::new(),
        "dashboard-token".to_string(),
        "api-token".to_string(),
        None,
        None,
        lifecycle,
    )
    .unwrap();
    (port, store)
}

#[test]
fn tier1_injection_mid_turn_is_delivered_once_as_the_next_user_turn() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let script = temp.path().join("claude.jsonl");
    let mut stream = String::from(
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"provider-inj\"}\n",
    );
    stream.push_str(&"{\"type\":\"future.event\"}\n".repeat(6));
    std::fs::write(&script, stream).unwrap();
    let _mock_guard = super::super::api_session_http::set_test_headless_executable(
        http_test_mock_agent().to_string_lossy().into_owned(),
        vec![
            "--mock-stream-json".to_string(),
            script.to_string_lossy().into_owned(),
            "--mock-stream-delay-ms".to_string(),
            "200".to_string(),
            "--mock-stream-echo-argv".to_string(),
        ],
    );
    let (port, store) = spawn_inject_daemon(temp.path());
    let create = format!(
        r#"{{"backend":"claude","cwd":"{}"}}"#,
        cwd.to_string_lossy().replace('\\', "\\\\")
    );
    let (_, _, body) = fetch_api_request(
        port,
        "POST",
        "/v1/sessions",
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(&create),
    )
    .unwrap();
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _, _) = fetch_api_request(
        port,
        "POST",
        &format!("/v1/sessions/{id}/turns"),
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        Some(r#"{"message":"original brief"}"#),
    )
    .unwrap();
    assert_status(&status, "202");

    let inject = r#"{"message":"REFINEMENT-7Q: also add a test","client_msg_id":"c-1","from":"operator","ticket":"T-7"}"#;
    let (status, first) = post_inject(port, &id, inject);
    assert_status(&status, "202");
    assert_eq!(first["accepted"], true);
    assert_eq!(first["state"], "queued", "{first}");
    assert_eq!(first["seq"], 1);
    let (_, again) = post_inject(port, &id, inject);
    assert_eq!(
        (again["seq"].clone(), again["duplicate"].clone()),
        (1.into(), true.into())
    );

    wait_for_api_generation_to_finish(&store, &id, 2);
    std::thread::sleep(Duration::from_millis(500));
    let record = store.get(&id).unwrap();
    assert_eq!(
        record.generation, 2,
        "the duplicate must not start a third turn"
    );
    let entry = &record.injections[0];
    assert_eq!(record.injections.len(), 1);
    assert_eq!(entry.state.as_str(), "delivered");
    assert_eq!(entry.from.as_deref(), Some("operator"));
    assert_eq!(entry.ticket.as_deref(), Some("T-7"));
    let second_turn = &record.turns[1].id;
    assert_eq!(entry.turn_id.as_ref(), Some(second_turn));
    let raw = std::fs::read_to_string(
        temp.path()
            .join("logs")
            .join("api")
            .join(&id)
            .join("2.jsonl"),
    )
    .unwrap();
    assert!(
        raw.contains("REFINEMENT-7Q"),
        "turn 2 must carry the injection: {raw}"
    );
    assert!(
        raw.contains("provider-inj"),
        "turn 2 must resume the same conversation"
    );
    let first_raw = std::fs::read_to_string(
        temp.path()
            .join("logs")
            .join("api")
            .join(&id)
            .join("1.jsonl"),
    )
    .unwrap();
    assert!(!first_raw.contains("REFINEMENT-7Q"));
    assert!(record
        .events
        .iter()
        .any(|event| event.kind == "injection_delivered"));

    let (status, _, _) = fetch_api_request(
        port,
        "DELETE",
        &format!("/v1/sessions/{id}"),
        "127.0.0.1",
        Some("Bearer api-token"),
        None,
        None,
    )
    .unwrap();
    assert_status(&status, "200");
    let (status, sealed) =
        post_inject(port, &id, r#"{"message":"too late","client_msg_id":"c-2"}"#);
    assert_status(&status, "409");
    assert_eq!(sealed["code"], "session_sealed");
}

use super::*;
use crate::backend::{
    Backend, HarnessSelection, LaunchMode, ModelProvider, PreferenceSource, RoutingMode,
};
use crate::command::LaunchPlan;
use crate::daemon::wire_prost::{
    decode_worker_server_line, encode_worker_client_line, DaemonWireFormat,
};
use std::io::BufRead;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

fn cross_route_plan() -> LaunchPlan {
    LaunchPlan {
        unsafe_mode: false,
        allow_plan_mode: false,
        command: vec!["claude".to_string()],
        iterations: 1,
        backend: Backend::Claude,
        routing_mode: RoutingMode::Direct,
        model_provider: Some(ModelProvider::Codex),
        requested_harness: Some(HarnessSelection::Claude),
        effective_harness: Some(Backend::Claude),
        provider_source: Some(PreferenceSource::Cli),
        harness_source: Some(PreferenceSource::Cli),
        launch_mode: LaunchMode::Subprocess,
        cwd: None,
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
        coauthor: crate::attribution::Coauthor::default(),
        route: None,
    }
}

#[test]
fn worker_cross_route_runtime_exposes_bridge_only_to_the_child_environment() {
    let runtime = start_worker_runtime(&cross_route_plan(), &[], &[]).unwrap();
    let env = runtime.env();
    assert!(env.iter().any(|(key, _)| key == "ANTHROPIC_BASE_URL"));
    assert!(env.iter().any(|(key, _)| key == "ANTHROPIC_AUTH_TOKEN"));
    assert!(!env.iter().any(|(key, _)| key == "ANTHROPIC_API_KEY"));
}

fn test_shared(tmp: &TempDir) -> Arc<WorkerShared> {
    let snapshot = SessionSnapshot {
        id: "worker-wire-test".to_string(),
        kind: SessionKind::Subprocess,
        backend: None,
        launch_mode: None,
        repo_root: None,
        command: Vec::new(),
        cwd: None,
        name: Some("worker wire test".to_string()),
        created_at: Some(1),
        detachable: true,
        background: true,
        attachable: true,
        repeat_interval_secs: None,
        repeat_next_run_at: None,
        repeat_running: false,
        daemon_pid: std::process::id(),
        worker_pid: std::process::id(),
        worker_port: 0,
        root_pid: None,
        daemon_pid_start: 0,
        worker_pid_start: 0,
        root_pid_start: 0,
        exit_code: None,
        exited_at: None,
        ctrl_c: None,
    };
    Arc::new(WorkerShared::new(
        tmp.path().to_path_buf(),
        "worker-wire-test".to_string(),
        snapshot,
    ))
}

fn test_runtime() -> SessionRuntime {
    SessionRuntime::Subprocess(Arc::new(NativeProcess::new(ProcessConfig {
        command: subprocess::command_spec_for_subprocess(vec![
            "__clud_unstarted_test_process__".to_string()
        ]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    })))
}

fn attach_message() -> WorkerClientMessage {
    WorkerClientMessage::Attach {
        terminal: None,
        rows: Some(24),
        cols: Some(80),
    }
}

fn write_client_message(
    stream: &mut TcpStream,
    message: &WorkerClientMessage,
    format: DaemonWireFormat,
) {
    let bytes = encode_worker_client_line(message, format).unwrap();
    stream.write_all(&bytes).unwrap();
    stream.flush().unwrap();
}

fn spawn_worker_handler(
    shared: Arc<WorkerShared>,
) -> (TcpStream, thread::JoinHandle<io::Result<()>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handle_worker_client(
            stream,
            &shared,
            &test_runtime(),
            &GraphicsConfig::default(),
            SessionKind::Subprocess,
            24,
            80,
        )
    });
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    (stream, handle)
}

fn read_server_message(reader: &mut BufReader<TcpStream>) -> (String, WorkerServerMessage) {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let message = decode_worker_server_line(&line).unwrap();
    (line, message)
}

#[test]
fn worker_attach_live_path_preserves_json_wire() {
    let tmp = TempDir::new().unwrap();
    let shared = test_shared(&tmp);
    let (mut stream, handle) = spawn_worker_handler(shared);

    write_client_message(&mut stream, &attach_message(), DaemonWireFormat::Json);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let (line, message) = read_server_message(&mut reader);

    assert!(line.starts_with(r#"{"op":"attached""#), "{line:?}");
    assert!(matches!(message, WorkerServerMessage::Attached { .. }));
    drop(reader);
    drop(stream);
    assert!(handle.join().unwrap().is_ok());
}

#[test]
fn worker_attach_live_path_accepts_prost_messages() {
    let tmp = TempDir::new().unwrap();
    let shared = test_shared(&tmp);
    let (mut stream, handle) = spawn_worker_handler(Arc::clone(&shared));

    write_client_message(&mut stream, &attach_message(), DaemonWireFormat::Prost);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let (line, message) = read_server_message(&mut reader);

    assert!(line.starts_with("CLUD-FRAME/1 434c5544 "), "{line:?}");
    assert!(matches!(message, WorkerServerMessage::Attached { .. }));

    write_client_message(
        &mut stream,
        &WorkerClientMessage::Input {
            data_b64: base64::engine::general_purpose::STANDARD.encode(b"abc"),
            submit: false,
        },
        DaemonWireFormat::Prost,
    );
    write_client_message(
        &mut stream,
        &WorkerClientMessage::Resize {
            rows: 30,
            cols: 100,
        },
        DaemonWireFormat::Prost,
    );
    write_client_message(
        &mut stream,
        &WorkerClientMessage::Interrupt {
            profile: Some(CtrlCProfile {
                cli_pid: Some(77),
                fast_path: true,
                ..CtrlCProfile::default()
            }),
        },
        DaemonWireFormat::Prost,
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = shared.snapshot();
        if snapshot
            .ctrl_c
            .as_ref()
            .is_some_and(|profile| profile.cli_pid == Some(77) && profile.fast_path)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "prost interrupt frame was not decoded by the worker attach loop"
        );
        thread::sleep(Duration::from_millis(25));
    }

    drop(reader);
    drop(stream);
    assert!(handle.join().unwrap().is_ok());
}

// ---------------------------------------------------------------------------
// #1142: a worker whose daemon died must leave its accept loop.
// ---------------------------------------------------------------------------

/// The deadlock, stated as the truth table that produced it.
///
/// A client attached when the daemon dies used to pin the worker forever:
/// `broadcast_exit` sets `stop_accepting`, which stops the eviction thread,
/// so `has_client()` can never go false again and the old
/// `stop_accepting && !has_client()` condition never fires.
#[test]
fn a_dead_daemon_stops_the_loop_even_with_a_client_attached() {
    assert!(
        should_stop_accepting(true, true, true, false),
        "this is the 8-and-15-hour leak: daemon gone, client stuck attached"
    );
}

/// The daemon's death is decisive on its own. The watchdog sets the flag after
/// `broadcast_exit`, but the loop must not depend on having observed both.
#[test]
fn a_dead_daemon_stops_the_loop_on_its_own() {
    assert!(should_stop_accepting(true, false, false, false));
    assert!(should_stop_accepting(true, false, true, false));
}

/// The graceful drain is untouched. The child exited and a client is still
/// reading its final output, so the loop stays up — removing this would trade
/// the leak for truncated output at every normal exit.
#[test]
fn a_graceful_exit_still_waits_for_the_client_to_finish_reading() {
    assert!(
        !should_stop_accepting(false, true, true, false),
        "a live daemon plus an attached client must keep draining"
    );
    assert!(
        should_stop_accepting(false, true, false, false),
        "once the client is gone there is nothing left to drain"
    );
}

/// A healthy, idle worker keeps accepting.
#[test]
fn a_running_worker_keeps_accepting() {
    assert!(!should_stop_accepting(false, false, false, false));
    assert!(!should_stop_accepting(false, false, true, false));
}

/// The eviction thread shares this predicate, so this case is doing double
/// duty: it says the accept loop keeps draining, and it says the *evictor*
/// keeps running while it does.
///
/// That second reading is the fix for the other half of #1142. The evictor
/// used to stop the instant `stop_accepting` was set, which is the instant
/// shutdown begins — so a client whose socket had already died could never be
/// cleared, `has_client()` stayed true, and the accept loop waited on it
/// forever. Reachable with a perfectly healthy daemon, which is why the
/// daemon-death flag alone does not cover it.
#[test]
fn the_evictor_outlives_the_start_of_shutdown() {
    assert!(
        !should_stop_accepting(false, true, true, false),
        "the evictor must still be running here, or the client that pins \
         has_client() can never be cleared"
    );
}

/// #1582: a child that exits before the launching client connects must not
/// take the worker's port with it. The worker used to stop accepting the
/// moment the child exited with no client attached, so a client still busy
/// with pre-attach work (Codex model discovery) found the port closed and
/// reported "worker has died" for a session that had succeeded.
#[test]
fn an_exit_before_the_first_attach_keeps_the_worker_accepting() {
    assert!(
        !should_stop_accepting(false, true, false, true),
        "no client has attached yet: the exit code must wait for one"
    );
    assert!(
        should_stop_accepting(true, true, false, true),
        "a dead daemon still ends the loop"
    );
}

#[test]
fn the_first_attach_window_opens_at_exit_and_closes_on_attach_or_grace() {
    let tmp = TempDir::new().unwrap();
    let shared = test_shared(&tmp);
    let grace = std::time::Duration::from_secs(30);
    shared.broadcast_exit(0);
    assert!(
        shared.awaiting_first_client(grace),
        "the child exited before any client attached"
    );
    assert!(
        !shared.awaiting_first_client(std::time::Duration::ZERO),
        "the wait is bounded, so a --detach session with no client still ends"
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (client_id, ..) = shared.attach_client(stream).unwrap();
    shared.detach_client(client_id);
    assert!(
        !shared.awaiting_first_client(grace),
        "once a client has attached, the normal drain rule applies"
    );
}

/// #1628: a `--detach` launch never attaches, so its exited worker must not
/// sit out the 30 s first-attach grace (the Windows daemon integration tests
/// waited 10 s for it to exit and failed).
#[test]
fn a_background_launch_has_no_first_attach_grace() {
    let tmp = TempDir::new().unwrap();
    let shared = test_shared(&tmp);
    shared.broadcast_exit(17);
    assert!(!shared.awaiting_first_client(first_attach_grace(true)));
    assert!(should_stop_accepting(
        false,
        true,
        false,
        shared.awaiting_first_client(first_attach_grace(true)),
    ));
    assert!(
        shared.awaiting_first_client(first_attach_grace(false)),
        "a foreground launcher is still on its way to attach"
    );
}

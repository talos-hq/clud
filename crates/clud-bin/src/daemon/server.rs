use super::startup_phases::StartupPhases;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
use serde_json::json;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};

use crate::orphan_reaper;
use crate::win_creation_flags::invisible_helper_creationflags;

use super::activity::{should_idle_shutdown, DaemonActivity};
use super::checkout_claims::{CheckoutClaimRegistry, CheckoutRequest};
use super::client::cleanup_stale_state;
use super::client_leases::ClientLeaseRegistry;
use super::daemon_events;
use super::gc_service::{
    spawn_registry_worker_for_state, GcRequestMsg, RegistryMsg, WORKER_REPLY_TIMEOUT,
};
use super::http::{
    default_live_sessions_provider, spawn_dashboard_with_activity_and_lifecycle, TelemetryStore,
    ToolTelemetryStore,
};
use super::io_helpers::{new_session_id, read_json_file, write_json_file};
use super::paths::{
    daemon_events_path, daemon_info_path, session_snapshot_path, sessions_dir, spec_path, specs_dir,
};
use super::proc_sampler::{spawn_proc_sampler, ProcSamplerHandle, DEFAULT_SAMPLE_INTERVAL_MS};
use super::process_utils::{identity_is_alive, signal_process_tree_as};
use super::runtime_config::{DaemonRuntimeConfig, TestExpiryReason, TestRuntimeActivity};
use super::session_flags::{
    handle_session_flag_connection, SessionFlagRegistry, SessionFlagRequest,
};
use super::sessions::list_live_session_cwds;
use super::types::{
    unix_millis_now, CtrlCProfile, DaemonInfo, DaemonRequest, DaemonResponse, GcReply,
    SessionSnapshot, WorkerLaunchSpec,
};
use super::wire_prost::{
    decode_daemon_request_line, encode_daemon_response_line, DaemonWireFormat, WireError,
};

fn current_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn spawn_tool_installer() {
    let _ = thread::Builder::new()
        .name("clud-tool-install".to_string())
        .spawn(crate::tool_install::ensure_installed);
}

/// #2142: why the accept loop should stop without a `Shutdown` request.
/// `interrupted` is flipped by the process-wide SIGTERM/SIGHUP/SIGQUIT/SIGINT
/// handlers, which replace the default "die" disposition, so a daemon that
/// never polls it ignores `kill -TERM`. A daemon whose `daemon.json` now names
/// another pid has been superseded and must release `data.redb` for it.
/// The disk read is throttled to once a second.
fn stop_reason(
    interrupted: &AtomicBool,
    state_dir: &Path,
    last_check: &mut Instant,
) -> Option<&'static str> {
    if interrupted.load(Ordering::SeqCst) {
        return Some("daemon_signal_shutdown");
    }
    if last_check.elapsed() < Duration::from_secs(1) {
        return None;
    }
    *last_check = Instant::now();
    let on_disk = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)).ok()?;
    is_superseded(on_disk.pid, std::process::id()).then_some("daemon_superseded_shutdown")
}

fn is_superseded(recorded_pid: u32, own_pid: u32) -> bool {
    recorded_pid != own_pid
}

#[cfg(test)]
pub(super) fn run_daemon_uninterrupted(state_dir: &Path) -> i32 {
    run_daemon(state_dir, &AtomicBool::new(false))
}

/// Bind the IPC listener on an ephemeral loopback port (`loopback_bind`).
fn bind_loopback(phases: &StartupPhases) -> Result<(TcpListener, u16), i32> {
    let fail = |what: &str, err: io::Error| phases.fail("loopback_bind", &format!("{what}: {err}"));
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| fail("failed to bind daemon listener", err))?;
    let port = listener
        .local_addr()
        .map_err(|err| fail("failed to read daemon listener addr", err))?
        .port();
    phases.mark("loopback_bind");
    Ok((listener, port))
}

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub(super) fn run_daemon(state_dir: &Path, interrupted: &AtomicBool) -> i32 {
    // Retag the crash reporter installed by main.rs so any crash inside the
    // daemon process gets written under role="daemon". `install_native`
    // also ensures the SIGSEGV / SIGBUS / SIGILL / SIGFPE / SIGABRT /
    // Windows-SEH handler is in place — both panic and native crashes go
    // to `~/.clud/state/crashes/`.
    crate::crash_report::install_native("daemon");
    let runtime_config = DaemonRuntimeConfig::from_env();
    // Settings are startup-only daemon policy: never read the settings file on
    // the listener's idle path.
    let production_idle_timeout = production_idle_timeout();
    let daemon_started_at = Instant::now();
    let activity = DaemonActivity::new(daemon_started_at);
    let test_activity = runtime_config
        .test_mode
        .then(|| TestRuntimeActivity::new(daemon_started_at));
    if let Err(err) = fs::create_dir_all(state_dir) {
        eprintln!("[clud] failed to create daemon state dir: {}", err);
        return 1;
    }
    let phases = StartupPhases::new(state_dir, daemon_started_at);
    daemon_events::log_event(state_dir, "daemon_starting", []);
    daemon_events::log_event(
        state_dir,
        "daemon_runtime_profile",
        [
            ("test_mode", json!(runtime_config.test_mode)),
            (
                "test_max_lifetime_secs",
                json!(runtime_config
                    .test_max_lifetime
                    .map(|value| value.as_secs())),
            ),
            (
                "test_idle_timeout_secs",
                json!(runtime_config
                    .test_idle_timeout
                    .map(|value| value.as_secs())),
            ),
            (
                "host_scans_enabled",
                json!(runtime_config.host_scans_enabled),
            ),
            (
                "periodic_maintenance_enabled",
                json!(runtime_config.periodic_maintenance_enabled),
            ),
            (
                "production_idle_timeout_secs",
                json!(production_idle_timeout.map(|value| value.as_secs())),
            ),
        ],
    );

    cleanup_stale_state(state_dir);
    phases.mark("stale_state_cleanup");
    // A new daemon never adopts a persisted provider PID. Seal active turns
    // before accepting either HTTP or RPC lifecycle mutations.
    let api_lifecycle = Arc::new(
        super::api_session_lifecycle::ApiSessionLifecycle::with_activity(
            super::api_sessions::ApiSessionStore::new(state_dir),
            activity.clone(),
        ),
    );
    // GC finished API sessions before reconcile, so a turn interrupted by
    // this restart stays resumable. Idle sessions are never reaped.
    if let Err(error) = super::api_sessions::ApiSessionStore::new(state_dir).gc() {
        daemon_events::log_event(
            state_dir,
            "api_session_gc_failed",
            [("error", json!(error.to_string()))],
        );
    }
    if let Err(error) =
        super::api_sessions::ApiSessionStore::new(state_dir).reconcile_after_restart()
    {
        daemon_events::log_event(
            state_dir,
            "api_session_recovery_failed",
            [("error", json!(error.to_string()))],
        );
    }
    phases.mark("api_session_reconcile");

    let (listener, port) = match bind_loopback(&phases) {
        Ok(bound) => bound,
        Err(code) => return code,
    };
    // Issue #135: GC registry worker is in-process now. Failing to open
    // the registry is non-fatal — session ops still work; only GC ops
    // will error back to the CLI. Log once and continue.
    let gc_tx = match spawn_registry_worker_for_state(
        state_dir.to_path_buf(),
        runtime_config.periodic_maintenance_enabled,
        Some(activity.clone()),
    ) {
        Ok(tx) => Some(tx),
        Err(err) => {
            eprintln!("[clud] note: gc registry unavailable: {}", err);
            None
        }
    };
    phases.mark("gc_registry_worker");

    // Issue #183: in-process HTTP dashboard. Bind a second loopback port
    // alongside the IPC listener and run a `tiny_http` server on a worker
    // thread. The port is recorded in `daemon.json` so `clud ui` can
    // discover it. Bind failures are non-fatal — IPC keeps working;
    // `dashboard_port` is just `None` on this daemon instance.
    let started_at_unix = current_unix();
    // Issue #469: in-memory telemetry sink for `clud log` POSTs. Lives
    // only for the daemon's lifetime — restart wipes it. Persistence
    // is a follow-up once the prototype contract stabilizes.
    let telemetry = TelemetryStore::new();
    let tool_telemetry = ToolTelemetryStore::new();
    let dashboard_token = crate::dashboard_auth::generate_token();
    let api_token = crate::dashboard_auth::generate_token();
    let dashboard_port = spawn_dashboard_with_activity_and_lifecycle(
        state_dir.to_path_buf(),
        gc_tx.clone(),
        port,
        started_at_unix,
        default_live_sessions_provider(),
        telemetry,
        tool_telemetry,
        dashboard_token.clone(),
        api_token.clone(),
        test_activity.clone(),
        Some(activity.clone()),
        Arc::clone(&api_lifecycle),
    );
    phases.mark("dashboard_bind");

    // Workers inherit these private capability values and use them for their
    // telemetry POSTs. The value is never returned by a dashboard endpoint.
    if let Some(dashboard_port) = dashboard_port {
        // SAFETY: daemon startup completes before it spawns worker threads, so
        // no concurrent environment readers can observe a partial update.
        unsafe {
            std::env::set_var(
                crate::log_event::ENV_DAEMON_HTTP_SERVER,
                format!("http://127.0.0.1:{dashboard_port}"),
            );
            std::env::set_var(crate::log_event::ENV_DAEMON_HTTP_TOKEN, &dashboard_token);
        }
    }

    // Tool installation is deferred until after readiness. `clud tool` self-heals
    // its requested file inline, so daemon bringup no longer blocks callers
    // on a full bundled-tool scan.

    let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));
    let client_leases = ClientLeaseRegistry::default();
    let checkout_claims = CheckoutClaimRegistry::new(Duration::from_secs(2));
    let session_flags = SessionFlagRegistry::default();
    let mut last_client_lease_prune = Instant::now();
    let mut last_stop_check = Instant::now();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    // #933: a daemon's base environment is the OS/login environment, not the
    // shell that won daemon auto-start. Refresh it on admissions and on this
    // timer; each worker receives an immutable admission snapshot.
    super::login_env::start_refreshing(Arc::clone(&shutdown_requested));
    // #548: one host environment pass, shared. The proc sampler and the
    // periodic orphan sweep both need the `RUNNING_PROCESS_ORIGINATOR` /
    // daemon-marker populations, and each used to take its own full-host
    // `ReadProcessMemory` walk of every process's PEB on its own cadence.
    // Neither owns the cache: whichever finds it older than its own tolerance
    // refreshes it, which matters because a parked sampler skips its scan
    // entirely (#720) and leaves the sweep as the only scanner on an idle box.
    let env_scans = Arc::new(crate::process_scan::EnvScanCache::new());
    let proc_sampler = if runtime_config.host_scans_enabled {
        daemon_events::log_event(state_dir, "proc_sampler_started", []);
        spawn_proc_sampler(
            state_dir.to_path_buf(),
            Arc::clone(&shutdown_requested),
            Arc::clone(&env_scans),
        )
    } else {
        ProcSamplerHandle::empty(DEFAULT_SAMPLE_INTERVAL_MS)
    };
    if let Err(err) = listener.set_nonblocking(true) {
        eprintln!("[clud] failed to configure daemon listener: {}", err);
        return 1;
    }

    // running-process consumer adoption (upstream #385): serve the broker
    // v1 frame lane (BackendHandle identity probes + clud payload frames
    // under protocol 0x7C4C) on a local-socket endpoint next to the TCP
    // listener. Best-effort: failure to bind never takes the daemon down,
    // and RUNNING_PROCESS_DISABLE=1 skips the lane entirely.
    let rp_lane = super::rp_broker::spawn_frame_lane(
        state_dir,
        Arc::clone(&workers),
        gc_tx.clone(),
        Arc::clone(&shutdown_requested),
        proc_sampler.clone(),
        client_leases.clone(),
        activity.clone(),
        Arc::clone(&api_lifecycle),
    );

    // #2239: daemon.json is the readiness signal clients and tests poll, so
    // it is published only after the frame lane has written its identity
    // sidecar. Publishing it earlier let a loaded host observe a ready
    // daemon whose sidecar did not exist yet.
    let info = DaemonInfo {
        pid: std::process::id(),
        pid_start: crate::process_identity::self_start_time(),
        port,
        dashboard_port,
        dashboard_token: dashboard_port.map(|_| dashboard_token),
        api_token: dashboard_port.map(|_| api_token),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };
    if let Err(err) = write_json_file(&daemon_info_path(state_dir), &info) {
        return phases.fail(
            "daemon_info_write",
            &format!("failed to persist daemon info: {err}"),
        );
    }
    phases.mark("daemon_info_write");
    daemon_events::log_event(
        state_dir,
        "daemon_started",
        [
            ("port", json!(port)),
            ("dashboard_port", json!(dashboard_port)),
            ("version", json!(env!("CARGO_PKG_VERSION"))),
            ("event_log", json!(daemon_events_path(state_dir))),
        ],
    );
    spawn_tool_installer();

    // Periodic orphan sweep. Catches CLUD-tagged descendants whose
    // originator clud was SIGKILL'd and never ran its own exit hook (so
    // the on-exit `ReapOrphans` IPC never fired). Pure background work —
    // never blocks the accept loop. Sleeps in 1s slices so shutdown is
    // promptly observed.
    if runtime_config.host_scans_enabled {
        daemon_events::log_event(state_dir, "orphan_sweeper_started", []);
        spawn_orphan_sweeper(
            state_dir.to_path_buf(),
            Arc::clone(&shutdown_requested),
            Arc::clone(&env_scans),
        );
    }

    // #547: publish the CPU-alert state instead of answering one poll per open
    // terminal for it. One sampler here replaces M clients each opening a
    // connection every 2 s, and it writes only when the alert-relevant state
    // actually changes -- so an idle daemon writes nothing at all.
    super::cpu_alert_publish::spawn_cpu_alert_publisher(
        state_dir.to_path_buf(),
        Arc::clone(&shutdown_requested),
        sample_daemon_cpu_pct,
    );

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                // The listener is non-blocking so this loop can poll for
                // shutdown. On macOS and Windows an accepted socket inherits
                // that mode (Linux does not), so the handler's first read
                // could fail with WouldBlock before the client's request
                // arrived and the client saw an empty reply.
                if let Err(error) = stream.set_nonblocking(false) {
                    eprintln!(
                        "[clud-daemon] dropping connection: cannot make it blocking: {error}"
                    );
                    continue;
                }
                let activity_guard = test_activity
                    .as_ref()
                    .map(TestRuntimeActivity::start_request);
                let workers = Arc::clone(&workers);
                let state_dir = state_dir.to_path_buf();
                let gc_tx = gc_tx.clone();
                let shutdown_requested = Arc::clone(&shutdown_requested);
                let proc_sampler = proc_sampler.clone();
                let client_leases = client_leases.clone();
                let checkout_claims = checkout_claims.clone();
                let session_flags = session_flags.clone();
                let activity = activity.clone();
                let api_lifecycle = Arc::clone(&api_lifecycle);
                thread::spawn(move || {
                    let _connection_guard = activity.start_connection();
                    let _activity_guard = activity_guard;
                    let _ = handle_daemon_connection(
                        stream,
                        &TcpConnectionServices {
                            state_dir: &state_dir,
                            workers: &workers,
                            gc_tx: gc_tx.as_ref(),
                            shutdown_requested: &shutdown_requested,
                            proc_sampler: &proc_sampler,
                            client_leases: &client_leases,
                            checkout_claims: &checkout_claims,
                            session_flags: &session_flags,
                            activity: &activity,
                            api_lifecycle: &api_lifecycle,
                        },
                    );
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if shutdown_requested.load(Ordering::SeqCst) {
                    break;
                }
                let now = Instant::now();
                if let Some(reason) = stop_reason(interrupted, state_dir, &mut last_stop_check) {
                    daemon_events::log_event(state_dir, reason, []);
                    shutdown_requested.store(true, Ordering::SeqCst);
                    break;
                }
                if now.duration_since(last_client_lease_prune) >= Duration::from_secs(1) {
                    for identity in client_leases.prune_dead() {
                        daemon_events::log_event(
                            state_dir,
                            "client_lease_pruned",
                            [
                                ("client_pid", json!(identity.pid)),
                                ("client_pid_start", json!(identity.start_time)),
                                ("lease_count", json!(client_leases.len())),
                            ],
                        );
                    }
                    last_client_lease_prune = now;
                }
                let worker_count = workers.lock().expect("workers mutex poisoned").len();
                let activity_snapshot = activity.snapshot(now);
                if worker_count > 0 {
                    activity.note_activity(now);
                    if let Some(activity) = &test_activity {
                        activity.note_activity(now);
                    }
                }
                let (idle_for, active_requests) = test_activity
                    .as_ref()
                    .map(|activity| activity.snapshot(now))
                    .unwrap_or((Duration::ZERO, 0));
                if let Some(reason) = runtime_config.test_expiry_reason(
                    now.duration_since(daemon_started_at),
                    idle_for,
                    worker_count,
                    active_requests,
                ) {
                    let (op, timeout) = match reason {
                        TestExpiryReason::Idle => {
                            ("daemon_test_idle_expired", runtime_config.test_idle_timeout)
                        }
                        TestExpiryReason::MaxLifetime => (
                            "daemon_test_max_lifetime_expired",
                            runtime_config.test_max_lifetime,
                        ),
                    };
                    daemon_events::log_event(
                        state_dir,
                        op,
                        [
                            ("timeout_secs", json!(timeout.map(|value| value.as_secs()))),
                            (
                                "uptime_ms",
                                json!(now.duration_since(daemon_started_at).as_millis()),
                            ),
                            ("idle_ms", json!(idle_for.as_millis())),
                            ("worker_count", json!(worker_count)),
                            ("active_requests", json!(active_requests)),
                        ],
                    );
                    shutdown_requested.store(true, Ordering::SeqCst);
                    break;
                }
                if !runtime_config.test_mode
                    && should_idle_shutdown(
                        production_idle_timeout,
                        activity_snapshot,
                        worker_count,
                        client_leases.len(),
                    )
                {
                    daemon_events::log_event(
                        state_dir,
                        "daemon_idle_shutdown",
                        [
                            (
                                "timeout_secs",
                                json!(production_idle_timeout.map(|value| value.as_secs())),
                            ),
                            ("idle_ms", json!(activity_snapshot.idle_for.as_millis())),
                            ("worker_count", json!(worker_count)),
                            ("lease_count", json!(client_leases.len())),
                            (
                                "active_connections",
                                json!(activity_snapshot.active_connections),
                            ),
                            ("active_jobs", json!(activity_snapshot.active_jobs)),
                        ],
                    );
                    shutdown_requested.store(true, Ordering::SeqCst);
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                if shutdown_requested.load(Ordering::SeqCst) {
                    break;
                }
                eprintln!("[clud] daemon listener accept failed: {}", err);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    if let Some(lane) = rp_lane {
        lane.cleanup();
    }
    daemon_events::log_event(state_dir, "daemon_stopping", []);
    // A superseded daemon must not delete the successor's `daemon.json`.
    let successor_owns_info = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir))
        .is_ok_and(|on_disk| is_superseded(on_disk.pid, std::process::id()));
    if !successor_owns_info {
        let _ = fs::remove_file(daemon_info_path(state_dir));
    }
    daemon_events::log_event(state_dir, "daemon_stopped", []);
    0
}

struct TcpConnectionServices<'a> {
    state_dir: &'a Path,
    workers: &'a Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    gc_tx: Option<&'a mpsc::Sender<RegistryMsg>>,
    shutdown_requested: &'a Arc<AtomicBool>,
    proc_sampler: &'a ProcSamplerHandle,
    client_leases: &'a ClientLeaseRegistry,
    checkout_claims: &'a CheckoutClaimRegistry,
    session_flags: &'a SessionFlagRegistry,
    activity: &'a DaemonActivity,
    api_lifecycle: &'a Arc<super::api_session_lifecycle::ApiSessionLifecycle>,
}

fn handle_daemon_connection(
    mut stream: TcpStream,
    services: &TcpConnectionServices<'_>,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    if let Ok(request) = serde_json::from_str::<CheckoutRequest>(&line) {
        return handle_checkout_connection(&mut stream, reader, services.checkout_claims, request);
    }
    if let Ok(request) = serde_json::from_str::<SessionFlagRequest>(&line) {
        return handle_session_flag_connection(&mut stream, services.session_flags, request);
    }
    let (request, response_format) = decode_daemon_request_line(&line).map_err(wire_error_to_io)?;
    let request_id = daemon_events::request_id();
    daemon_events::log_event(
        services.state_dir,
        "request_received",
        [
            ("request_id", json!(request_id)),
            ("request_op", json!(request_op(&request))),
            ("wire_format", json!(response_format_name(response_format))),
        ],
    );
    let started = Instant::now();
    let _job_guard = matches!(
        &request,
        DaemonRequest::Gc { .. } | DaemonRequest::ReapOrphans | DaemonRequest::AdoptKill { .. }
    )
    .then(|| services.activity.start_job());
    let response = dispatch_daemon_request_with_id(
        services.state_dir,
        services.workers,
        services.gc_tx,
        Some(services.proc_sampler),
        services.client_leases,
        services.api_lifecycle,
        request_id,
        request,
    );
    let is_shutdown = matches!(response, DaemonResponse::ShutdownAck { .. });
    let result = write_daemon_response(&mut stream, &response, response_format);
    daemon_events::log_event(
        services.state_dir,
        "request_replied",
        [
            ("request_id", json!(request_id)),
            ("response_op", json!(response_op(&response))),
            ("duration_ms", json!(started.elapsed().as_millis())),
            (
                "write_error",
                json!(result.as_ref().err().map(|err| err.to_string())),
            ),
        ],
    );
    if is_shutdown {
        let _ = stream.shutdown(std::net::Shutdown::Write);
        services.shutdown_requested.store(true, Ordering::SeqCst);
    }
    result
}

fn handle_checkout_connection(
    stream: &mut TcpStream,
    mut reader: BufReader<TcpStream>,
    claims: &CheckoutClaimRegistry,
    request: CheckoutRequest,
) -> io::Result<()> {
    let connection_id = daemon_events::request_id();
    let hold_presence = matches!(request, CheckoutRequest::Present { .. });
    let reply = claims.respond(connection_id, request);
    let result = serde_json::to_writer(&mut *stream, &reply)
        .map_err(io::Error::other)
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.flush());
    if result.is_ok() && hold_presence {
        let mut byte = [0u8; 1];
        while matches!(reader.read(&mut byte), Ok(1)) {}
    }
    if hold_presence {
        claims.disconnect(connection_id);
    }
    result
}

/// Map one decoded [`DaemonRequest`] to its [`DaemonResponse`].
///
/// Shared by both transport lanes — the legacy TCP line wire above and
/// the running-process broker v1 frame lane (`rp_broker`) — so request
/// semantics cannot drift between them. Transport concerns (encoding,
/// flagging shutdown after the reply is written) stay with each lane.
#[cfg(test)]
pub(super) fn dispatch_daemon_request(
    state_dir: &Path,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    gc_tx: Option<&mpsc::Sender<RegistryMsg>>,
    request: DaemonRequest,
) -> DaemonResponse {
    let client_leases = ClientLeaseRegistry::default();
    dispatch_daemon_request_with_sampler(state_dir, workers, gc_tx, None, &client_leases, request)
}

#[allow(dead_code)]
pub(super) fn dispatch_daemon_request_with_sampler(
    state_dir: &Path,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    gc_tx: Option<&mpsc::Sender<RegistryMsg>>,
    proc_sampler: Option<&ProcSamplerHandle>,
    client_leases: &ClientLeaseRegistry,
    request: DaemonRequest,
) -> DaemonResponse {
    let lifecycle = Arc::new(super::api_session_lifecycle::ApiSessionLifecycle::new(
        super::api_sessions::ApiSessionStore::new(state_dir),
    ));
    dispatch_daemon_request_with_sampler_and_lifecycle(
        state_dir,
        workers,
        gc_tx,
        proc_sampler,
        client_leases,
        &lifecycle,
        request,
    )
}

pub(super) fn dispatch_daemon_request_with_sampler_and_lifecycle(
    state_dir: &Path,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    gc_tx: Option<&mpsc::Sender<RegistryMsg>>,
    proc_sampler: Option<&ProcSamplerHandle>,
    client_leases: &ClientLeaseRegistry,
    api_lifecycle: &Arc<super::api_session_lifecycle::ApiSessionLifecycle>,
    request: DaemonRequest,
) -> DaemonResponse {
    let request_id = daemon_events::request_id();
    dispatch_daemon_request_with_id(
        state_dir,
        workers,
        gc_tx,
        proc_sampler,
        client_leases,
        api_lifecycle,
        request_id,
        request,
    )
}

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn dispatch_daemon_request_with_id(
    state_dir: &Path,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    gc_tx: Option<&mpsc::Sender<RegistryMsg>>,
    proc_sampler: Option<&ProcSamplerHandle>,
    client_leases: &ClientLeaseRegistry,
    api_lifecycle: &Arc<super::api_session_lifecycle::ApiSessionLifecycle>,
    request_id: u64,
    request: DaemonRequest,
) -> DaemonResponse {
    match request {
        DaemonRequest::Create { spec } => match daemon_create_session(state_dir, workers, *spec) {
            Ok(session) => DaemonResponse::Created { session },
            Err(err) => DaemonResponse::Error {
                message: err.to_string(),
            },
        },
        DaemonRequest::Session { session_id } => {
            match read_json_file::<SessionSnapshot>(&session_snapshot_path(state_dir, &session_id))
            {
                Ok(session) => DaemonResponse::Session { session },
                Err(err) => DaemonResponse::Error {
                    message: err.to_string(),
                },
            }
        }
        DaemonRequest::ListLiveCwds => DaemonResponse::LiveCwds {
            paths: list_live_session_cwds(state_dir),
        },
        DaemonRequest::Terminate { session_id } => {
            match daemon_terminate_session(state_dir, workers, &session_id) {
                Ok(session) => DaemonResponse::Terminated { session },
                Err(err) => DaemonResponse::Error {
                    message: err.to_string(),
                },
            }
        }
        DaemonRequest::Interrupt {
            session_id,
            profile,
        } => match daemon_interrupt_session(state_dir, workers, &session_id, profile) {
            Ok(session) => DaemonResponse::Interrupted { session },
            Err(err) => DaemonResponse::Error {
                message: err.to_string(),
            },
        },
        DaemonRequest::ApiSessionKill { session_id } => match api_lifecycle.kill(&session_id) {
            Ok(_) => DaemonResponse::ApiSessionKilled { session_id },
            Err(error) => DaemonResponse::Error {
                message: format!("{error:?}"),
            },
        },
        DaemonRequest::AdoptKill { pids, reason } => {
            daemon_events::log_event(
                state_dir,
                "adopt_kill_accepted",
                [
                    ("request_id", json!(request_id)),
                    ("pids", json!(pids)),
                    ("reason", json!(reason)),
                ],
            );
            spawn_adopt_kill_worker(state_dir.to_path_buf(), pids.clone(), reason);
            DaemonResponse::AdoptKillAck {
                accepted: pids.len(),
            }
        }
        DaemonRequest::ReapOrphans => {
            // Fire-and-forget: spawn the sweep on a background thread so the
            // CLI's exit path never blocks on `kill_tree`. Ack with zeros —
            // the foreground caller doesn't wait for the actual count.
            daemon_events::log_event(
                state_dir,
                "reap_orphans_accepted",
                [("request_id", json!(request_id))],
            );
            spawn_orphan_reap_once(state_dir.to_path_buf(), "request", Some(request_id));
            DaemonResponse::ReapOrphansAck {
                found: 0,
                reaped: 0,
            }
        }
        DaemonRequest::Metrics => DaemonResponse::Metrics {
            pid: std::process::id(),
            cpu_pct: sample_daemon_cpu_pct(),
        },
        DaemonRequest::ProcSnapshot {
            include_dead_since_ms,
        } => {
            let snapshot = proc_sampler
                .cloned()
                .unwrap_or_else(|| ProcSamplerHandle::empty(DEFAULT_SAMPLE_INTERVAL_MS))
                .snapshot(include_dead_since_ms);
            DaemonResponse::ProcSnapshot { snapshot }
        }
        DaemonRequest::AcquireClientLease { identity } => match client_leases.acquire(identity) {
            Ok(lease_count) => {
                daemon_events::log_event(
                    state_dir,
                    "client_lease_acquired",
                    [
                        ("client_pid", json!(identity.pid)),
                        ("client_pid_start", json!(identity.start_time)),
                        ("lease_count", json!(lease_count)),
                    ],
                );
                DaemonResponse::ClientLeaseAcquired { lease_count }
            }
            Err(message) => DaemonResponse::Error {
                message: message.to_string(),
            },
        },
        DaemonRequest::ReleaseClientLease { identity } => {
            let lease_count = client_leases.release(identity);
            daemon_events::log_event(
                state_dir,
                "client_lease_released",
                [
                    ("client_pid", json!(identity.pid)),
                    ("client_pid_start", json!(identity.start_time)),
                    ("lease_count", json!(lease_count)),
                ],
            );
            DaemonResponse::ClientLeaseReleased { lease_count }
        }
        DaemonRequest::Gc { payload } => {
            let reply = dispatch_gc_op(state_dir, gc_tx, request_id, payload);
            DaemonResponse::Gc { reply }
        }
        DaemonRequest::Shutdown {
            client_version,
            expected_daemon,
        } => shutdown_response(client_version.as_deref(), expected_daemon),
    }
}

fn shutdown_response(
    _client_version: Option<&str>,
    expected_daemon: Option<crate::process_identity::ProcessIdentity>,
) -> DaemonResponse {
    let actual = crate::process_identity::ProcessIdentity::new(
        std::process::id(),
        crate::process_identity::self_start_time(),
    );
    if expected_daemon.is_some_and(|expected| expected != actual) {
        return DaemonResponse::Error {
            message: format!(
                "refusing shutdown for a different daemon generation; running daemon is pid {} started at {}",
                actual.pid, actual.start_time
            ),
        };
    }
    DaemonResponse::ShutdownAck { pid: actual.pid }
}

fn write_daemon_response(
    stream: &mut TcpStream,
    response: &DaemonResponse,
    format: DaemonWireFormat,
) -> io::Result<()> {
    let bytes = encode_daemon_response_line(response, format).map_err(wire_error_to_io)?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn wire_error_to_io(err: WireError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

fn response_format_name(format: DaemonWireFormat) -> &'static str {
    match format {
        DaemonWireFormat::Json => "json",
        DaemonWireFormat::Prost => "prost",
    }
}

fn request_op(request: &DaemonRequest) -> &'static str {
    match request {
        DaemonRequest::Create { .. } => "create",
        DaemonRequest::Session { .. } => "session",
        DaemonRequest::ListLiveCwds => "list_live_cwds",
        DaemonRequest::Terminate { .. } => "terminate",
        DaemonRequest::ApiSessionKill { .. } => "api_session_kill",
        DaemonRequest::Interrupt { .. } => "interrupt",
        DaemonRequest::AdoptKill { .. } => "adopt_kill",
        DaemonRequest::Gc { .. } => "gc",
        DaemonRequest::Shutdown { .. } => "shutdown",
        DaemonRequest::ReapOrphans => "reap_orphans",
        DaemonRequest::Metrics => "metrics",
        DaemonRequest::ProcSnapshot { .. } => "proc_snapshot",
        DaemonRequest::AcquireClientLease { .. } => "acquire_client_lease",
        DaemonRequest::ReleaseClientLease { .. } => "release_client_lease",
    }
}

fn response_op(response: &DaemonResponse) -> &'static str {
    match response {
        DaemonResponse::Created { .. } => "created",
        DaemonResponse::Session { .. } => "session",
        DaemonResponse::LiveCwds { .. } => "live_cwds",
        DaemonResponse::Terminated { .. } => "terminated",
        DaemonResponse::ApiSessionKilled { .. } => "api_session_killed",
        DaemonResponse::Interrupted { .. } => "interrupted",
        DaemonResponse::AdoptKillAck { .. } => "adopt_kill_ack",
        DaemonResponse::Gc { .. } => "gc",
        DaemonResponse::ShutdownAck { .. } => "shutdown_ack",
        DaemonResponse::ReapOrphansAck { .. } => "reap_orphans_ack",
        DaemonResponse::Metrics { .. } => "metrics",
        DaemonResponse::ProcSnapshot { .. } => "proc_snapshot",
        DaemonResponse::ClientLeaseAcquired { .. } => "client_lease_acquired",
        DaemonResponse::ClientLeaseReleased { .. } => "client_lease_released",
        DaemonResponse::Error { .. } => "error",
    }
}

fn sample_daemon_cpu_pct() -> f32 {
    static SAMPLER: std::sync::OnceLock<Mutex<System>> = std::sync::OnceLock::new();
    let mut sys = SAMPLER
        .get_or_init(|| Mutex::new(System::new()))
        .lock()
        .expect("daemon metrics sampler poisoned");
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cpu(),
    );
    sys.process(pid).map(|proc| proc.cpu_usage()).unwrap_or(0.0)
}

fn gc_reply_op(reply: &GcReply) -> &'static str {
    match reply {
        GcReply::WatchOk => "watch_ok",
        GcReply::ListOk { .. } => "list_ok",
        GcReply::PurgeOk { .. } => "purge_ok",
        GcReply::PurgeStarted { .. } => "purge_started",
        GcReply::ReconcileOk { .. } => "reconcile_ok",
        GcReply::InsertOk { .. } => "insert_ok",
        GcReply::RepoVisitOk => "repo_visit_ok",
        GcReply::RepoVisitsOk { .. } => "repo_visits_ok",
        GcReply::CreatedInsertOk { .. } => "created_insert_ok",
        GcReply::CreatedRowsOk { .. } => "created_rows_ok",
        GcReply::Error { .. } => "error",
    }
}

fn should_journal_reply(reply: &GcReply) -> bool {
    match reply {
        GcReply::WatchOk => false,
        GcReply::Error { .. } | GcReply::PurgeOk { .. } | GcReply::PurgeStarted { .. } => true,
        GcReply::ReconcileOk { inserted } => *inserted > 0,
        GcReply::InsertOk { inserted } => *inserted,
        GcReply::CreatedInsertOk { .. } => true,
        GcReply::CreatedRowsOk { .. } => false,
        GcReply::ListOk { .. } | GcReply::RepoVisitOk | GcReply::RepoVisitsOk { .. } => false,
    }
}

fn gc_trace_enabled() -> bool {
    std::env::var("CLUD_DAEMON_TRACE_GC").as_deref() == Ok("1")
}

/// Hand a GC op to the registry worker and await the reply. Returns a
/// `GcReply::Error` if the worker is missing (failed to spawn at daemon
/// startup), hung up, or didn't reply within [`WORKER_REPLY_TIMEOUT`].
fn dispatch_gc_op(
    state_dir: &Path,
    gc_tx: Option<&mpsc::Sender<RegistryMsg>>,
    request_id: u64,
    op: super::types::GcOp,
) -> GcReply {
    let trace = gc_trace_enabled();
    if trace {
        daemon_events::log_event(state_dir, "gc_started", [("request_id", json!(request_id))]);
    }
    let started = Instant::now();
    let Some(tx) = gc_tx else {
        let reply = GcReply::Error {
            message: "gc registry unavailable in this daemon".to_string(),
        };
        log_gc_finished(state_dir, request_id, started, &reply, trace);
        return reply;
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel::<GcReply>(1);
    if tx
        .send(RegistryMsg::Op(GcRequestMsg { op, reply_tx }))
        .is_err()
    {
        let reply = GcReply::Error {
            message: "gc registry worker stopped".to_string(),
        };
        log_gc_finished(state_dir, request_id, started, &reply, trace);
        return reply;
    }
    let reply = reply_rx
        .recv_timeout(WORKER_REPLY_TIMEOUT)
        .unwrap_or_else(|_| GcReply::Error {
            message: "gc registry worker timed out".to_string(),
        });
    log_gc_finished(state_dir, request_id, started, &reply, trace);
    reply
}

fn log_gc_finished(
    state_dir: &Path,
    request_id: u64,
    started: Instant,
    reply: &GcReply,
    trace: bool,
) {
    if !trace && !should_journal_reply(reply) {
        return;
    }
    daemon_events::log_event(
        state_dir,
        "gc_finished",
        [
            ("request_id", json!(request_id)),
            ("response_op", json!(gc_reply_op(reply))),
            ("duration_ms", json!(started.elapsed().as_millis())),
        ],
    );
}

/// Env knob for the per-phase worker-spawn budget, in seconds.
///
/// Issue #305: 5s is generous on an idle machine and tight on a loaded one --
/// many parallel rustc invocations, a slow disk, an oversubscribed CI runner.
/// A timeout here is reported to the client as a failure, so a budget that is
/// merely too small reads as a broken worker.
pub(super) const ENV_WORKER_SPAWN_BUDGET_SECS: &str = "CLUD_DAEMON_WORKER_SPAWN_BUDGET_SECS";

/// Default per-phase budget.
///
/// Raised from the original 5s. Waiting longer costs a slower error on a
/// genuinely broken worker; waiting less costs a healthy session reported as
/// failed. The second is worse: the caller cannot tell the two apart, and the
/// retry re-runs the whole spawn -- which is how one slow machine turns into
/// a pile of half-started workers.
const DEFAULT_WORKER_SPAWN_BUDGET: Duration = Duration::from_secs(15);

/// Per-phase budget for worker startup.
///
/// Applied separately to "the worker wrote its snapshot" and "the worker's
/// port accepts connections": they fail for different reasons and each reports
/// its own message.
fn worker_spawn_budget() -> Duration {
    parse_worker_spawn_budget(std::env::var(ENV_WORKER_SPAWN_BUDGET_SECS).ok().as_deref())
}

/// Test seam. A missing, unparseable, or zero value keeps the default rather
/// than yielding a budget that fails instantly -- a typo in an env var must not
/// look like a broken daemon.
fn parse_worker_spawn_budget(raw: Option<&str>) -> Duration {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(DEFAULT_WORKER_SPAWN_BUDGET, Duration::from_secs)
}

/// The worst case [`daemon_create_session`] may legitimately take: two
/// worker phases of [`worker_spawn_budget`] each, plus the login-env refresh
/// and the spec writes. A launcher waiting on `Create` uses this as its
/// deadline, so a stalled admission is reported rather than waited on (#2000).
pub(super) fn create_session_budget() -> Duration {
    const ADMISSION_MARGIN: Duration = Duration::from_secs(10);
    worker_spawn_budget() * 2 + ADMISSION_MARGIN
}

fn log_create_phase(state_dir: &Path, op: &str, session_id: Option<&str>, started: Instant) {
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    daemon_events::log_event(
        state_dir,
        op,
        [
            ("session_id", serde_json::json!(session_id)),
            ("elapsed_ms", serde_json::json!(elapsed_ms)),
        ],
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn daemon_create_session(
    state_dir: &Path,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    mut spec: WorkerLaunchSpec,
) -> io::Result<SessionSnapshot> {
    // Refresh before each admission as the low-latency path after an installer
    // or `setx` update. The timer is only the idle-daemon backstop.
    let admission_started = Instant::now();
    spec.login_env = super::login_env::snapshot_for_new_session();
    // #2000: each admission phase leaves an event, so a launcher that timed
    // out can be matched to the phase the daemon was in.
    log_create_phase(state_dir, "create_login_env", None, admission_started);
    fs::create_dir_all(specs_dir(state_dir))?;
    fs::create_dir_all(sessions_dir(state_dir))?;

    let session_id = new_session_id();
    let spec_path = spec_path(state_dir, &session_id);
    write_json_file(&spec_path, &spec)?;

    // #465 handover registry: a daemon-launched detached/background session's
    // descendants are tagged with this daemon's PID as originator. Persist that
    // PID so a *successor* daemon (after a restart) spares the still-live
    // session instead of reaping it as a dead-originator orphan.
    if spec.detachable || spec.background_on_launch {
        super::handover_registry::register(state_dir, std::process::id());
    }

    let exe = std::env::current_exe()?;
    let worker = Arc::new(NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            exe.to_string_lossy().to_string(),
            "__worker".to_string(),
            "--state-dir".to_string(),
            state_dir.to_string_lossy().to_string(),
            "--session-id".to_string(),
            session_id.clone(),
            "--daemon-pid".to_string(),
            std::process::id().to_string(),
            "--spec-file".to_string(),
            spec_path.to_string_lossy().to_string(),
        ]),
        cwd: None,
        env: Some(std::env::vars().collect()),
        capture: false,
        stderr_mode: StderrMode::Stdout,
        // Issue #55: daemon-helper worker spawn — invisible by design.
        // stdio is `Null` and the user never sees this child's output
        // directly (output is forwarded via TCP to attaching clients),
        // so suppress the conhost window on Windows. No-op elsewhere.
        creationflags: invisible_helper_creationflags(),
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
        // `running-process-core` 3.4 removed the explicit `Containment`
        // knob; every `NativeProcess` is now automatically bound to a
        // kill-on-close Job Object on Windows. The worker can no longer
        // outlive the daemon on Windows, but the worker already polls
        // `pid_is_alive(daemon_pid)` to clean up if the daemon dies, so
        // the OS-level link only tightens that contract.
    }));
    worker
        .start()
        .map_err(|err| io::Error::other(err.to_string()))?;
    log_create_phase(
        state_dir,
        "create_worker_started",
        Some(&session_id),
        admission_started,
    );

    // Issue #305: each startup phase gets its own budget, measured from its
    // own start.
    //
    // Both waits used to share one `Instant` and one 5s bound, so a snapshot
    // that took 4.9s left the readiness check 0.1s -- and the readiness check
    // is the one that reports the failure. Under load that produced "TCP port
    // is not accepting connections" for a worker whose port was fine and whose
    // startup was merely slow, which sends whoever reads it looking at
    // networking instead of at contention.
    let budget = worker_spawn_budget();
    let snapshot_started = Instant::now();
    let mut snapshot = loop {
        match read_json_file::<SessionSnapshot>(&session_snapshot_path(state_dir, &session_id)) {
            Ok(snapshot) => break snapshot,
            Err(err) if snapshot_started.elapsed() < budget => {
                let _ = err;
                thread::sleep(Duration::from_millis(25));
            }
            Err(err) => {
                let _ = err;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "worker for session {session_id} did not write its snapshot within \
                         {:.0}s; the spawn is slow rather than broken under load. Raise \
                         {ENV_WORKER_SPAWN_BUDGET_SECS} to allow longer.",
                        budget.as_secs_f64()
                    ),
                ));
            }
        }
    };

    log_create_phase(
        state_dir,
        "create_snapshot_seen",
        Some(&session_id),
        admission_started,
    );

    // Verify the worker's TCP listener is actually accepting connections
    // before reporting the session as ready. If the backend exits immediately,
    // the worker can persist the final snapshot and close its listener before
    // this readiness loop observes it; that is still a valid created session
    // whose failure can be inspected later.
    if snapshot.attachable {
        let ready_started = Instant::now();
        loop {
            if snapshot.exit_code.is_some() {
                break;
            }
            if TcpStream::connect(("127.0.0.1", snapshot.worker_port)).is_ok() {
                break;
            }
            if let Ok(updated) =
                read_json_file::<SessionSnapshot>(&session_snapshot_path(state_dir, &session_id))
            {
                snapshot = updated;
                if snapshot.exit_code.is_some() {
                    break;
                }
            }
            if ready_started.elapsed() >= budget {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "worker wrote snapshot but TCP port {} is not accepting connections \
                         after {:.0}s",
                        snapshot.worker_port,
                        budget.as_secs_f64()
                    ),
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    log_create_phase(
        state_dir,
        "create_ready",
        Some(&session_id),
        admission_started,
    );
    workers
        .lock()
        .expect("workers mutex poisoned")
        .insert(session_id.clone(), Arc::clone(&worker));
    reap_worker_when_done(
        state_dir.to_path_buf(),
        Arc::clone(workers),
        session_id.clone(),
        worker,
    );
    Ok(snapshot)
}

/// Remove a worker from the registry as soon as its process exits.
///
/// One dedicated waiter thread per worker, blocking on `wait`. That is worth
/// naming because #305 phase 4 proposes adding a 30-second sweep that scans
/// the registry for dead PIDs, on the premise that "today's cleanup is
/// reactive ... only runs on activity". It is not: this waiter is
/// unconditional and fires on process exit regardless of whether any client
/// touches the daemon again. A periodic scan would duplicate it, and #542
/// separately asks the daemon not to grow new fixed-interval work.
fn reap_worker_when_done(
    state_dir: std::path::PathBuf,
    workers: Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    session_id: String,
    worker: Arc<NativeProcess>,
) {
    thread::spawn(move || {
        let started = Instant::now();
        daemon_events::log_event(
            &state_dir,
            "worker_reap_wait_started",
            [("session_id", json!(session_id))],
        );
        let wait_result = worker.wait(None);
        let mut guard = workers.lock().expect("workers mutex poisoned");
        let removed = if guard
            .get(&session_id)
            .is_some_and(|current| Arc::ptr_eq(current, &worker))
        {
            guard.remove(&session_id);
            true
        } else {
            false
        };
        daemon_events::log_event(
            &state_dir,
            "worker_reaped",
            [
                ("session_id", json!(session_id)),
                ("removed", json!(removed)),
                ("duration_ms", json!(started.elapsed().as_millis())),
                (
                    "wait_error",
                    json!(wait_result.err().map(|err| err.to_string())),
                ),
            ],
        );
    });
}

/// Background `kill_tree` for [`DaemonRequest::AdoptKill`].
///
/// The CLI hands us a list of root PIDs it was about to wait on and we
/// finish the job from out here so the foreground `clud` can drop the
/// user back to the shell immediately. Best-effort and fire-and-forget:
/// failures are silent because the parent CLI already returned 130 and
/// nobody is on the other end to receive a reply. The kill walk uses
/// `process_tree::kill_tree` for parity with the synchronous path it
/// replaces (see `runner::teardown_interrupted_child`).
/// Env override for the orphan-sweep period, in milliseconds (#465 AC 1).
const ENV_ORPHAN_SWEEP_INTERVAL_MS: &str = "CLUD_ORPHAN_SWEEP_INTERVAL_MS";
/// Default period between dead-originator orphan sweeps. 60s keeps the
/// full-host env scan (sysinfo + `environ` read for every process) off the
/// idle-CPU budget (#542) while still clearing SIGKILL'd-clud orphans within a
/// minute; the on-exit `Drop` guard and `clud slay` own the fast path.
const DEFAULT_ORPHAN_SWEEP_INTERVAL_MS: u64 = 60_000;
/// Floor so a misconfigured tiny value can't spin the sweep into a hot loop
/// (each sweep reads `environ` for every process on the host).
const MIN_ORPHAN_SWEEP_INTERVAL_MS: u64 = 5_000;

/// Floor for the post-death grace window. Below this the periodic sweep starts
/// racing the on-exit `Drop` guard that owns the same cleanup, which is the
/// thing the window exists to prevent (#465).
const MIN_ORPHAN_GRACE_MS: u64 = 1_000;

pub(super) fn orphan_sweep_interval() -> Duration {
    orphan_sweep_interval_from(
        std::env::var(ENV_ORPHAN_SWEEP_INTERVAL_MS).ok().as_deref(),
        settings_u64("/daemon/orphan_sweep/interval_ms"),
    )
}

/// The grace window a freshly observed orphan gets before the periodic sweep
/// will reap it (#465).
pub(super) fn orphan_sweep_grace() -> Duration {
    orphan_sweep_grace_from(settings_u64("/daemon/orphan_sweep/grace_ms"))
}

/// Read a `u64` out of the global settings document, or `None` if settings are
/// unreadable / the key is absent. A daemon must start with defaults rather
/// than fail because a settings file is missing or malformed.
fn settings_u64(pointer: &str) -> Option<u64> {
    crate::clud_settings::load_or_init_global_settings()
        .ok()?
        .pointer(pointer)
        .and_then(serde_json::Value::as_u64)
}

/// Settings seed the conservative 900-second production default. Missing,
/// malformed, or explicitly zero configuration keeps automatic retirement off.
fn production_idle_timeout() -> Option<Duration> {
    production_idle_timeout_from(settings_u64("/daemon/idle_timeout_secs"))
}

fn production_idle_timeout_from(configured_secs: Option<u64>) -> Option<Duration> {
    configured_secs
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

/// Resolve the sweep interval: env override first, then settings, then the 60s
/// default; zero/garbage at either layer is ignored, and the result is clamped
/// to the floor.
///
/// Env wins over settings deliberately. The env var is the break-glass knob for
/// an incident on a box whose settings file you may not want to edit; settings
/// are the durable configuration. Pure so both layers and the clamp are
/// unit-tested without touching the environment or the filesystem.
fn orphan_sweep_interval_from(raw_env: Option<&str>, from_settings: Option<u64>) -> Duration {
    let ms = raw_env
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .or(from_settings.filter(|&ms| ms > 0))
        .unwrap_or(DEFAULT_ORPHAN_SWEEP_INTERVAL_MS)
        .max(MIN_ORPHAN_SWEEP_INTERVAL_MS);
    Duration::from_millis(ms)
}

/// Resolve the grace window from settings, else [`orphan_reaper::ORPHAN_GRACE_MS`],
/// clamped to [`MIN_ORPHAN_GRACE_MS`].
fn orphan_sweep_grace_from(from_settings: Option<u64>) -> Duration {
    let ms = from_settings
        .filter(|&ms| ms > 0)
        .unwrap_or(orphan_reaper::ORPHAN_GRACE_MS)
        .max(MIN_ORPHAN_GRACE_MS);
    Duration::from_millis(ms)
}

fn spawn_orphan_sweeper(
    state_dir: std::path::PathBuf,
    shutdown_requested: Arc<AtomicBool>,
    env_scans: Arc<crate::process_scan::EnvScanCache>,
) {
    // Both knobs are read once at bringup, like every other daemon runtime
    // setting: a sweep that re-read settings each tick would put a file read on
    // the idle path this sweep is already careful about (#542).
    let interval = orphan_sweep_interval();
    let grace = orphan_sweep_grace();
    let _ = thread::Builder::new()
        .name("clud-orphan-sweep".to_string())
        .spawn(move || {
            // When each still-present orphan candidate was first observed.
            // Thread-local to the sweeper: the grace window (#614) only
            // applies to this periodic path, never to an explicit request.
            let mut first_seen: HashMap<u32, Instant> = HashMap::new();
            loop {
                // Sleep in 1-second slices so shutdown is observed within ~1s.
                let mut remaining = interval;
                while remaining > Duration::ZERO {
                    if shutdown_requested.load(Ordering::SeqCst) {
                        return;
                    }
                    let slice = remaining.min(Duration::from_secs(1));
                    thread::sleep(slice);
                    remaining = remaining.saturating_sub(slice);
                }
                if shutdown_requested.load(Ordering::SeqCst) {
                    return;
                }
                // #548: tolerate a scan up to one sweep interval old. The
                // sampler refreshes on its own tighter cadence when active,
                // so in that state this reuses its pass; on a parked daemon
                // the sampler scans not at all and this is the only pass.
                run_orphan_sweep(
                    &state_dir,
                    "periodic",
                    None,
                    Some((&mut first_seen, grace)),
                    Some((&env_scans, interval)),
                );
            }
        });
}

fn spawn_orphan_reap_once(
    state_dir: std::path::PathBuf,
    trigger: &'static str,
    request_id: Option<u64>,
) {
    // `None` for the scan cache is deliberate (#548): an explicit `clud slay`
    // / `ReapOrphans` request means "reap what is there *now*", so it takes a
    // fresh host pass rather than reusing one the periodic sweep warmed.
    thread::spawn(move || run_orphan_sweep(&state_dir, trigger, request_id, None, None));
}

/// Decide which candidates clear the grace window, and refresh `first_seen`.
///
/// Returns the admission closure's verdict for `pid`. A PID seen for the first
/// time is recorded and spared; on a later tick, once it has been visible for
/// [`ORPHAN_GRACE_MS`], it is admitted. Entries for PIDs that stop appearing
/// are pruned by the caller so the map cannot grow without bound.
fn grace_admits(
    first_seen: &mut HashMap<u32, Instant>,
    pid: u32,
    now: Instant,
    grace: Duration,
) -> bool {
    let first = *first_seen.entry(pid).or_insert(now);
    now.duration_since(first) >= grace
}

/// Single-line record of the last completed orphan sweep, alongside the
/// existing `session-tmp-sweep.last` / `uv-cache-sweep.last` sentinels (#465).
pub(super) const ORPHAN_SWEEP_SENTINEL: &str = "orphan-sweep.last";

pub(super) fn orphan_sweep_sentinel_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join(ORPHAN_SWEEP_SENTINEL)
}

/// Best-effort: a sweep must never fail because its bookkeeping file could not
/// be written.
fn write_orphan_sweep_sentinel(state_dir: &Path, found: usize, reaped: usize) {
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let line = json!({ "ts_ms": ts_ms, "found": found, "reaped": reaped }).to_string();
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(orphan_sweep_sentinel_path(state_dir), line);
}

/// Run one orphan sweep.
///
/// `env_scans` is the shared host-environment cache and the caller's staleness
/// tolerance (#548). `None` means "take a fresh pass" — used by explicit
/// `clud slay` / `ReapOrphans` requests, where the operator asked for an
/// answer *now* and reusing a cached population would be the wrong trade. Only
/// the periodic sweep shares.
fn run_orphan_sweep(
    state_dir: &Path,
    trigger: &'static str,
    request_id: Option<u64>,
    grace: Option<(&mut HashMap<u32, Instant>, Duration)>,
    env_scans: Option<(&Arc<crate::process_scan::EnvScanCache>, Duration)>,
) {
    daemon_events::log_event(
        state_dir,
        "orphan_sweep_started",
        [
            ("trigger", json!(trigger)),
            ("request_id", json!(request_id)),
        ],
    );
    let started = Instant::now();
    let opts = orphan_reaper::ReapOpts {
        keep: false,
        // Quiet: the daemon log shouldn't fill stderr with per-tick
        // scan reports; the JSONL stream is the durable diagnostic surface.
        quiet: true,
        explain: false,
    };
    // `deferred` is only meaningful on the periodic path; explicit requests
    // reap immediately (see `ORPHAN_GRACE_MS`).
    let mut deferred: Vec<u32> = Vec::new();
    let outcome = match grace {
        Some((first_seen, grace_window)) => {
            // #465 handover registry: spare intentionally-detached cohorts
            // (registered under the launching daemon's PID) so a successor
            // daemon doesn't reap a live detached session across a restart.
            // Applied only on the periodic path — an explicit `clud slay` /
            // `ReapOrphans` request means "reap now", registry or not.
            let spared = super::handover_registry::load(state_dir);
            let now = Instant::now();
            let mut observed: HashSet<u32> = HashSet::new();
            let mut admit = |pid: u32| {
                observed.insert(pid);
                let admitted = grace_admits(first_seen, pid, now, grace_window);
                if !admitted {
                    deferred.push(pid);
                }
                admitted
            };
            let (outcome, observed_origins) = match env_scans {
                Some((cache, max_age)) => orphan_reaper::reap_orphans_from_scan(
                    (*cache.get("CLUD", max_age)).clone(),
                    &opts,
                    &spared,
                    &mut admit,
                ),
                None => orphan_reaper::reap_orphans_filtered_sparing(&opts, &spared, &mut admit),
            };
            // Drop bookkeeping for PIDs that are gone, so a long-lived daemon
            // doesn't accumulate an entry per orphan it has ever seen.
            first_seen.retain(|pid, _| observed.contains(pid));
            // Drop handover entries whose originator no longer tags any live
            // process, bounding the registry's growth.
            super::handover_registry::prune(state_dir, &observed_origins);
            outcome
        }
        None => orphan_reaper::reap_orphans(&opts),
    };
    // Candidates that were selected but never handed to `kill_tree` — today
    // that is the `--keep-orphans` path. Previously hardcoded to `[]`, which
    // reported "nothing was skipped" even when something was (issue #612):
    // a kill log that cannot distinguish spared from reaped is not auditable.
    // Note this does NOT include PIDs pruned *inside* `kill_tree_filtered` by
    // the declared-daemon predicate; that walk is intentionally silent, so
    // those remain unobservable here.
    let skipped_pids: Vec<u32> = outcome
        .candidate_pids
        .iter()
        .filter(|pid| !outcome.reaped_pids.contains(pid))
        .copied()
        .collect();
    // #465: also record the result in a dedicated single-line sentinel, the
    // same pattern `session-tmp-sweep.last` / `uv-cache-sweep.last` already use
    // in this directory. `clud daemon orphan-status` reads this rather than
    // scanning the shared event log, which rotates at 1 MB — a burst of
    // unrelated high-volume events can otherwise push every sweep record out of
    // both log files and make a healthy daemon report "no sweep recorded".
    write_orphan_sweep_sentinel(state_dir, outcome.found, outcome.reaped);
    daemon_events::log_event(
        state_dir,
        "orphan_sweep_finished",
        [
            ("trigger", json!(trigger)),
            ("request_id", json!(request_id)),
            ("found", json!(outcome.found)),
            ("reaped", json!(outcome.reaped)),
            ("candidate_pids", json!(outcome.candidate_pids)),
            ("reaped_pids", json!(outcome.reaped_pids)),
            // #1146: a name for every PID above. Added as its own key rather
            // than by widening the three *_pids arrays, because those are a
            // persisted record shape — a reader of an existing log would break
            // on an array whose elements changed type.
            ("pid_labels", json!(outcome.labels)),
            ("skipped_pids", json!(skipped_pids)),
            // Candidates held back by the grace window this pass; they become
            // eligible on a later tick if they are still around.
            ("deferred_pids", json!(deferred)),
            ("reason", json!("dead_originator")),
            ("duration_ms", json!(started.elapsed().as_millis())),
        ],
    );
}

fn spawn_adopt_kill_worker(state_dir: std::path::PathBuf, pids: Vec<u32>, reason: Option<String>) {
    let _ = thread::Builder::new()
        .name("clud-adopt-kill".to_string())
        .spawn(move || {
            daemon_events::log_event(
                &state_dir,
                "adopt_kill_started",
                [("pids", json!(pids)), ("reason", json!(reason))],
            );
            let started = Instant::now();
            let mut killed = Vec::new();
            for pid in &pids {
                let pid = *pid;
                crate::process_tree::kill_tree(pid);
                killed.push(pid);
            }
            daemon_events::log_event(
                &state_dir,
                "adopt_kill_finished",
                [
                    ("pids", json!(pids)),
                    ("killed", json!(killed)),
                    ("reason", json!(reason)),
                    ("duration_ms", json!(started.elapsed().as_millis())),
                ],
            );
        });
}

/// Tear down the processes a session owns: the agent tree first, then the
/// worker that supervises it.
///
/// Every PID here comes off disk, so each is re-checked against the start
/// time recorded next to it (issue #558) before anything is signalled. On a
/// busy Windows host the worker of a long-finished session can easily have
/// had its PID handed to something unrelated, and `clud kill` would otherwise
/// take that process down instead. A live `NativeProcess` handle, when the
/// daemon still owns one, is immune to the problem and is preferred.
fn terminate_session_processes(
    session: &SessionSnapshot,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    session_id: &str,
) {
    if let Some(root) = session.root_identity() {
        signal_process_tree_as(&root, Signal::Term);
        thread::sleep(Duration::from_millis(150));
        signal_process_tree_as(&root, Signal::Kill);
    }

    if let Some(worker) = workers
        .lock()
        .expect("workers mutex poisoned")
        .remove(session_id)
    {
        let _ = worker.kill();
        let _ = worker.wait(Some(Duration::from_secs(2)));
    } else {
        let worker = session.worker_identity();
        if identity_is_alive(&worker) {
            signal_process_tree_as(&worker, Signal::Term);
            thread::sleep(Duration::from_millis(150));
            signal_process_tree_as(&worker, Signal::Kill);
        }
    }
}

fn daemon_terminate_session(
    state_dir: &Path,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    session_id: &str,
) -> io::Result<SessionSnapshot> {
    let path = session_snapshot_path(state_dir, session_id);
    let mut session = read_json_file::<SessionSnapshot>(&path)?;

    terminate_session_processes(&session, workers, session_id);

    session.background = false;
    session.exit_code = Some(130);
    write_json_file(&path, &session)?;
    let _ = fs::remove_file(spec_path(state_dir, session_id));
    Ok(session)
}

fn daemon_interrupt_session(
    state_dir: &Path,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    session_id: &str,
    profile: CtrlCProfile,
) -> io::Result<SessionSnapshot> {
    let path = session_snapshot_path(state_dir, session_id);
    let mut session = read_json_file::<SessionSnapshot>(&path)?;
    merge_ctrl_c_profile_for_daemon(&mut session, profile);
    session.background = false;
    session.exit_code = Some(130);
    write_json_file(&path, &session)?;

    let state_dir = state_dir.to_path_buf();
    let workers = Arc::clone(workers);
    let session_id = session_id.to_string();
    thread::spawn(move || {
        finish_daemon_interrupt_session(&state_dir, &workers, &session_id);
    });

    Ok(session)
}

fn finish_daemon_interrupt_session(
    state_dir: &Path,
    workers: &Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    session_id: &str,
) {
    let path = session_snapshot_path(state_dir, session_id);
    let mut session = match read_json_file::<SessionSnapshot>(&path) {
        Ok(session) => session,
        Err(_) => return,
    };
    let started_at_ms = unix_millis_now();
    {
        let profile = session.ctrl_c.get_or_insert_with(CtrlCProfile::default);
        if profile.daemon_kill_started_at_ms.is_none() {
            profile.daemon_kill_started_at_ms = Some(started_at_ms);
        }
        profile.fast_path = true;
    }
    let _ = write_json_file(&path, &session);

    terminate_session_processes(&session, workers, session_id);

    let finished_at_ms = unix_millis_now();
    if let Ok(mut latest) = read_json_file::<SessionSnapshot>(&path) {
        latest.background = false;
        latest.exit_code = Some(130);
        let profile = latest.ctrl_c.get_or_insert_with(CtrlCProfile::default);
        if profile.daemon_kill_started_at_ms.is_none() {
            profile.daemon_kill_started_at_ms = Some(started_at_ms);
        }
        profile.daemon_kill_finished_at_ms = Some(finished_at_ms);
        profile.daemon_kill_ms = Some(finished_at_ms.saturating_sub(started_at_ms));
        profile.fast_path = true;
        let _ = write_json_file(&path, &latest);
    }
    let _ = fs::remove_file(spec_path(state_dir, session_id));
}

fn merge_ctrl_c_profile_for_daemon(session: &mut SessionSnapshot, mut update: CtrlCProfile) {
    let now = unix_millis_now();
    update.fast_path = true;
    if update.daemon_received_at_ms.is_none() {
        update.daemon_received_at_ms = Some(now);
    }
    let current = session.ctrl_c.get_or_insert_with(CtrlCProfile::default);
    if update.cli_pid.is_some() {
        current.cli_pid = update.cli_pid;
    }
    if update.cli_observed_at_ms.is_some() {
        current.cli_observed_at_ms = update.cli_observed_at_ms;
    }
    if update.cli_handoff_at_ms.is_some() {
        current.cli_handoff_at_ms = update.cli_handoff_at_ms;
    }
    if update.cli_return_ready_at_ms.is_some() {
        current.cli_return_ready_at_ms = update.cli_return_ready_at_ms;
    }
    if update.cli_handoff_ms.is_some() {
        current.cli_handoff_ms = update.cli_handoff_ms;
    }
    if update.daemon_received_at_ms.is_some() {
        current.daemon_received_at_ms = update.daemon_received_at_ms;
    }
    current.fast_path = true;
}

#[cfg(test)]
mod tests {
    use super::super::checkout_claims::{CheckoutKey, CheckoutOccupant, CheckoutReply};
    use super::super::wire_prost::{
        decode_daemon_response_line, encode_daemon_request_line, DaemonWireFormat,
    };
    use super::*;
    use std::io::{BufRead, Write};
    use std::net::TcpStream;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn startup_failure_in_daemon_info_write_records_start_failed_after_phases() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where daemon.json belongs forces the write to fail.
        fs::create_dir_all(daemon_info_path(tmp.path())).unwrap();
        assert_eq!(run_daemon(tmp.path(), &AtomicBool::new(false)), 1);
        let log = fs::read_to_string(daemon_events_path(tmp.path())).unwrap();
        let ops: Vec<(String, String)> = log
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| {
                matches!(
                    e["op"].as_str(),
                    Some("daemon_starting" | "daemon_startup_phase" | "daemon_start_failed")
                )
            })
            .map(|e| {
                (
                    e["op"].as_str().unwrap().to_string(),
                    e["phase"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let phases: Vec<&str> = ops.iter().map(|(_, p)| p.as_str()).collect();
        assert_eq!(
            phases,
            [
                "",
                "stale_state_cleanup",
                "api_session_reconcile",
                "loopback_bind",
                "gc_registry_worker",
                "dashboard_bind",
                "daemon_info_write",
            ]
        );
        assert_eq!(ops.last().unwrap().0, "daemon_start_failed");
    }

    #[test]
    fn checkout_claim_round_trips_through_daemon_tcp_handler() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let claims = CheckoutClaimRegistry::new(Duration::ZERO);
        let observed_claims = claims.clone();
        let server = thread::spawn(move || {
            let mut workers = Vec::new();
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let claims = claims.clone();
                workers.push(thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let request = serde_json::from_str(&line).unwrap();
                    handle_checkout_connection(&mut stream, reader, &claims, request).unwrap();
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        let key = CheckoutKey {
            worktree: "/repo".into(),
            common_git_dir: "/repo/.git".into(),
        };
        let call = |request: CheckoutRequest| {
            let mut stream = TcpStream::connect(address).unwrap();
            serde_json::to_writer(&mut stream, &request).unwrap();
            stream.write_all(b"\n").unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            (
                stream,
                serde_json::from_str::<CheckoutReply>(&line).unwrap(),
            )
        };
        let (presence, reply) = call(CheckoutRequest::Present {
            checkout: key.clone(),
            occupant: CheckoutOccupant {
                session_id: "one".into(),
                pid: 1,
                tool: "grind".into(),
                run: None,
                started_at: 1,
                claimed: false,
            },
        });
        assert!(matches!(reply, CheckoutReply::Present { .. }));
        assert!(matches!(
            call(CheckoutRequest::Claim {
                checkout: key.clone(),
                session_id: "one".into(),
                restoring: false,
            })
            .1,
            CheckoutReply::Claimed
        ));
        let (second_presence, _) = call(CheckoutRequest::Present {
            checkout: key.clone(),
            occupant: CheckoutOccupant {
                session_id: "two".into(),
                pid: 2,
                tool: "do".into(),
                run: None,
                started_at: 2,
                claimed: false,
            },
        });
        assert!(matches!(
            call(CheckoutRequest::Claim {
                checkout: key.clone(),
                session_id: "two".into(),
                restoring: false,
            }).1,
            CheckoutReply::Refused { holder: Some(holder), .. } if holder.session_id == "one"
        ));
        let (_, reply) = call(CheckoutRequest::Who {
            checkout: key.clone(),
            exclude_session: None,
        });
        assert!(
            matches!(reply, CheckoutReply::Who { occupants } if occupants.len() == 2 && occupants.iter().filter(|person| person.claimed).count() == 1)
        );
        drop(presence);
        drop(second_presence);
        server.join().unwrap();
        assert!(observed_claims.who(&key, None).is_empty());
    }

    #[test]
    fn api_kill_dispatch_uses_the_supplied_lifecycle_owner() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = super::super::api_sessions::ApiSessionStore::new(temp.path());
        let record = store
            .create(super::super::api_sessions::CreateApiSession {
                backend: super::super::api_sessions::ApiSessionBackend::Claude,
                cwd: temp.path().to_path_buf(),
                name: None,
                origin: crate::session_origin::SessionOrigin::Test,
                resolved_settings: super::super::api_sessions::ResolvedApiSessionSettings {
                    model: None,
                    safe: false,
                    model_provider: None,
                    harness: None,
                    routing_mode: None,
                },
            })
            .unwrap();
        let lifecycle =
            Arc::new(super::super::api_session_lifecycle::ApiSessionLifecycle::new(store.clone()));
        let workers = Arc::new(Mutex::new(HashMap::new()));
        let reply = dispatch_daemon_request_with_sampler_and_lifecycle(
            temp.path(),
            &workers,
            None,
            None,
            &ClientLeaseRegistry::default(),
            &lifecycle,
            DaemonRequest::ApiSessionKill {
                session_id: record.id.clone(),
            },
        );
        assert!(matches!(reply, DaemonResponse::ApiSessionKilled { .. }));
        assert_eq!(
            store.get(&record.id).unwrap().state,
            super::super::api_sessions::ApiSessionState::Terminated
        );
    }

    #[test]
    fn production_idle_timeout_is_disabled_until_explicitly_configured() {
        assert_eq!(production_idle_timeout_from(None), None);
        assert_eq!(production_idle_timeout_from(Some(0)), None);
        assert_eq!(
            production_idle_timeout_from(Some(7)),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn orphan_sweep_interval_defaults_and_clamps() {
        // Unset / garbage / zero → the 60s default (#465 AC 1).
        assert_eq!(
            orphan_sweep_interval_from(None, None),
            Duration::from_millis(DEFAULT_ORPHAN_SWEEP_INTERVAL_MS)
        );
        assert_eq!(
            orphan_sweep_interval_from(Some("not-a-number"), None),
            Duration::from_millis(DEFAULT_ORPHAN_SWEEP_INTERVAL_MS)
        );
        assert_eq!(
            orphan_sweep_interval_from(Some("0"), None),
            Duration::from_millis(DEFAULT_ORPHAN_SWEEP_INTERVAL_MS)
        );
        // A valid override is honored (with surrounding whitespace trimmed).
        assert_eq!(
            orphan_sweep_interval_from(Some("  120000 "), None),
            Duration::from_millis(120_000)
        );
        // A too-small value is clamped to the floor, never a hot loop.
        assert_eq!(
            orphan_sweep_interval_from(Some("10"), None),
            Duration::from_millis(MIN_ORPHAN_SWEEP_INTERVAL_MS)
        );
    }

    /// #465 AC 1 asks for the interval in `~/.clud/settings.json`, not only in
    /// an env var — a running daemon cannot see an env var change, and an
    /// operator editing config expects it to take effect on restart.
    #[test]
    fn orphan_sweep_interval_reads_settings_when_no_env_override() {
        assert_eq!(
            orphan_sweep_interval_from(None, Some(120_000)),
            Duration::from_millis(120_000)
        );
        // Settings are clamped by the same floor as the env var: a config typo
        // must not be able to spin a full-host env scan into a hot loop.
        assert_eq!(
            orphan_sweep_interval_from(None, Some(10)),
            Duration::from_millis(MIN_ORPHAN_SWEEP_INTERVAL_MS)
        );
        // A zero in settings is garbage, not "disable the sweep".
        assert_eq!(
            orphan_sweep_interval_from(None, Some(0)),
            Duration::from_millis(DEFAULT_ORPHAN_SWEEP_INTERVAL_MS)
        );
    }

    /// Env is the break-glass knob for an incident on a box whose settings file
    /// you may not want to touch, so it outranks the durable configuration.
    #[test]
    fn an_env_override_outranks_the_settings_value() {
        assert_eq!(
            orphan_sweep_interval_from(Some("30000"), Some(120_000)),
            Duration::from_millis(30_000)
        );
        // ...but a *garbage* env value falls through to settings rather than
        // silently discarding the operator's configured value.
        assert_eq!(
            orphan_sweep_interval_from(Some("nonsense"), Some(120_000)),
            Duration::from_millis(120_000)
        );
    }

    /// The grace window is the other knob #465 wanted exposed: it is what an
    /// operator reaches for when the sweep is racing the on-exit `Drop` guard.
    #[test]
    fn orphan_sweep_grace_defaults_and_clamps() {
        assert_eq!(
            orphan_sweep_grace_from(None),
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        );
        assert_eq!(
            orphan_sweep_grace_from(Some(30_000)),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            orphan_sweep_grace_from(Some(0)),
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        );
        // Below the floor the periodic sweep starts racing the `Drop` guard
        // that owns the same cleanup — the exact thing the window prevents.
        assert_eq!(
            orphan_sweep_grace_from(Some(1)),
            Duration::from_millis(MIN_ORPHAN_GRACE_MS)
        );
    }

    /// A configured grace window must actually change admission, not just be
    /// parsed: the value has to reach `grace_admits`.
    #[test]
    fn a_configured_grace_window_changes_admission() {
        let mut first_seen: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();
        let grace = orphan_sweep_grace_from(Some(30_000));
        assert!(!grace_admits(&mut first_seen, 4321, now, grace));
        // Still inside the *configured* window even though the default one has
        // long since elapsed.
        let past_default = now + Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS + 1_000);
        assert!(!grace_admits(&mut first_seen, 4321, past_default, grace));
        assert!(grace_admits(
            &mut first_seen,
            4321,
            now + Duration::from_millis(30_000),
            grace
        ));
    }

    // #380 + #387: even after the N=2->8 / N=9->25 sample bump, 1.2× was
    // still below the median's noise floor on macOS x86 (σ ≈ 40-50ms over
    // a ~50-195ms range → SE of median ~10ms, so the 95% CIs of JSON and
    // prost medians overlapped at the old 1.2× threshold). 1.5× still
    // catches a real >50% prost regression while staying above the noise.
    // Tightening below this would require either >=100 samples or a
    // trimmed-mean statistic — both deferred to a future perf-test
    // redesign.
    const PROST_PERF_BUDGET_NUMERATOR: u128 = 150;
    const PROST_PERF_BUDGET_DENOMINATOR: u128 = 100;
    // #994: an absolute slack added on top of the relative budget. When the
    // JSON median lands in the fast cluster (a few ms) on a loaded macOS
    // runner, one scheduler hiccup in a prost sample blows the *relative*
    // margin even though both protocols are healthy. Real prost regressions
    // are order-of-magnitude, so millisecond-scale slack cannot mask one.
    const PROST_PERF_LOAD_FLOOR: Duration = Duration::from_millis(10);
    // #380: macOS ARM runners exhibit bimodal latency (fast cluster ~50ms,
    // slow cluster ~200ms). At N=9 the JSON and prost medians can land on
    // opposite sides of the cluster gap purely by sample-count luck,
    // flagging a false-positive budget violation. Bump warmup to settle the
    // daemon process JIT / OS scheduler, and bump measured samples so the
    // median's σ/√N variance drops below the 1.5× budget margin.
    const DAEMON_WIRE_PERF_WARMUP_SAMPLES: usize = 8;
    const DAEMON_WIRE_PERF_MEASURED_SAMPLES: usize = 25;

    fn gc_event_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("gc event test lock poisoned")
    }

    fn test_gc_insert(path: &str) -> DaemonRequest {
        DaemonRequest::Gc {
            payload: super::super::types::GcOp::Insert {
                kind: "worktree".to_string(),
                path: path.to_string(),
                repo_root: None,
                branch: None,
                agent_id: None,
                created_unix: Some(100),
            },
        }
    }

    fn test_gc_worker(tmp: &Path) -> mpsc::Sender<RegistryMsg> {
        let registry = crate::gc::Registry::open_at(&tmp.join("gc.redb")).unwrap();
        super::super::gc_service::spawn_registry_worker_with(registry).unwrap()
    }

    #[test]
    fn should_journal_reply_covers_variant_matrix() {
        let rows = Vec::new();
        let cases = [
            (GcReply::ListOk { rows: rows.clone() }, false),
            (
                GcReply::PurgeOk {
                    removed: 0,
                    skipped: 0,
                },
                true,
            ),
            (
                GcReply::PurgeStarted {
                    dispatched: 0,
                    skipped: 0,
                },
                true,
            ),
            (GcReply::ReconcileOk { inserted: 0 }, false),
            (GcReply::ReconcileOk { inserted: 1 }, true),
            (GcReply::InsertOk { inserted: false }, false),
            (GcReply::InsertOk { inserted: true }, true),
            (GcReply::RepoVisitOk, false),
            (GcReply::RepoVisitsOk { rows: Vec::new() }, false),
            (
                GcReply::Error {
                    message: "failed".to_string(),
                },
                true,
            ),
        ];
        for (reply, expected) in cases {
            assert_eq!(should_journal_reply(&reply), expected, "{reply:?}");
        }
    }

    #[test]
    fn noop_insert_dispatch_writes_no_events() {
        let _guard = gc_event_test_lock();
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));
        let tx = test_gc_worker(tmp.path());

        let first = dispatch_daemon_request(
            tmp.path(),
            &workers,
            Some(&tx),
            test_gc_insert("/tmp/journal-noop"),
        );
        assert!(matches!(
            first,
            DaemonResponse::Gc {
                reply: GcReply::InsertOk { inserted: true }
            }
        ));
        let after_first = read_daemon_events(tmp.path());
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0]["op"], "gc_finished");

        let second = dispatch_daemon_request(
            tmp.path(),
            &workers,
            Some(&tx),
            test_gc_insert("/tmp/journal-noop"),
        );
        assert!(matches!(
            second,
            DaemonResponse::Gc {
                reply: GcReply::InsertOk { inserted: false }
            }
        ));
        assert_eq!(read_daemon_events(tmp.path()).len(), after_first.len());
    }

    /// #1666: a creation-ledger row inserted through the daemon's GC op is
    /// answered by a path query, via the registry worker that owns redb.
    #[test]
    fn creation_ledger_round_trips_through_the_daemon() {
        use super::super::types::GcOp;
        let _guard = gc_event_test_lock();
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));
        let tx = test_gc_worker(tmp.path());
        let entry = crate::gc::CreatedEntry {
            session_id: "session-1".into(),
            path: "/ledger/out".into(),
            kind: crate::gc::CreatedKind::Dir,
            role: "agent".into(),
            created_unix: 100,
            dev: Some(1),
            ino: Some(2),
            uid: Some(3),
        };
        // Through the JSON payload encoding the wire carries.
        let wire = |op: GcOp| -> DaemonRequest {
            let json = serde_json::to_string(&op).unwrap();
            DaemonRequest::Gc {
                payload: serde_json::from_str(&json).unwrap(),
            }
        };
        let inserted = dispatch_daemon_request(
            tmp.path(),
            &workers,
            Some(&tx),
            wire(GcOp::InsertCreated {
                entry: entry.clone(),
            }),
        );
        assert!(matches!(
            inserted,
            DaemonResponse::Gc {
                reply: GcReply::CreatedInsertOk { inserted: true }
            }
        ));
        let query = |session: &str, path: &str| -> Vec<crate::gc::CreatedEntry> {
            let request = wire(GcOp::QueryCreated {
                session_id: session.into(),
                path: path.into(),
            });
            let reply = dispatch_daemon_request(tmp.path(), &workers, Some(&tx), request);
            match reply {
                DaemonResponse::Gc {
                    reply: GcReply::CreatedRowsOk { rows },
                } => rows,
                other => panic!("unexpected reply: {other:?}"),
            }
        };
        assert_eq!(query("session-1", "/ledger/out/a/b"), vec![entry.clone()]);
        assert_eq!(query("session-1", "/ledger/out"), vec![entry]);
        assert!(query("session-1", "/ledger/outside").is_empty());
        assert!(query("session-2", "/ledger/out/a").is_empty());
    }

    #[test]
    fn mutating_and_error_ops_still_journal_gc_finished() {
        let _guard = gc_event_test_lock();
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));
        let tx = test_gc_worker(tmp.path());
        let _ = dispatch_daemon_request(
            tmp.path(),
            &workers,
            Some(&tx),
            test_gc_insert("/tmp/journal-mutation"),
        );
        let _ = dispatch_daemon_request(
            tmp.path(),
            &workers,
            None,
            test_gc_insert("/tmp/journal-error"),
        );
        let events = read_daemon_events(tmp.path());
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event["op"] == "gc_finished"));
        assert_eq!(events[0]["response_op"], "insert_ok");
        assert_eq!(events[1]["response_op"], "error");
    }

    #[test]
    fn trace_gate_restores_full_stream() {
        let _guard = gc_event_test_lock();
        let prior = std::env::var_os("CLUD_DAEMON_TRACE_GC");
        std::env::set_var("CLUD_DAEMON_TRACE_GC", "1");
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(value) = self.0.take() {
                    std::env::set_var("CLUD_DAEMON_TRACE_GC", value);
                } else {
                    std::env::remove_var("CLUD_DAEMON_TRACE_GC");
                }
            }
        }
        let _restore = Restore(prior);
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));
        let tx = test_gc_worker(tmp.path());
        let _ = dispatch_daemon_request(
            tmp.path(),
            &workers,
            Some(&tx),
            test_gc_insert("/tmp/journal-trace"),
        );
        let _ = dispatch_daemon_request(
            tmp.path(),
            &workers,
            Some(&tx),
            test_gc_insert("/tmp/journal-trace"),
        );
        let events = read_daemon_events(tmp.path());
        let ops: Vec<&str> = events
            .iter()
            .map(|event| event["op"].as_str().unwrap())
            .collect();
        assert_eq!(
            ops,
            ["gc_started", "gc_finished", "gc_started", "gc_finished"]
        );
    }

    fn read_daemon_events(state_dir: &Path) -> Vec<serde_json::Value> {
        let path = daemon_events_path(state_dir);
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn wait_for_daemon_event_ops(state_dir: &Path, expected: &[&str]) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = read_daemon_events(state_dir);
            let ops: Vec<&str> = events
                .iter()
                .filter_map(|event| event["op"].as_str())
                .collect();
            if expected.iter().all(|expected| ops.contains(expected)) {
                return events;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for daemon events {expected:?}; saw {ops:?}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Starts-and-waits on a live daemon, so callers are `tier1_` tests that
    /// `bash test --host` skips (#2356).
    fn wait_for_daemon_ready(state_dir: &Path) -> DaemonInfo {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(info) = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)) {
                if TcpStream::connect(("127.0.0.1", info.port)).is_ok() {
                    return info;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for daemon startup"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn send_daemon_request_line(
        state_dir: &Path,
        request: &DaemonRequest,
        format: DaemonWireFormat,
    ) -> (DaemonResponse, String) {
        let info = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", info.port)).unwrap();
        let bytes = encode_daemon_request_line(request, format).unwrap();
        stream.write_all(&bytes).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response = decode_daemon_response_line(&line).unwrap();
        (response, line)
    }

    fn timed_daemon_request_line(
        state_dir: &Path,
        request: &DaemonRequest,
        format: DaemonWireFormat,
    ) -> (DaemonResponse, String, Duration) {
        let started = Instant::now();
        let (response, line) = send_daemon_request_line(state_dir, request, format);
        (response, line, started.elapsed())
    }

    fn median_duration(samples: &mut [Duration]) -> Duration {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    fn scaled_duration(sample: Duration, numerator: u128, denominator: u128) -> Duration {
        let nanos = sample.as_nanos().saturating_mul(numerator) / denominator;
        Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
    }

    fn send_raw_daemon_request_line(state_dir: &Path, request_line: &str) -> String {
        let info = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", info.port)).unwrap();
        stream.write_all(request_line.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    }

    /// #2142: SIGTERM/SIGHUP/SIGQUIT only flip `interrupted`; the daemon must
    /// poll it and exit instead of ignoring the signal.
    #[test]
    fn tier1_daemon_exits_when_interrupted_flag_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let flag = Arc::new(AtomicBool::new(false));
        let (dir, thread_flag) = (state_dir.clone(), Arc::clone(&flag));
        let daemon_thread = thread::spawn(move || run_daemon(&dir, &thread_flag));
        wait_for_daemon_ready(&state_dir);

        flag.store(true, Ordering::SeqCst);
        assert_eq!(daemon_thread.join().unwrap(), 0);
        assert!(!daemon_info_path(&state_dir).exists());
    }

    /// #2142: a daemon whose `daemon.json` now names another pid exits, and
    /// leaves the successor's file alone.
    #[test]
    fn tier1_superseded_daemon_exits_and_keeps_successor_info() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let dir = state_dir.clone();
        let daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&dir));
        let mut info = wait_for_daemon_ready(&state_dir);

        info.pid = std::process::id().wrapping_add(1);
        write_json_file(&daemon_info_path(&state_dir), &info).unwrap();
        assert_eq!(daemon_thread.join().unwrap(), 0);
        let after = read_json_file::<DaemonInfo>(&daemon_info_path(&state_dir)).unwrap();
        assert_eq!(after.pid, info.pid, "successor's daemon.json must survive");
    }

    /// #2142: a shutdown request and the interrupted flag both finish well
    /// inside `clud daemon restart`'s 10s wait, even with an idle client
    /// connection held open against the daemon.
    #[test]
    fn tier1_shutdown_and_signal_flag_finish_within_the_restart_timeout() {
        let bound = Duration::from_secs(5);
        for use_flag in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let state_dir = tmp.path().to_path_buf();
            let flag = Arc::new(AtomicBool::new(false));
            let (dir, thread_flag) = (state_dir.clone(), Arc::clone(&flag));
            let daemon_thread = thread::spawn(move || run_daemon(&dir, &thread_flag));
            let info = wait_for_daemon_ready(&state_dir);
            let _idle = TcpStream::connect(("127.0.0.1", info.port)).unwrap();

            let started = Instant::now();
            if use_flag {
                flag.store(true, Ordering::SeqCst);
            } else {
                let (response, _) = send_daemon_request_line(
                    &state_dir,
                    &DaemonRequest::Shutdown {
                        client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                        expected_daemon: Some(info.identity()),
                    },
                    DaemonWireFormat::Json,
                );
                assert!(matches!(response, DaemonResponse::ShutdownAck { .. }));
            }
            assert_eq!(daemon_thread.join().unwrap(), 0);
            assert!(
                started.elapsed() < bound,
                "daemon took {:?} to stop (use_flag={use_flag})",
                started.elapsed()
            );
        }
    }

    #[test]
    fn supersede_check_compares_recorded_pid() {
        assert!(!is_superseded(7, 7));
        assert!(is_superseded(8, 7));
    }

    #[test]
    fn tier1_daemon_accepts_legacy_json_and_prost_clients_in_one_process() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let daemon_state_dir = state_dir.clone();
        let daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&daemon_state_dir));

        let info = wait_for_daemon_ready(&state_dir);
        let (json_response, json_line) = send_daemon_request_line(
            &state_dir,
            &DaemonRequest::ListLiveCwds,
            DaemonWireFormat::Json,
        );
        assert!(matches!(json_response, DaemonResponse::LiveCwds { .. }));
        assert!(
            json_line.starts_with(r#"{"op":"live_cwds""#),
            "default legacy request should receive a JSON daemon response: {json_line:?}"
        );

        let after_json = read_json_file::<DaemonInfo>(&daemon_info_path(&state_dir)).unwrap();
        assert_eq!(after_json.pid, info.pid);
        assert_eq!(after_json.port, info.port);

        let (prost_response, prost_line) = send_daemon_request_line(
            &state_dir,
            &DaemonRequest::Shutdown {
                client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                expected_daemon: Some(info.identity()),
            },
            DaemonWireFormat::Prost,
        );
        assert!(matches!(
            prost_response,
            DaemonResponse::ShutdownAck { pid } if pid == std::process::id()
        ));
        assert!(
            prost_line.starts_with("CLUD-FRAME/1 434c5544 "),
            "prost request should receive a prost daemon response: {prost_line:?}"
        );
        assert_eq!(daemon_thread.join().unwrap(), 0);
        assert!(
            !daemon_info_path(&state_dir).exists(),
            "daemon should remove daemon.json during shutdown"
        );
    }

    /// `spawn_adopt_kill_worker` must return immediately — the whole
    /// point of the AdoptKill IPC is that the CLI's wait time is bounded
    /// by an `mpsc` thread-spawn, not by the eventual `kill_tree` walk.
    /// Hand it a PID that's almost certainly dead so `kill_tree` returns
    /// fast, and pin the spawn-side latency below 100ms even on slow CI.
    #[test]
    fn spawn_adopt_kill_worker_returns_promptly() {
        let tmp = tempfile::tempdir().unwrap();
        let started = Instant::now();
        spawn_adopt_kill_worker(
            tmp.path().to_path_buf(),
            vec![u32::MAX],
            Some("test".to_string()),
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "spawn took too long: {:?}",
            started.elapsed()
        );
    }

    /// Previous-release clients may still use JSON requests, including
    /// unversioned shutdown when the operator explicitly asks for it.
    /// Keep this fixture raw so it cannot accidentally acquire current-client
    /// credentials through a helper.
    #[test]
    fn tier1_daemon_accepts_previous_release_raw_json_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let daemon_state_dir = state_dir.clone();
        let daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&daemon_state_dir));

        let info = wait_for_daemon_ready(&state_dir);
        let list_line = send_raw_daemon_request_line(&state_dir, r#"{"op":"list_live_cwds"}"#);
        assert!(
            !list_line.starts_with("CLUD-FRAME/1 "),
            "raw legacy JSON clients must receive legacy JSON replies: {list_line:?}"
        );
        let list_json: serde_json::Value = serde_json::from_str(&list_line).unwrap();
        assert_eq!(list_json["op"], "live_cwds");
        assert!(
            list_json["paths"].is_array(),
            "live_cwds JSON response should keep the previous-release paths array: {list_line:?}"
        );

        let shutdown_line = send_raw_daemon_request_line(&state_dir, r#"{"op":"shutdown"}"#);
        assert!(
            !shutdown_line.starts_with("CLUD-FRAME/1 "),
            "raw legacy JSON shutdown must receive a legacy JSON reply: {shutdown_line:?}"
        );
        let shutdown_json: serde_json::Value = serde_json::from_str(&shutdown_line).unwrap();
        assert_eq!(shutdown_json["op"], "shutdown_ack");
        assert_eq!(shutdown_json["pid"], info.pid);
        assert_eq!(daemon_thread.join().unwrap(), 0);
        assert!(
            !daemon_info_path(&state_dir).exists(),
            "daemon should remove daemon.json during shutdown"
        );
    }

    #[test]
    fn shutdown_accepts_older_client_but_checks_supplied_generation() {
        let actual = crate::process_identity::ProcessIdentity::new(
            std::process::id(),
            crate::process_identity::self_start_time(),
        );
        assert!(matches!(
            shutdown_response(Some("0.0.1"), Some(actual)),
            DaemonResponse::ShutdownAck { .. }
        ));
        assert!(matches!(
            shutdown_response(None, None),
            DaemonResponse::ShutdownAck { .. }
        ));
        assert!(matches!(
            shutdown_response(
                None,
                Some(crate::process_identity::ProcessIdentity::pid_only(u32::MAX))
            ),
            DaemonResponse::Error { .. }
        ));
    }

    const PROST_PERF_MAX_ATTEMPTS: usize = 3;

    fn timed_list_live_cwds(state_dir: &Path, format: DaemonWireFormat) -> Duration {
        let (response, _, elapsed) =
            timed_daemon_request_line(state_dir, &DaemonRequest::ListLiveCwds, format);
        assert!(matches!(response, DaemonResponse::LiveCwds { .. }));
        elapsed
    }

    /// One warmup + interleaved measurement round. `Err` carries the numbers.
    fn measure_prost_vs_json_once(state_dir: &Path) -> Result<(), String> {
        for _ in 0..DAEMON_WIRE_PERF_WARMUP_SAMPLES {
            timed_list_live_cwds(state_dir, DaemonWireFormat::Json);
            timed_list_live_cwds(state_dir, DaemonWireFormat::Prost);
        }
        let mut json_samples = Vec::with_capacity(DAEMON_WIRE_PERF_MEASURED_SAMPLES);
        let mut prost_samples = Vec::with_capacity(DAEMON_WIRE_PERF_MEASURED_SAMPLES);
        for sample in 0..DAEMON_WIRE_PERF_MEASURED_SAMPLES {
            if sample % 2 == 0 {
                json_samples.push(timed_list_live_cwds(state_dir, DaemonWireFormat::Json));
                prost_samples.push(timed_list_live_cwds(state_dir, DaemonWireFormat::Prost));
            } else {
                prost_samples.push(timed_list_live_cwds(state_dir, DaemonWireFormat::Prost));
                json_samples.push(timed_list_live_cwds(state_dir, DaemonWireFormat::Json));
            }
        }
        let json_median = median_duration(&mut json_samples);
        let prost_median = median_duration(&mut prost_samples);
        let budget = scaled_duration(
            json_median,
            PROST_PERF_BUDGET_NUMERATOR,
            PROST_PERF_BUDGET_DENOMINATOR,
        ) + PROST_PERF_LOAD_FLOOR;
        if prost_median <= budget {
            Ok(())
        } else {
            Err(format!(
                "prost median {prost_median:?} > budget {budget:?}; JSON median {json_median:?}; JSON samples {json_samples:?}; prost samples {prost_samples:?}"
            ))
        }
    }

    #[test]
    fn tier1_prost_daemon_wire_list_rpc_stays_within_json_latency_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let daemon_state_dir = state_dir.clone();
        let daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&daemon_state_dir));

        let info = wait_for_daemon_ready(&state_dir);
        // #2212: a single noisy window on a shared runner can push the prost
        // median over budget. Allow up to PROST_PERF_MAX_ATTEMPTS independent
        // warmup+measure rounds; any one meeting budget passes, so a real
        // regression (which fails every round) is still caught.
        let mut failures = Vec::new();
        let mut passed = false;
        for attempt in 1..=PROST_PERF_MAX_ATTEMPTS {
            match measure_prost_vs_json_once(&state_dir) {
                Ok(()) => {
                    passed = true;
                    break;
                }
                Err(detail) => failures.push(format!("attempt {attempt}: {detail}")),
            }
        }
        assert!(
            passed,
            "prost ListLiveCwds exceeded 50% JSON budget in all {PROST_PERF_MAX_ATTEMPTS} attempts:\n{}",
            failures.join("\n")
        );

        let (shutdown_response, shutdown_line) = send_daemon_request_line(
            &state_dir,
            &DaemonRequest::Shutdown {
                client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                expected_daemon: Some(info.identity()),
            },
            DaemonWireFormat::Prost,
        );
        assert!(matches!(
            shutdown_response,
            DaemonResponse::ShutdownAck { pid } if pid == std::process::id()
        ));
        assert!(
            shutdown_line.starts_with("CLUD-FRAME/1 434c5544 "),
            "prost shutdown should receive a prost daemon response: {shutdown_line:?}"
        );
        assert_eq!(daemon_thread.join().unwrap(), 0);
    }

    #[test]
    fn spawn_adopt_kill_worker_accepts_empty_pids() {
        // Zero-PID payload is valid wire data (the CLI may have lost the
        // root PID); the worker must still spawn without panicking.
        let tmp = tempfile::tempdir().unwrap();
        let started = Instant::now();
        spawn_adopt_kill_worker(tmp.path().to_path_buf(), Vec::new(), None);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn tier1_adopt_kill_request_writes_jsonl_events() {
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));

        let response = dispatch_daemon_request(
            tmp.path(),
            &workers,
            None,
            DaemonRequest::AdoptKill {
                pids: vec![u32::MAX],
                reason: Some("ctrl_c_subprocess".to_string()),
            },
        );
        assert!(matches!(
            response,
            DaemonResponse::AdoptKillAck { accepted: 1 }
        ));

        let events = wait_for_daemon_event_ops(
            tmp.path(),
            &[
                "adopt_kill_accepted",
                "adopt_kill_started",
                "adopt_kill_finished",
            ],
        );
        let accepted = events
            .iter()
            .find(|event| event["op"] == "adopt_kill_accepted")
            .unwrap();
        assert_eq!(accepted["pids"], json!([u32::MAX]));
        assert_eq!(accepted["reason"], "ctrl_c_subprocess");
        let finished = events
            .iter()
            .find(|event| event["op"] == "adopt_kill_finished")
            .unwrap();
        assert_eq!(finished["killed"], json!([u32::MAX]));
        assert!(finished["duration_ms"].is_u64());
    }

    #[test]
    fn tier1_reap_orphans_request_writes_jsonl_events() {
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));

        let response =
            dispatch_daemon_request(tmp.path(), &workers, None, DaemonRequest::ReapOrphans);
        assert!(matches!(
            response,
            DaemonResponse::ReapOrphansAck {
                found: 0,
                reaped: 0
            }
        ));

        let events = wait_for_daemon_event_ops(
            tmp.path(),
            &[
                "reap_orphans_accepted",
                "orphan_sweep_started",
                "orphan_sweep_finished",
            ],
        );
        let finished = events
            .iter()
            .find(|event| event["op"] == "orphan_sweep_finished")
            .unwrap();
        assert_eq!(finished["trigger"], "request");
        assert!(finished["found"].is_u64());
        assert!(finished["reaped"].is_u64());
        assert!(finished["candidate_pids"].is_array());
        assert!(finished["reaped_pids"].is_array());
        assert_eq!(finished["reason"], "dead_originator");
    }

    #[test]
    fn metrics_request_returns_daemon_pid_and_cpu_sample() {
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));

        let response = dispatch_daemon_request(tmp.path(), &workers, None, DaemonRequest::Metrics);
        match response {
            DaemonResponse::Metrics { pid, cpu_pct } => {
                assert_eq!(pid, std::process::id());
                assert!(cpu_pct >= 0.0, "cpu_pct should be non-negative: {cpu_pct}");
            }
            other => panic!("expected Metrics, got {other:?}"),
        }
    }

    #[test]
    fn proc_snapshot_request_returns_cached_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));

        let response = dispatch_daemon_request(
            tmp.path(),
            &workers,
            None,
            DaemonRequest::ProcSnapshot {
                include_dead_since_ms: 0,
            },
        );
        match response {
            DaemonResponse::ProcSnapshot { snapshot } => {
                assert_eq!(snapshot.schema_version, 1);
                assert_eq!(snapshot.sampler_pid, std::process::id());
            }
            other => panic!("expected ProcSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn client_lease_rpc_is_idempotent_for_one_process_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let workers = Arc::new(Mutex::new(HashMap::<String, Arc<NativeProcess>>::new()));
        let leases = ClientLeaseRegistry::default();
        let identity = crate::process_identity::ProcessIdentity::new(
            std::process::id(),
            crate::process_identity::self_start_time(),
        );

        for expected_count in [1, 1] {
            let response = dispatch_daemon_request_with_sampler(
                tmp.path(),
                &workers,
                None,
                None,
                &leases,
                DaemonRequest::AcquireClientLease { identity },
            );
            assert!(matches!(
                response,
                DaemonResponse::ClientLeaseAcquired { lease_count }
                    if lease_count == expected_count
            ));
        }
        for expected_count in [0, 0] {
            let response = dispatch_daemon_request_with_sampler(
                tmp.path(),
                &workers,
                None,
                None,
                &leases,
                DaemonRequest::ReleaseClientLease { identity },
            );
            assert!(matches!(
                response,
                DaemonResponse::ClientLeaseReleased { lease_count }
                    if lease_count == expected_count
            ));
        }
    }

    /// Issue #614: the periodic sweep must not reap a candidate the first
    /// time it sees it. The on-exit `Drop` guard of the clud that just died
    /// may still be working on that subtree.
    #[test]
    fn grace_window_spares_a_newly_observed_orphan() {
        let mut first_seen: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();
        assert!(!grace_admits(
            &mut first_seen,
            4321,
            now,
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        ));
        assert!(first_seen.contains_key(&4321), "first sighting is recorded");
    }

    /// Once the candidate has been visible for the whole window it is
    /// admitted. Uses a synthetic "now" in the future rather than sleeping.
    #[test]
    fn grace_window_admits_after_the_window_elapses() {
        let mut first_seen: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();
        assert!(!grace_admits(
            &mut first_seen,
            4321,
            now,
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        ));
        let later = now + Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS);
        assert!(grace_admits(
            &mut first_seen,
            4321,
            later,
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        ));
    }

    /// A candidate still inside the window on a later tick stays spared --
    /// the clock runs from first sighting, not from the current tick.
    #[test]
    fn grace_window_still_spares_just_inside_the_boundary() {
        let mut first_seen: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();
        assert!(!grace_admits(
            &mut first_seen,
            4321,
            now,
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        ));
        let nearly = now + Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS - 1);
        assert!(!grace_admits(
            &mut first_seen,
            4321,
            nearly,
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        ));
    }

    /// Each PID is timed independently: an old candidate being admitted must
    /// not drag a freshly-seen sibling in with it.
    #[test]
    fn grace_window_times_each_pid_independently() {
        let mut first_seen: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();
        assert!(!grace_admits(
            &mut first_seen,
            100,
            now,
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        ));
        let later = now + Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS);
        assert!(grace_admits(
            &mut first_seen,
            100,
            later,
            Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
        ));
        assert!(
            !grace_admits(
                &mut first_seen,
                200,
                later,
                Duration::from_millis(orphan_reaper::ORPHAN_GRACE_MS)
            ),
            "pid 200 was first seen at `later`, so its own window has not elapsed"
        );
    }

    /// Issue #305: the budget is a knob because 5s was a guess that held on an
    /// idle machine and not on a loaded one.
    #[test]
    fn the_worker_spawn_budget_is_configurable() {
        assert_eq!(
            parse_worker_spawn_budget(Some("45")),
            Duration::from_secs(45)
        );
        assert_eq!(
            parse_worker_spawn_budget(Some("  30  ")),
            Duration::from_secs(30),
            "the sibling env knobs trim, so this one does too"
        );
    }

    /// A bad value must not become a budget that fails instantly.
    ///
    /// Zero is the dangerous one: it parses, so a naive implementation would
    /// accept it and every spawn would time out immediately -- an env typo
    /// presenting as a totally broken daemon.
    #[test]
    fn a_useless_budget_value_falls_back_to_the_default() {
        for raw in [None, Some(""), Some("soon"), Some("-1"), Some("0")] {
            assert_eq!(
                parse_worker_spawn_budget(raw),
                DEFAULT_WORKER_SPAWN_BUDGET,
                "{raw:?} must not shorten the budget"
            );
        }
    }

    /// The default was raised from 5s, which is the value #305 names as too
    /// tight under load. Pinned so a future edit cannot quietly restore it.
    #[test]
    fn the_default_budget_is_longer_than_the_five_seconds_that_was_too_tight() {
        assert!(
            DEFAULT_WORKER_SPAWN_BUDGET > Duration::from_secs(5),
            "the point of #305 phase 3 is that 5s was the problem"
        );
    }
}

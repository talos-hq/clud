use std::fs;
use std::io::{self, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use running_process::pty::NativePtyProcess;
use running_process::{NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode};

use crate::foreground_runtime::ForegroundRuntime;
use crate::graphics::GraphicsConfig;
use crate::launch_log;
use crate::process_identity::{self, ProcessIdentity};
use crate::subprocess;
use crate::terminal_queries::TerminalQueryScanner;
use crate::win_creation_flags::invisible_helper_creationflags;

use super::io_helpers::{child_env_from_login_base, read_json_file};
use super::paths::spec_path;
use super::process_utils::identity_is_alive;
use super::types::{
    CtrlCProfile, SessionKind, SessionRuntime, SessionSnapshot, WorkerClientMessage,
    WorkerLaunchSpec, WorkerServerMessage, DEFAULT_BACKLOG_LIMIT_BYTES,
};
use super::wire_prost::{
    decode_worker_client_line, encode_worker_server_line, DaemonWireFormat, WireError,
};
use super::worker_shared::WorkerShared;

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub(super) fn run_worker(
    state_dir: &Path,
    session_id: &str,
    daemon_pid: u32,
    spec_file: &Path,
) -> i32 {
    // Retag the crash reporter for the worker process so crashes here get
    // written under role="worker" instead of inheriting the foreground tag.
    // Native-crash handling is installed at the same time.
    crate::crash_report::install_native("worker");
    let spec = match read_json_file::<WorkerLaunchSpec>(spec_file) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("[clud] failed to read worker spec: {}", err);
            return 1;
        }
    };
    if spec.repeat_run_command.is_some() {
        return run_repeat_worker(state_dir, session_id, daemon_pid, &spec);
    }
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("[clud] failed to bind worker listener: {}", err);
            return 1;
        }
    };
    let _ = listener.set_nonblocking(true);
    let worker_port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(err) => {
            eprintln!("[clud] failed to read worker listener addr: {}", err);
            return 1;
        }
    };

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let snapshot = SessionSnapshot {
        id: session_id.to_string(),
        kind: spec.kind.clone(),
        backend: Some(spec.plan.backend.executable_name().to_string()),
        launch_mode: Some(spec.plan.launch_mode.as_str().to_string()),
        repo_root: spec
            .plan
            .cwd
            .as_deref()
            .and_then(launch_log::repo_root_for_cwd),
        command: spec.plan.command.clone(),
        cwd: spec.plan.cwd.clone(),
        name: spec.name.clone(),
        created_at: Some(created_at),
        detachable: spec.detachable,
        background: spec.background_on_launch,
        attachable: spec.attachable,
        repeat_interval_secs: spec.repeat_interval_secs,
        repeat_next_run_at: None,
        repeat_running: spec.repeat_interval_secs.is_some(),
        daemon_pid,
        worker_pid: std::process::id(),
        worker_port,
        root_pid: None,
        // Issue #558: pin each PID to the process that holds it now, so a
        // later reader can tell this worker from whatever inherits its
        // number. The daemon is by construction alive at this point, so
        // reading its start time here is accurate.
        daemon_pid_start: process_identity::start_time_of(daemon_pid),
        worker_pid_start: process_identity::self_start_time(),
        root_pid_start: process_identity::UNKNOWN_START_TIME,
        exit_code: None,
        exited_at: None,
        ctrl_c: None,
    };
    let backlog_limit = spec.backlog_bytes.unwrap_or(DEFAULT_BACKLOG_LIMIT_BYTES);
    let shared = Arc::new(WorkerShared::new_with_backlog(
        state_dir.to_path_buf(),
        session_id.to_string(),
        snapshot,
        backlog_limit,
    ));
    shared.init_log_file();
    if let Some(path) = &spec.transcript_path {
        if let Err(err) = shared.init_transcript_file(path) {
            eprintln!(
                "[clud] failed to open transcript {}: {}",
                path.display(),
                err
            );
            return 1;
        }
    }

    // The worker, rather than the daemon client, owns the bridge lifetime.
    // Keep this value in the worker scope until the session exits: its
    // environment is handed only to the harness child and BridgeHandle drops
    // (closing its listener and discarding its bearer) on every exit path.
    let launch_runtime = match start_worker_runtime(&spec.plan, &spec.login_env, &spec.client_env) {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("[clud] failed to start cross-route runtime: {}", err);
            return 1;
        }
    };

    let runtime = match spec.kind {
        SessionKind::Subprocess => {
            match start_subprocess_session(&spec, &shared, launch_runtime.env().to_vec()) {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("[clud] failed to start subprocess session: {}", err);
                    return 1;
                }
            }
        }
        SessionKind::Pty => {
            match start_pty_session(&spec, &shared, launch_runtime.env().to_vec()) {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("[clud] failed to start PTY session: {}", err);
                    return 1;
                }
            }
        }
    };

    shared.set_root_pid(runtime.root_pid());
    if let Err(err) = persist_snapshot(state_dir, session_id, &shared) {
        eprintln!("[clud] failed to write session metadata: {}", err);
        return 1;
    }

    // #1142: set when the daemon is observed gone, and read by the accept loop
    // below. `stop_accepting` cannot carry this: it also means "the child
    // exited, let the client finish reading", and that meaning waits on
    // `!has_client()`. See `should_stop_accepting`.
    let daemon_gone = Arc::new(AtomicBool::new(false));
    {
        let shared = Arc::clone(&shared);
        let runtime = runtime.clone();
        let state_dir = state_dir.to_path_buf();
        let session_id = session_id.to_string();
        let daemon_gone = Arc::clone(&daemon_gone);
        // Issue #558: watch the daemon *process*. Polling a bare PID would
        // keep this worker alive forever if an unrelated process inherited
        // the daemon's number after it died.
        let daemon = shared.snapshot().daemon_identity();
        thread::spawn(move || loop {
            if shared.snapshot().exit_code.is_some() {
                break;
            }
            if !identity_is_alive(&daemon) {
                runtime.cleanup_tree();
                shared.broadcast_exit(137);
                let _ = persist_snapshot(&state_dir, &session_id, &shared);
                let _ = fs::remove_file(spec_path(&state_dir, &session_id));
                // Order matters: `broadcast_exit` above stops the eviction
                // thread, so this is what actually ends the accept loop.
                // Setting it last keeps the snapshot and spec-file cleanup
                // ahead of the exit, as they were.
                daemon_gone.store(true, Ordering::Release);
                break;
            }
            thread::sleep(Duration::from_millis(200));
        });
    }

    let attach_grace = first_attach_grace(spec.background_on_launch);

    // Heartbeat thread: periodically probe the attached client's TCP connection.
    // If the peer has disconnected (e.g. terminal crash, SSH drop), evict the
    // dead client so new attach attempts succeed immediately.
    {
        let shared = Arc::clone(&shared);
        let daemon_gone = Arc::clone(&daemon_gone);
        thread::spawn(move || loop {
            // #1142: the same predicate the accept loop uses, so this keeps
            // evicting for exactly as long as that loop can still be waiting
            // on `has_client()`.
            //
            // This used to break on `stop_accepting` alone, which is set by
            // `broadcast_exit` — so the moment the worker began shutting down,
            // the only thing that could clear a dead client stopped running.
            // A client whose socket had died but had not yet been evicted then
            // held `has_client()` true forever and the accept loop never
            // ended. That is reachable on an ordinary exit, with the daemon
            // still alive, so the `daemon_gone` flag alone does not cover it.
            if should_stop_accepting(
                daemon_gone.load(Ordering::Acquire),
                shared.stop_accepting.load(Ordering::Acquire),
                shared.has_client(),
                shared.awaiting_first_client(attach_grace),
            ) {
                break;
            }
            shared.evict_dead_client();
            thread::sleep(Duration::from_secs(2));
        });
    }

    loop {
        if should_stop_accepting(
            daemon_gone.load(Ordering::Acquire),
            shared.stop_accepting.load(Ordering::Acquire),
            shared.has_client(),
            shared.awaiting_first_client(attach_grace),
        ) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let shared = Arc::clone(&shared);
                let runtime = runtime.clone();
                let graphics = spec.plan.graphics.clone();
                let kind = spec.kind.clone();
                let rows = spec.rows;
                let cols = spec.cols;
                thread::spawn(move || {
                    let _ = handle_worker_client(
                        stream, &shared, &runtime, &graphics, kind, rows, cols,
                    );
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }
    shared.close_transcript();
    let _ = persist_snapshot(state_dir, session_id, &shared);
    let _ = fs::remove_file(spec_path(state_dir, session_id));
    0
}

fn start_worker_runtime(
    plan: &crate::command::LaunchPlan,
    login_env: &[(String, String)],
    client_env: &[(String, String)],
) -> io::Result<ForegroundRuntime> {
    ForegroundRuntime::start(
        plan,
        child_env_from_login_base(login_env.to_vec(), client_env),
    )
    .map_err(|err| io::Error::other(err.to_string()))
}

fn run_repeat_worker(
    state_dir: &Path,
    session_id: &str,
    daemon_pid: u32,
    spec: &WorkerLaunchSpec,
) -> i32 {
    let repeat_interval_secs = spec.repeat_interval_secs.unwrap_or(0);
    let repeat_run_command = spec.repeat_run_command.clone().unwrap_or_default();
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let snapshot = SessionSnapshot {
        id: session_id.to_string(),
        kind: SessionKind::Subprocess,
        backend: Some(spec.plan.backend.executable_name().to_string()),
        launch_mode: Some(spec.plan.launch_mode.as_str().to_string()),
        repo_root: spec
            .plan
            .cwd
            .as_deref()
            .and_then(launch_log::repo_root_for_cwd),
        command: spec.plan.command.clone(),
        cwd: spec.plan.cwd.clone(),
        name: spec.name.clone(),
        created_at: Some(created_at),
        detachable: false,
        background: true,
        attachable: false,
        repeat_interval_secs: Some(repeat_interval_secs),
        repeat_next_run_at: None,
        repeat_running: true,
        daemon_pid,
        worker_pid: std::process::id(),
        worker_port: 0,
        root_pid: None,
        daemon_pid_start: process_identity::start_time_of(daemon_pid),
        worker_pid_start: process_identity::self_start_time(),
        root_pid_start: process_identity::UNKNOWN_START_TIME,
        exit_code: None,
        exited_at: None,
        ctrl_c: None,
    };
    let shared = Arc::new(WorkerShared::new_with_backlog(
        state_dir.to_path_buf(),
        session_id.to_string(),
        snapshot,
        spec.backlog_bytes.unwrap_or(DEFAULT_BACKLOG_LIMIT_BYTES),
    ));
    shared.init_log_file();
    if let Some(path) = &spec.transcript_path {
        if let Err(err) = shared.init_transcript_file(path) {
            eprintln!(
                "[clud] failed to open transcript {}: {}",
                path.display(),
                err
            );
            return 1;
        }
    }
    if let Err(err) = persist_snapshot(state_dir, session_id, &shared) {
        eprintln!("[clud] failed to write repeat session metadata: {}", err);
        return 1;
    }

    // Issue #558: watch the daemon *process*, not its PID. A worker that
    // outlives its daemon must exit; one that keeps running because an
    // unrelated process inherited the daemon's number is an orphan the
    // reapers then have to clean up.
    let daemon = shared.snapshot().daemon_identity();

    loop {
        if !identity_is_alive(&daemon) {
            shared.set_exit_code(137);
            shared.close_transcript();
            let _ = persist_snapshot(state_dir, session_id, &shared);
            let _ = fs::remove_file(spec_path(state_dir, session_id));
            return 0;
        }

        shared.set_repeat_state(true, None);
        if !run_repeat_once(&repeat_run_command, spec, &daemon, &shared) {
            shared.close_transcript();
            let _ = persist_snapshot(state_dir, session_id, &shared);
            let _ = fs::remove_file(spec_path(state_dir, session_id));
            return 0;
        }
        shared.set_root_pid(None);

        let next_run_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
            + repeat_interval_secs.saturating_mul(1000);
        shared.set_repeat_state(false, Some(next_run_at));

        while (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64)
            < next_run_at
        {
            if !identity_is_alive(&daemon) {
                shared.set_exit_code(137);
                shared.close_transcript();
                let _ = persist_snapshot(state_dir, session_id, &shared);
                let _ = fs::remove_file(spec_path(state_dir, session_id));
                return 0;
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

/// Should the worker's accept loop stop?
///
/// Two different reasons, and they must not share a rule (#1142).
///
/// `stop_accepting` alone is a *graceful* end: the child exited and the
/// attached client still has final output to read, so the loop stays up until
/// that client goes away. That drain is deliberate and is why the condition
/// waits on `!has_client()`.
///
/// A dead daemon is not that. It is the end of the only route a client has to
/// this worker, so there is nothing left to drain to and nobody left to drain
/// for -- and waiting for `!has_client()` there is a deadlock rather than a
/// courtesy:
///
/// 1. the daemon watchdog calls `broadcast_exit`, which sets `stop_accepting`;
/// 2. the client-eviction thread's first act is to `break` on `stop_accepting`,
///    so it stops evicting at exactly the moment eviction is needed;
/// 3. a client registered at that instant is therefore never evicted,
///    `has_client()` stays true, and this loop runs forever.
///
/// Observed on a developer machine: two `__worker` processes alive 8 and 15
/// hours after their daemons died, reparented to init, ignoring SIGTERM and
/// needing SIGKILL. The watchdog had run -- the tree was reaped and the exit
/// broadcast -- but the process itself never left this loop.
/// #1582: how long an exited worker keeps its port open for a client that
/// has not attached yet. The launching client learns the port from the
/// daemon and then does pre-attach work before connecting; a child that
/// exits inside that gap must still hand its exit code to that client.
pub(super) const FIRST_ATTACH_GRACE: Duration = Duration::from_secs(30);

/// #1628: the first-attach grace only exists for a launcher that is on its
/// way to attach. A `--detach` (or repeat) launch returns without attaching,
/// so no client is coming: holding the exited worker 30 s there kept it (and
/// its reaping) alive well past the session's end.
fn first_attach_grace(background_on_launch: bool) -> Duration {
    if background_on_launch {
        Duration::ZERO
    } else {
        FIRST_ATTACH_GRACE
    }
}

fn should_stop_accepting(
    daemon_gone: bool,
    stop_accepting: bool,
    has_client: bool,
    awaiting_first_client: bool,
) -> bool {
    daemon_gone || (stop_accepting && !has_client && !awaiting_first_client)
}

fn run_repeat_once(
    command: &[String],
    spec: &WorkerLaunchSpec,
    daemon: &ProcessIdentity,
    shared: &Arc<WorkerShared>,
) -> bool {
    let process = Arc::new(NativeProcess::new(ProcessConfig {
        command: subprocess::command_spec_for_subprocess(command.to_vec()),
        cwd: spec.plan.cwd.as_ref().map(PathBuf::from),
        env: Some(child_env_from_login_base(
            spec.login_env.clone(),
            &spec.client_env,
        )),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        // Issue #55: repeat-job runs are invisible by design — stdio is
        // captured into a TCP-broadcast log, the user never sees the
        // child's console directly. Suppress the conhost window on
        // Windows so each scheduled run doesn't pop a flash. No-op
        // elsewhere.
        creationflags: invisible_helper_creationflags(),
        create_process_group: true,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    }));
    if let Err(err) = process.start() {
        shared
            .push_output(format!("[clud repeat] failed to start child run: {err}\n").into_bytes());
        return true;
    }
    shared.set_root_pid(process.pid());

    loop {
        if !identity_is_alive(daemon) {
            let _ = process.kill();
            let _ = process.wait(Some(Duration::from_secs(2)));
            shared.set_exit_code(137);
            return false;
        }
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => {
                let mut chunk = event.line;
                chunk.push(b'\n');
                shared.push_output(chunk);
            }
            ReadStatus::Timeout => {
                if process.returncode().is_some() {
                    break;
                }
            }
            ReadStatus::Eof => {
                if process.returncode().is_some() {
                    break;
                }
            }
        }
    }
    // Exit is not end of output: the last lines can reach the capture queue
    // after the exit code on a loaded machine. `wait` runs running-process's
    // bounded capture drain, so the queue is complete once it returns (#2159).
    let _ = process.wait(Some(Duration::from_secs(2)));
    for event in process.drain_combined() {
        let mut chunk = event.line;
        chunk.push(b'\n');
        shared.push_output(chunk);
    }
    true
}

fn start_subprocess_session(
    spec: &WorkerLaunchSpec,
    shared: &Arc<WorkerShared>,
    env: Vec<(String, String)>,
) -> io::Result<SessionRuntime> {
    let process = Arc::new(NativeProcess::new(ProcessConfig {
        command: subprocess::command_spec_for_subprocess(spec.plan.command.clone()),
        cwd: spec.plan.cwd.as_ref().map(PathBuf::from),
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        // Issue #55: daemon-managed subprocess session — stdio is fully
        // piped and routed via TCP to attaching clients. The child's
        // console would never be the user's interaction surface, so
        // suppress the conhost window on Windows. No-op elsewhere.
        creationflags: invisible_helper_creationflags(),
        create_process_group: true,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    }));
    process
        .start()
        .map_err(|err| io::Error::other(err.to_string()))?;

    // Drain stdout in a dedicated thread. We must NOT broadcast the
    // backend's exit until this drain has fully completed, otherwise
    // a race lets the wait-thread enqueue `Exited` ahead of an
    // unflushed final `Output` chunk on the worker→client channel —
    // an attaching client then breaks on Exited and silently drops
    // the backend's last line of output. macOS-ARM hit this most
    // often in `test_attach_last` (PR #136); the equivalent flake on
    // other platforms is harder to trigger but the bug is pre-existing.
    let read_handle = {
        let process = Arc::clone(&process);
        let shared = Arc::clone(shared);
        thread::spawn(move || loop {
            match process.read_combined(Some(Duration::from_millis(100))) {
                ReadStatus::Line(event) => {
                    let mut chunk = event.line;
                    chunk.push(b'\n');
                    shared.push_output(chunk);
                }
                ReadStatus::Timeout => {
                    if process.returncode().is_some() {
                        break;
                    }
                }
                ReadStatus::Eof => break,
            }
        })
    };

    {
        let process = Arc::clone(&process);
        let shared = Arc::clone(shared);
        thread::spawn(move || {
            let code = match process.wait(None) {
                Ok(code) => code,
                Err(_) => return,
            };
            // Wait for the stdout drain to finish so every `push_output`
            // call has landed before we enqueue `Exited`. `read_combined`
            // polls with a 100ms timeout and rechecks `returncode()` on
            // each Timeout, so this join terminates within ~100ms.
            let _ = read_handle.join();
            // The reader stops at exit, but lines can reach the queue after
            // the exit code; `wait` ran the bounded capture drain, so they
            // are all queued now (#2159).
            for event in process.drain_combined() {
                let mut chunk = event.line;
                chunk.push(b'\n');
                shared.push_output(chunk);
            }
            shared.broadcast_exit(code);
        });
    }

    Ok(SessionRuntime::Subprocess(process))
}

fn start_pty_session(
    spec: &WorkerLaunchSpec,
    shared: &Arc<WorkerShared>,
    env: Vec<(String, String)>,
) -> io::Result<SessionRuntime> {
    let process = Arc::new(
        NativePtyProcess::new(
            spec.plan.command.clone(),
            spec.plan.cwd.clone(),
            Some(env),
            spec.rows,
            spec.cols,
            None,
        )
        .map_err(|err| io::Error::other(err.to_string()))?,
    );
    process.set_echo(false);
    // Start the terminal emulator at the same dims as the PTY so early output
    // (launch banners, first frame of a TUI) lands in the grid from byte 0.
    // Without this, a client that attaches before any resize happens would
    // see a repaint of an empty 0x0 grid.
    shared.init_capture(spec.rows, spec.cols);
    process
        .start_impl()
        .map_err(|err| io::Error::other(err.to_string()))?;

    // Same Output-vs-Exited race fix as `start_subprocess_session`:
    // join the PTY-read thread before broadcasting exit so the
    // final chunk can never be enqueued after `Exited`.
    let read_handle = {
        let process = Arc::clone(&process);
        let shared = Arc::clone(shared);
        let mut queries = TerminalQueryScanner::default();
        thread::spawn(move || loop {
            match process.read_chunk_impl(Some(0.1)) {
                Ok(Some(chunk)) => {
                    // #1366 / #1702: ConPTY emits ESC[6n at startup and
                    // blocks until it gets a cursor report, and a child TUI
                    // may block on its own capability probes. A `--detach`
                    // session has no client to answer, so the worker answers
                    // while nobody is attached.
                    let replies = detached_query_replies(&mut queries, &chunk, shared.has_client());
                    shared.push_output(chunk);
                    if !replies.is_empty() {
                        let _ = process.write_impl(&replies, false);
                    }
                }
                Ok(None) => {
                    if process.wait_impl(Some(0.0)).is_ok() {
                        break;
                    }
                }
                Err(_) => break,
            }
        })
    };

    {
        let process = Arc::clone(&process);
        let shared = Arc::clone(shared);
        thread::spawn(move || {
            let code = match process.wait_impl(None) {
                Ok(code) => code,
                Err(_) => return,
            };
            // #1354: release the ConPTY after the reader drains, or conhost leaks.
            let closer = Arc::clone(&process);
            finish_pty_exit(
                code,
                || {
                    let _ = read_handle.join();
                },
                || {
                    let _ = closer.close_impl();
                },
                |code| shared.broadcast_exit(code),
            );
        });
    }

    Ok(SessionRuntime::Pty(process))
}

/// The stub replies a detached worker owes its child for `chunk` (#1366,
/// #1702). The scanner sees every chunk, so a query split across reads stays
/// tracked while a client is attached; only an unattended worker answers,
/// since an attached client's terminal (or its pump) owns the reply.
fn detached_query_replies(
    scanner: &mut TerminalQueryScanner,
    chunk: &[u8],
    has_client: bool,
) -> Vec<u8> {
    let replies = scanner.replies(chunk);
    if has_client {
        Vec::new()
    } else {
        replies
    }
}

/// Post-exit sequence for a PTY session: drain the reader, release the
/// pseudo-console, then broadcast `Exited`. Close must follow the reader
/// join so the final chunk is never lost (#1354).
fn finish_pty_exit<C>(
    code: C,
    join_reader: impl FnOnce(),
    close_pty: impl FnOnce(),
    broadcast: impl FnOnce(C),
) {
    join_reader();
    close_pty();
    broadcast(code);
}

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn handle_worker_client(
    mut stream: TcpStream,
    shared: &Arc<WorkerShared>,
    runtime: &SessionRuntime,
    graphics: &GraphicsConfig,
    kind: SessionKind,
    default_rows: u16,
    default_cols: u16,
) -> io::Result<()> {
    let reader_stream = stream.try_clone()?;
    reader_stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    if read_worker_line(&mut reader, &mut line, None)? == 0 {
        return Ok(());
    }
    let (message, format) = decode_worker_client_line(&line).map_err(worker_wire_error_to_io)?;
    let (terminal, attach_rows, attach_cols) = match message {
        WorkerClientMessage::Attach {
            terminal,
            rows,
            cols,
        } => (
            terminal,
            rows.unwrap_or(default_rows),
            cols.unwrap_or(default_cols),
        ),
        WorkerClientMessage::Input { .. }
        | WorkerClientMessage::Resize { .. }
        | WorkerClientMessage::Interrupt { .. } => {
            return write_worker_server_line(
                &mut stream,
                &WorkerServerMessage::Error {
                    message: "expected attach handshake".to_string(),
                },
                format,
            );
        }
    };
    let is_pty = matches!(kind, SessionKind::Pty);

    let shutdown_handle = stream.try_clone()?;
    let (client_id, rx, snapshot, backlog) = match shared.attach_client(shutdown_handle) {
        Ok(values) => values,
        Err(message) => {
            return write_worker_server_line(
                &mut stream,
                &WorkerServerMessage::Error { message },
                format,
            );
        }
    };
    let mut writer = stream.try_clone()?;
    write_worker_server_line(
        &mut writer,
        &WorkerServerMessage::Attached {
            session: Box::new(snapshot.clone()),
        },
        format,
    )?;
    let mut header_restore = None;
    if is_pty {
        let decision = crate::graphics::decide_sixel(graphics, terminal.as_ref());
        if decision.enabled {
            match crate::graphics::render_header(graphics, attach_rows, attach_cols) {
                Ok(Some(header)) => {
                    runtime.resize(header.text_rows, attach_cols);
                    shared.resize_capture(header.text_rows, attach_cols);
                    write_worker_server_line(
                        &mut writer,
                        &WorkerServerMessage::Output {
                            data_b64: base64::engine::general_purpose::STANDARD
                                .encode(&header.bytes),
                        },
                        format,
                    )?;
                    header_restore = Some(header.restore_bytes);
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("[clud] warning: failed to render daemon graphics header: {err}");
                }
            }
        }
    }
    for chunk in backlog {
        write_worker_server_line(
            &mut writer,
            &WorkerServerMessage::Output {
                data_b64: base64::engine::general_purpose::STANDARD.encode(chunk),
            },
            format,
        )?;
    }
    if let Some(exit_code) = snapshot.exit_code {
        if let Some(bytes) = header_restore.as_deref() {
            write_worker_server_line(
                &mut writer,
                &WorkerServerMessage::Output {
                    data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
                },
                format,
            )?;
        }
        write_worker_server_line(
            &mut writer,
            &WorkerServerMessage::Exited { exit_code },
            format,
        )?;
        shared.detach_client(client_id);
        return Ok(());
    }

    let shared_for_writer = Arc::clone(shared);
    let writer_thread = thread::spawn(move || {
        let mut header_restore = header_restore;
        while let Ok(message) = rx.recv() {
            if let WorkerServerMessage::Exited { .. } = &message {
                if let Some(bytes) = header_restore.take() {
                    if write_worker_server_line(
                        &mut writer,
                        &WorkerServerMessage::Output {
                            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
                        },
                        format,
                    )
                    .is_err()
                    {
                        break;
                    }
                }
            }
            if write_worker_server_line(&mut writer, &message, format).is_err() {
                break;
            }
        }
        shared_for_writer.detach_client(client_id);
    });

    loop {
        let mut line = String::new();
        match read_worker_line(&mut reader, &mut line, Some((shared, client_id)))? {
            0 => break,
            _ => {
                if !shared.owns_client(client_id) {
                    break;
                }
                let Ok((message, _message_format)) = decode_worker_client_line(&line) else {
                    continue;
                };
                match message {
                    WorkerClientMessage::Attach { .. } => break,
                    WorkerClientMessage::Input { data_b64, submit } => {
                        if let Ok(data) =
                            base64::engine::general_purpose::STANDARD.decode(data_b64.as_bytes())
                        {
                            runtime.write(&data, submit);
                        }
                    }
                    WorkerClientMessage::Resize { rows, cols } => {
                        let effective_rows = if is_pty
                            && crate::graphics::decide_sixel(graphics, terminal.as_ref()).enabled
                        {
                            match crate::graphics::render_header(graphics, rows, cols) {
                                Ok(Some(header)) => {
                                    shared.send_to_client(WorkerServerMessage::Output {
                                        data_b64: base64::engine::general_purpose::STANDARD
                                            .encode(&header.bytes),
                                    });
                                    header.text_rows
                                }
                                Ok(None) => {
                                    shared.send_to_client(WorkerServerMessage::Output {
                                        data_b64: base64::engine::general_purpose::STANDARD.encode(
                                            crate::graphics::reset_layout_bytes(rows, true),
                                        ),
                                    });
                                    rows
                                }
                                Err(err) => {
                                    eprintln!(
                                        "[clud] warning: failed to redraw daemon graphics header: {err}"
                                    );
                                    shared.send_to_client(WorkerServerMessage::Output {
                                        data_b64: base64::engine::general_purpose::STANDARD.encode(
                                            crate::graphics::reset_layout_bytes(rows, true),
                                        ),
                                    });
                                    rows
                                }
                            }
                        } else {
                            rows
                        };
                        runtime.resize(effective_rows, cols);
                        shared.resize_capture(effective_rows, cols);
                    }
                    WorkerClientMessage::Interrupt { profile } => {
                        start_interrupt_fast_path(shared, runtime, profile);
                        break;
                    }
                }
            }
        }
    }

    shared.detach_client(client_id);
    let _ = writer_thread.join();
    Ok(())
}

fn write_worker_server_line(
    writer: &mut TcpStream,
    message: &WorkerServerMessage,
    format: DaemonWireFormat,
) -> io::Result<()> {
    let bytes = encode_worker_server_line(message, format).map_err(worker_wire_error_to_io)?;
    writer.write_all(&bytes)?;
    writer.flush()
}

fn worker_wire_error_to_io(err: WireError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

fn start_interrupt_fast_path(
    shared: &Arc<WorkerShared>,
    runtime: &SessionRuntime,
    profile: Option<CtrlCProfile>,
) {
    shared.record_ctrl_c_handoff(profile.unwrap_or_default());
    let shared = Arc::clone(shared);
    let runtime = runtime.clone();
    thread::spawn(move || {
        let started_at_ms = shared.record_ctrl_c_kill_started();
        runtime.cleanup_tree();
        shared.record_ctrl_c_kill_finished(started_at_ms);
        shared.broadcast_exit(130);
    });
}

fn read_worker_line(
    reader: &mut BufReader<TcpStream>,
    line: &mut String,
    active_client: Option<(&Arc<WorkerShared>, u64)>,
) -> io::Result<usize> {
    use std::io::BufRead;
    loop {
        line.clear();
        match reader.read_line(line) {
            Ok(read) => return Ok(read),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if active_client.is_some_and(|(shared, client_id)| !shared.owns_client(client_id)) {
                    return Ok(0);
                }
            }
            Err(err) => return Err(err),
        }
    }
}

fn persist_snapshot(
    _state_dir: &Path,
    _session_id: &str,
    shared: &Arc<WorkerShared>,
) -> io::Result<()> {
    shared.persist_current_snapshot()
}

// Silence import warnings for items consumed only by the `Write` trait or
// other macros above (none here currently).
#[allow(unused_imports)]
use Write as _;

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;

#[cfg(test)]
mod query_reply_tests {
    use super::detached_query_replies;
    use crate::terminal_queries::TerminalQueryScanner;

    /// #1702: an unattended worker answers the same probes the local pump
    /// does, not only `ESC[6n`, even when a query is split across reads.
    #[test]
    fn a_detached_worker_answers_da1_and_cursor_queries_split_across_reads() {
        let mut scanner = TerminalQueryScanner::default();
        assert!(detached_query_replies(&mut scanner, b"boot\x1b[", false).is_empty());
        assert_eq!(
            detached_query_replies(&mut scanner, b"c\x1b[6n\x1b[6n", false),
            b"\x1b[?1;2c\x1b[1;1R\x1b[1;1R"
        );
    }

    #[test]
    fn an_attached_client_owns_the_reply_but_the_split_stays_tracked() {
        let mut scanner = TerminalQueryScanner::default();
        assert!(detached_query_replies(&mut scanner, b"\x1b[", true).is_empty());
        assert!(detached_query_replies(&mut scanner, b"c", true).is_empty());
        // Already answered by the client: no late reply once it detaches.
        assert!(detached_query_replies(&mut scanner, b"x", false).is_empty());
    }
}

#[cfg(test)]
mod pty_exit_tests {
    use super::finish_pty_exit;
    use std::cell::RefCell;

    #[test]
    fn pty_exit_closes_pseudo_console_after_reader_join_before_broadcast() {
        let order: RefCell<Vec<&str>> = RefCell::new(Vec::new());
        let seen_code = RefCell::new(None);
        finish_pty_exit(
            7,
            || order.borrow_mut().push("join"),
            || order.borrow_mut().push("close"),
            |code| {
                order.borrow_mut().push("broadcast");
                *seen_code.borrow_mut() = Some(code);
            },
        );
        assert_eq!(*order.borrow(), vec!["join", "close", "broadcast"]);
        assert_eq!(*seen_code.borrow(), Some(7));
    }
}

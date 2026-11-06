use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command; // running-process: command-builder
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::{cmp::Ordering, fmt};

use fs4::fs_std::FileExt;
use sysinfo::Signal;

use crate::gc::InsertInput;

use super::io_helpers::read_json_file;
use super::paths::{
    daemon_info_path, daemon_lock_path, session_snapshot_path, sessions_dir, spec_path, specs_dir,
};
use super::process_utils::{identity_is_alive, signal_process_tree_as};
use super::types::{
    CtrlCProfile, DaemonInfo, DaemonRequest, DaemonResponse, GcOp, GcReply, GcWatchRoot, ListRow,
    ProcTreeSnapshot, RepoVisit, SessionSnapshot, WorkerClientMessage, ENV_ALLOW_DAEMON_SPAWN,
};
use super::wire_prost::{
    daemon_wire_format_from_env, decode_daemon_response_line, encode_daemon_request_line,
    encode_worker_client_line, DaemonWireFormat, WireError,
};
use crate::process_identity::ProcessIdentity;

use super::client_compat::{compare_versions, is_old_daemon_signature};

#[derive(Debug)]
struct IncompatibleDaemonVersion {
    running: String,
    client: &'static str,
    pid: u32,
}

impl fmt::Display for IncompatibleDaemonVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // #2163: restarting the daemon from this older clud would downgrade
        // it under every session already on the newer one.
        if compare_versions(&self.running, self.client) == Some(Ordering::Greater) {
            return write!(
                f,
                "daemon pid {} runs version {}, newer than this clud {}; start a new session with the newer clud (do not restart the daemon from this one, which would downgrade it)",
                self.pid, self.running, self.client
            );
        }
        write!(
            f,
            "daemon pid {} runs version {}, which clud {} cannot compare; run `clud daemon stop` or `clud daemon restart` to replace it",
            self.pid, self.running, self.client
        )
    }
}

impl std::error::Error for IncompatibleDaemonVersion {}

pub(super) fn incompatible_daemon_error(info: &DaemonInfo) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        IncompatibleDaemonVersion {
            running: info.version.as_deref().unwrap_or("<unknown>").to_string(),
            client: env!("CARGO_PKG_VERSION"),
            pid: info.pid,
        },
    )
}

pub fn is_incompatible_daemon_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.downcast_ref::<IncompatibleDaemonVersion>().is_some())
}

pub fn print_incompatible_daemon_error(error: &io::Error) {
    eprintln!("{}", incompatible_daemon_error_line(error));
}

fn incompatible_daemon_error_line(error: &io::Error) -> String {
    format!("\x1b[33m[clud] error: {error}\x1b[0m")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonVersionDisposition {
    Match,
    ReplaceOlder,
    RefuseNewerOrUnknown,
}

fn daemon_spawn_allowed() -> bool {
    std::env::var_os(ENV_ALLOW_DAEMON_SPAWN).is_some_and(|value| value == "1")
}

fn require_daemon_spawn_permission(action: &str) -> io::Result<()> {
    if daemon_spawn_allowed() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "daemon {action} is not allowed from this clud mode; only a normal clud launch may set {ENV_ALLOW_DAEMON_SPAWN}=1"
            ),
        ))
    }
}

fn daemon_version_disposition(info: &DaemonInfo) -> DaemonVersionDisposition {
    let Some(running) = info.version.as_deref() else {
        return DaemonVersionDisposition::ReplaceOlder;
    };
    let by_version = match compare_versions(running, env!("CARGO_PKG_VERSION")) {
        Some(Ordering::Equal) => DaemonVersionDisposition::Match,
        Some(Ordering::Less) => DaemonVersionDisposition::ReplaceOlder,
        Some(Ordering::Greater) | None => DaemonVersionDisposition::RefuseNewerOrUnknown,
    };
    with_deleted_executable(by_version, daemon_executable_deleted(info.pid))
}

/// #1841: every build between releases reports the same version, so a
/// reinstall can leave the old daemon running a binary that no longer exists
/// on disk while it still compares equal. It then misreads newer requests
/// (checkout claims got an empty reply). A deleted executable can never be
/// the build the current client runs, so it is stale with certainty, and
/// unlike a build-id comparison this cannot make two live builds keep
/// replacing each other's daemon.
fn with_deleted_executable(
    by_version: DaemonVersionDisposition,
    executable_deleted: bool,
) -> DaemonVersionDisposition {
    if by_version == DaemonVersionDisposition::Match && executable_deleted {
        DaemonVersionDisposition::ReplaceOlder
    } else {
        by_version
    }
}

/// Whether the daemon's executable has been deleted or replaced on disk.
/// Linux only: `/proc/<pid>/exe` names a removed file with a ` (deleted)`
/// suffix. macOS and Windows have no cheap equivalent and report `false`.
fn daemon_executable_deleted(pid: u32) -> bool {
    if crate::platform::host::LINUX {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|target| exe_link_is_deleted(&target.to_string_lossy()))
            .unwrap_or(false)
    } else {
        false
    }
}

// Read only by the Linux `/proc/<pid>/exe` probe above (and its tests).
fn exe_link_is_deleted(link_target: &str) -> bool {
    link_target.ends_with(" (deleted)")
}

pub struct ForegroundClientLease {
    state_dir: PathBuf,
    identity: ProcessIdentity,
    active: bool,
}

impl ForegroundClientLease {
    pub fn release(mut self) {
        self.release_best_effort();
    }

    fn release_best_effort(&mut self) {
        if !self.active {
            return;
        }
        let _ = send_daemon_request(
            &self.state_dir,
            &DaemonRequest::ReleaseClientLease {
                identity: self.identity,
            },
        );
        self.active = false;
    }
}

impl Drop for ForegroundClientLease {
    fn drop(&mut self) {
        self.release_best_effort();
    }
}

pub fn acquire_foreground_client_lease(state_dir: &Path) -> io::Result<ForegroundClientLease> {
    let identity = ProcessIdentity::new(
        std::process::id(),
        crate::process_identity::self_start_time(),
    );
    if !identity.has_start_time() {
        return Err(io::Error::other(
            "cannot acquire client lease without a PID start time",
        ));
    }
    match send_daemon_request(state_dir, &DaemonRequest::AcquireClientLease { identity })? {
        DaemonResponse::ClientLeaseAcquired { .. } => Ok(ForegroundClientLease {
            state_dir: state_dir.to_path_buf(),
            identity,
            active: true,
        }),
        DaemonResponse::Error { message } => Err(io::Error::other(message)),
        response => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected daemon response: {response:?}"),
        )),
    }
}

/// Idempotent best-effort daemon spawn (issue #135). Always called via
/// `main.rs`; the session daemon is now an always-on background service.
///
/// 1. Fast path: read the info file, probe its PID + port; return if up.
/// 2. Slow path: acquire `<state_dir>/daemon.lock` (issue #138 bringup
///    serialization), clean stale state, re-probe under the lock, and
///    only spawn `clud __daemon --state-dir <state_dir>` detached if a
///    sibling didn't bring the daemon up while we waited. Lock releases
///    when this function returns.
///
/// Visible to `main.rs` (the `clud` binary) so it can call this during
/// early startup. `pub` rather than `pub(crate)` because the binary is
/// a separate crate within the package.
pub fn ensure_daemon(state_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(state_dir)?;
    if let Some(info) = probe_existing(state_dir) {
        match daemon_version_disposition(&info) {
            DaemonVersionDisposition::Match => return Ok(()),
            DaemonVersionDisposition::RefuseNewerOrUnknown => {
                return Err(incompatible_daemon_error(&info));
            }
            DaemonVersionDisposition::ReplaceOlder => {}
        }
        require_daemon_spawn_permission("replacement")?;
        // Issue #192: stale daemon from a prior clud version. Kill it
        // under the bringup lock so a fresh `__daemon` (with the current
        // binary's dashboard + registry-merge code) takes over.
        let _bringup_lock = acquire_bringup_lock(state_dir)?;
        if let Some(info) = probe_existing(state_dir) {
            match daemon_version_disposition(&info) {
                DaemonVersionDisposition::Match => return Ok(()),
                DaemonVersionDisposition::ReplaceOlder => {
                    replace_stale_daemon(state_dir, &info)?;
                }
                DaemonVersionDisposition::RefuseNewerOrUnknown => {
                    return Err(incompatible_daemon_error(&info));
                }
            }
        }
        return spawn_and_await_daemon(state_dir);
    }

    let _bringup_lock = acquire_bringup_lock(state_dir)?;
    cleanup_stale_state(state_dir);
    // Re-probe under the lock: a sibling may have spawned while we waited.
    if let Some(info) = probe_existing(state_dir) {
        match daemon_version_disposition(&info) {
            DaemonVersionDisposition::Match => return Ok(()),
            DaemonVersionDisposition::ReplaceOlder => replace_stale_daemon(state_dir, &info)?,
            DaemonVersionDisposition::RefuseNewerOrUnknown => {
                return Err(incompatible_daemon_error(&info));
            }
        }
    }
    require_daemon_spawn_permission("spawn")?;
    spawn_and_await_daemon(state_dir)
}

/// How long a launch waits for a freshly spawned daemon to publish itself.
/// A healthy daemon answers in milliseconds; this bound only matters when the
/// spawn is slow. The first exec of a new binary on macOS goes through
/// Gatekeeper/XProtect assessment, which made 5 s too short on a slow hosted
/// runner and would fail a slow machine's first launch after an upgrade (#1730).
const DAEMON_STARTUP_WAIT: Duration = Duration::from_secs(20);

fn spawn_and_await_daemon(state_dir: &Path) -> io::Result<()> {
    spawn_detached_daemon(state_dir)?;

    let started = Instant::now();
    let our_pid = std::process::id();
    await_daemon_startup(
        || {
            // Make sure we didn't read a stale info file from before the spawn.
            probe_existing(state_dir)
                .is_some_and(|info| info.pid != our_pid && daemon_version_matches(&info))
        },
        || started.elapsed(),
        thread::sleep,
    )
}

/// Poll `ready` every 25 ms until it holds or [`DAEMON_STARTUP_WAIT`] passes.
/// The clock and the sleep are parameters so the bound is testable.
fn await_daemon_startup(
    mut ready: impl FnMut() -> bool,
    elapsed: impl Fn() -> Duration,
    mut sleep: impl FnMut(Duration),
) -> io::Result<()> {
    loop {
        if ready() {
            return Ok(());
        }
        if elapsed() > DAEMON_STARTUP_WAIT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for daemon startup",
            ));
        }
        sleep(Duration::from_millis(25));
    }
}

/// Start `clud __daemon --state-dir <state_dir>` as a detached daemon (#1186).
///
/// Goes through running-process's daemon spawn rather than a hand-rolled
/// detach, which buys three things the old trampoline lacked:
///
/// - **No inherited descriptors.** Unix sweeps every fd above 2; Windows
///   whitelists exactly the three NUL stdio handles. A launcher's leaked pipe
///   can no longer be pinned open for the daemon's whole lifetime.
/// - **`RUNNING_PROCESS_IS_DAEMON`.** The cooperative spare signal the reapers
///   read (DD-021, DD-023), which still protects the daemon when a supervisor
///   refused job breakaway and left it inside a Job Object.
/// - **Breakaway with the same refusal retry**: `CREATE_BREAKAWAY_FROM_JOB`,
///   retried without it on `ERROR_ACCESS_DENIED`; `setsid` on Unix.
///
/// `EnvironmentPolicy::Inherit` keeps today's environment exactly: the daemon
/// uses its own environment as the base for every worker (see
/// `io_helpers::child_env_from`), and changing that model is #933's to decide.
/// The originator tag is removed because the free spawn functions do not
/// strip it — only `ContainedProcessGroup::spawn_daemon` does (#683) — and a
/// daemon started lazily from inside an agent session would otherwise carry
/// that session's tag for as long as it lives.
fn spawn_detached_daemon(state_dir: &Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    // Best effort: without a home or a writable state dir, run in place as before.
    let exe = crate::home::user_home()
        .and_then(|home| crate::version_dir::pinned_executable(&home, &exe).ok())
        .unwrap_or(exe);
    let mut command = Command::new(exe); // running-process: command-builder
    command.arg("__daemon").arg("--state-dir").arg(state_dir);
    command.env_remove(running_process::ORIGINATOR_ENV_VAR);
    // Dropping a `DaemonChild` does not terminate the process.
    let _daemon = running_process::spawn_daemon_breaking_away_with_env_policy(
        &mut command,
        running_process::EnvironmentPolicy::Inherit,
    )?;
    Ok(())
}

pub(super) fn probe_existing(state_dir: &Path) -> Option<DaemonInfo> {
    let info = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)).ok()?;
    if !identity_is_alive(&info.identity()) {
        return None;
    }
    if TcpStream::connect(("127.0.0.1", info.port)).is_ok() {
        Some(info)
    } else {
        None
    }
}

/// Issue #192: returns true when the running daemon was built from the
/// same `CARGO_PKG_VERSION` as this binary. `None` here means the daemon
/// was started by clud <= 2.0.14 (pre-fix daemons never wrote a `version`
/// field), so treat as a mismatch — they predate the registry-merge
/// dashboard fix and should be replaced.
fn daemon_version_matches(info: &DaemonInfo) -> bool {
    info.version.as_deref() == Some(env!("CARGO_PKG_VERSION"))
}

/// Issue #192: terminate a stale daemon (and its worker tree) and delete
/// its `daemon.json` so a fresh daemon can take over. Best-effort — if
/// the kill races with the daemon's own exit, the file may already be
/// gone. Held by the caller under `acquire_bringup_lock` so only one
/// upgrade attempt runs at a time.
fn replace_stale_daemon(state_dir: &Path, info: &DaemonInfo) -> io::Result<()> {
    require_daemon_spawn_permission("replacement")?;
    if daemon_version_disposition(info) != DaemonVersionDisposition::ReplaceOlder {
        return Err(incompatible_daemon_error(info));
    }
    eprintln!(
        "[clud] restarting daemon: running {} != binary {}",
        info.version.as_deref().unwrap_or("<pre-2.0.15>"),
        env!("CARGO_PKG_VERSION"),
    );
    // Identity-guarded throughout (issue #558): between writing daemon.json
    // and this upgrade attempt the old daemon may have exited and had its PID
    // reissued, and killing whatever now holds it would be a plain bug.
    let daemon = info.identity();
    signal_process_tree_as(&daemon, Signal::Term);
    let deadline = Instant::now() + Duration::from_secs(2);
    while identity_is_alive(&daemon) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if identity_is_alive(&daemon) {
        signal_process_tree_as(&daemon, Signal::Kill);
        let deadline = Instant::now() + Duration::from_secs(2);
        while identity_is_alive(&daemon) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
    }
    // Remove the stale info file so `probe_existing` doesn't return it
    // again during the spawn-await loop.
    let _ = fs::remove_file(daemon_info_path(state_dir));
    Ok(())
}

fn acquire_bringup_lock(state_dir: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(state_dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(daemon_lock_path(state_dir))?;
    FileExt::lock_exclusive(&file)?;
    Ok(file)
}

pub(super) fn send_daemon_request(
    state_dir: &Path,
    request: &DaemonRequest,
) -> io::Result<DaemonResponse> {
    // #2000: `Create` is not idempotent and its admission can take tens of
    // seconds, so it never takes the frame lane. That lane has no deadline,
    // and a miss after the frame was sent would re-send `Create` over TCP and
    // could spawn a second worker. Over TCP it is bounded by the daemon's own
    // admission budget, so a stall surfaces as an error the launcher reports.
    if matches!(request, DaemonRequest::Create { .. }) {
        return send_admission_over_tcp(state_dir, request, super::server::create_session_budget());
    }
    // Fast path: running-process broker v1 frame lane (Hello-skip via
    // the daemon identity sidecar). Any miss — `RUNNING_PROCESS_DISABLE=1`,
    // no sidecar, connect/wire failure — falls through to the legacy TCP
    // wire below, which remains the authoritative path.
    if let Some(response) = super::rp_broker::try_send_via_frame_lane(state_dir, request) {
        return Ok(response);
    }
    send_daemon_request_over_tcp(state_dir, request, None)
}

/// One TCP round trip bounded by `budget`, with a timeout reported as the
/// admission stall it is rather than as a bare socket error.
fn send_admission_over_tcp(
    state_dir: &Path,
    request: &DaemonRequest,
    budget: Duration,
) -> io::Result<DaemonResponse> {
    send_daemon_request_over_tcp(state_dir, request, Some(budget)).map_err(|err| {
        if matches!(
            err.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "daemon did not admit the session within {}s ({err}); its admission \
                     phases are in daemon-events.jsonl",
                    budget.as_secs()
                ),
            )
        } else {
            err
        }
    })
}

fn send_daemon_request_over_tcp(
    state_dir: &Path,
    request: &DaemonRequest,
    timeout: Option<Duration>,
) -> io::Result<DaemonResponse> {
    let info = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir))?;
    let address = SocketAddr::from(([127, 0, 0, 1], info.port));
    let mut stream = match timeout {
        Some(timeout) => TcpStream::connect_timeout(&address, timeout)?,
        None => TcpStream::connect(address)?,
    };
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    write_daemon_request(
        &mut stream,
        request,
        daemon_wire_format_from_env().map_err(wire_error_to_io)?,
    )?;
    let line = match timeout {
        Some(timeout) => read_daemon_response_with_deadline(&mut stream, timeout)?,
        None => {
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line)?;
            line
        }
    };
    if line.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "daemon closed connection without replying",
        ));
    }
    decode_daemon_response_line(&line).map_err(wire_error_to_io)
}

fn read_daemon_response_with_deadline(
    stream: &mut TcpStream,
    timeout: Duration,
) -> io::Result<String> {
    const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
    let deadline = Instant::now() + timeout;
    let mut line = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon response exceeded dashboard deadline",
            ));
        }
        stream.set_read_timeout(Some(remaining))?;
        let bytes = stream.read(&mut chunk)?;
        if bytes == 0 {
            break;
        }
        let line_end = chunk[..bytes]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(bytes);
        if line.len() + line_end > MAX_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "daemon response exceeds dashboard size limit",
            ));
        }
        line.extend_from_slice(&chunk[..line_end]);
        if line_end < bytes || line.last() == Some(&b'\n') {
            break;
        }
    }
    String::from_utf8(line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn daemon_client_metrics(state_dir: &Path) -> io::Result<(u32, f32)> {
    match send_daemon_request(state_dir, &DaemonRequest::Metrics)? {
        DaemonResponse::Metrics { pid, cpu_pct } => Ok((pid, cpu_pct)),
        DaemonResponse::Error { message } => Err(io::Error::other(message)),
        response => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected daemon response: {response:?}"),
        )),
    }
}

pub(super) fn daemon_client_proc_snapshot(
    state_dir: &Path,
    include_dead_since_ms: u64,
) -> io::Result<ProcTreeSnapshot> {
    match send_daemon_request(
        state_dir,
        &DaemonRequest::ProcSnapshot {
            include_dead_since_ms,
        },
    )? {
        DaemonResponse::ProcSnapshot { snapshot } => Ok(snapshot),
        DaemonResponse::Error { message } => Err(io::Error::other(message)),
        response => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected daemon response: {response:?}"),
        )),
    }
}

/// The dashboard runs inside the daemon and treats the process tree as
/// optional data. Do not enter the broker frame lane or wait indefinitely on
/// a self-RPC: a silent peer would otherwise block the single dashboard HTTP
/// thread and make even unrelated `/state.json` polls time out (#1465).
pub(super) fn daemon_client_proc_snapshot_for_dashboard(
    state_dir: &Path,
) -> io::Result<ProcTreeSnapshot> {
    const SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(300);
    match send_daemon_request_over_tcp(
        state_dir,
        &DaemonRequest::ProcSnapshot {
            include_dead_since_ms: 0,
        },
        Some(SNAPSHOT_TIMEOUT),
    )? {
        DaemonResponse::ProcSnapshot { snapshot } => Ok(snapshot),
        DaemonResponse::Error { message } => Err(io::Error::other(message)),
        response => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected daemon response: {response:?}"),
        )),
    }
}

pub(super) fn request_session_termination(
    state_dir: &Path,
    session_id: &str,
) -> io::Result<SessionSnapshot> {
    match send_daemon_request(
        state_dir,
        &DaemonRequest::Terminate {
            session_id: session_id.to_string(),
        },
    )? {
        DaemonResponse::Terminated { session } => Ok(session),
        DaemonResponse::Error { message } => Err(io::Error::other(message)),
        response => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected daemon response: {response:?}"),
        )),
    }
}

pub(super) fn request_api_session_kill(state_dir: &Path, session_id: &str) -> io::Result<()> {
    match send_daemon_request(
        state_dir,
        &DaemonRequest::ApiSessionKill {
            session_id: session_id.to_string(),
        },
    )? {
        DaemonResponse::ApiSessionKilled { .. } => Ok(()),
        DaemonResponse::Error { message } => Err(io::Error::other(message)),
        response => Err(io::Error::other(format!(
            "unexpected API-session kill response: {response:?}"
        ))),
    }
}

/// Fire-and-forget handoff: ask the daemon to kill these process trees
/// on a background thread so the CLI can return from a Ctrl+C teardown
/// immediately. Returns `true` if the daemon acked the handoff. On
/// failure the caller is expected to fall back to a synchronous kill
/// (best behavior: same as before this op existed).
///
/// Uses tight read/write timeouts so a wedged daemon never blocks the
/// CLI for more than ~250ms total — the entire point of this call is
/// sub-100ms latency on Ctrl+C. Errors are logged at most once via the
/// returned bool; the caller decides whether to surface them.
pub fn try_handoff_kill_to_daemon(state_dir: &Path, pids: &[u32], reason: Option<&str>) -> bool {
    if pids.is_empty() {
        return true;
    }
    let info = match read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)) {
        Ok(info) => info,
        Err(_) => return false,
    };
    let mut stream = match TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], info.port)),
        Duration::from_millis(150),
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
    let request = DaemonRequest::AdoptKill {
        pids: pids.to_vec(),
        reason: reason.map(|s| s.to_string()),
    };
    let Ok(format) = daemon_wire_format_from_env() else {
        return false;
    };
    if write_daemon_request(&mut stream, &request, format).is_err() {
        return false;
    }
    // We could parse the ack here, but the wire contract guarantees the
    // daemon spawns its worker before replying; receiving any bytes back
    // means our PIDs are queued.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    matches!(reader.read_line(&mut line), Ok(n) if n > 0)
}

/// Fire-and-forget: ask the daemon to sweep dead-originator CLUD orphans.
/// Called from the foreground clud's exit hook. Tight 150ms read/write
/// timeouts (mirrors `try_handoff_kill_to_daemon`) so a wedged daemon
/// can't drag out CLI shutdown. Returns `true` when the ack arrives.
///
/// Failure modes (daemon down, version skew, network hiccup) all degrade
/// silently — the daemon will still catch any dead-originator orphans on
/// its next periodic sweep (`gc_service`-adjacent path), and the next
/// `clud slay` invocation does the synchronous version.
pub fn try_request_orphan_reap(state_dir: &Path) -> bool {
    let info = match read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)) {
        Ok(info) => info,
        Err(_) => return false,
    };
    let mut stream = match TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], info.port)),
        Duration::from_millis(150),
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
    let Ok(format) = daemon_wire_format_from_env() else {
        return false;
    };
    if write_daemon_request(&mut stream, &DaemonRequest::ReapOrphans, format).is_err() {
        return false;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    matches!(reader.read_line(&mut line), Ok(n) if n > 0)
}

pub(super) fn request_session_interrupt(
    state_dir: &Path,
    session_id: &str,
    profile: CtrlCProfile,
) -> io::Result<SessionSnapshot> {
    match send_daemon_request(
        state_dir,
        &DaemonRequest::Interrupt {
            session_id: session_id.to_string(),
            profile,
        },
    )? {
        DaemonResponse::Interrupted { session } => Ok(session),
        DaemonResponse::Error { message } => Err(io::Error::other(message)),
        response => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected daemon response: {response:?}"),
        )),
    }
}

/// Ask the daemon to terminate and wait for its pid to exit. Returns the
/// daemon pid that was stopped. If the running daemon predates the shutdown
/// IPC and drops the connection on the unknown request, fall back to killing
/// the recorded pid tree directly; that is the version-skew state this
/// recovery path is meant to repair.
pub(super) fn request_daemon_shutdown(state_dir: &Path) -> io::Result<u32> {
    let info = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir))?;
    let recorded_pid = info.pid;
    let recorded = info.identity();
    if !identity_is_alive(&recorded) {
        let _ = fs::remove_file(daemon_info_path(state_dir));
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("daemon pid {recorded_pid} is not running"),
        ));
    }

    let pid = match send_daemon_request(
        state_dir,
        &DaemonRequest::Shutdown {
            client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            expected_daemon: Some(recorded),
        },
    ) {
        Ok(DaemonResponse::ShutdownAck { pid }) if pid == recorded_pid => pid,
        Ok(DaemonResponse::ShutdownAck { pid }) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon shutdown acknowledgment named pid {pid}, expected pid {recorded_pid}"
                ),
            ));
        }
        Ok(DaemonResponse::Error { message }) => {
            eprintln!("[clud] daemon pid {recorded_pid} rejected shutdown IPC ({message}); terminating its recorded process directly");
            terminate_recorded_daemon(&recorded);
            recorded_pid
        }
        Ok(response) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected daemon response: {response:?}"),
            ));
        }
        Err(err) if is_old_daemon_signature(&err) => {
            eprintln!(
                "[clud] daemon pid {recorded_pid} does not support shutdown IPC; terminating it directly"
            );
            terminate_recorded_daemon(&recorded);
            recorded_pid
        }
        Err(err) => return shutdown_connect_error(state_dir, &recorded, err),
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    while identity_is_alive(&recorded) {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("daemon pid {pid} did not exit within 10s after shutdown"),
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }

    // #1995: sysinfo stops reporting a process before macOS has released
    // its entry, and a caller that checks the same identity with another
    // reader then still sees it alive. Stop reports success only once the
    // OS has let go of the PID (or handed it to another process).
    let release_deadline = Instant::now() + Duration::from_secs(5);
    while !recorded.released() {
        if Instant::now() >= release_deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "daemon pid {pid} stopped, but the OS still holds its process entry 5s later"
                ),
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }

    match fs::remove_file(daemon_info_path(state_dir)) {
        Ok(()) => Ok(pid),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(pid),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("daemon pid {pid} stopped, but its daemon.json could not be removed: {err}"),
        )),
    }
}

fn terminate_recorded_daemon(recorded: &ProcessIdentity) {
    signal_process_tree_as(recorded, Signal::Term);
    thread::sleep(Duration::from_millis(150));
    if identity_is_alive(recorded) {
        signal_process_tree_as(recorded, Signal::Kill);
    }
}

/// A daemon can exit after its identity is checked but before the shutdown
/// client connects. Treat that stale record exactly like the pre-connect dead
/// identity case, while preserving real transport errors for live daemons.
fn shutdown_connect_error(
    state_dir: &Path,
    recorded: &ProcessIdentity,
    err: io::Error,
) -> io::Result<u32> {
    if identity_is_alive(recorded) {
        return Err(err);
    }
    let _ = fs::remove_file(daemon_info_path(state_dir));
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("daemon pid {} exited before shutdown", recorded.pid),
    ))
}

fn write_daemon_request(
    stream: &mut TcpStream,
    request: &DaemonRequest,
    format: DaemonWireFormat,
) -> io::Result<()> {
    let bytes = encode_daemon_request_line(request, format).map_err(wire_error_to_io)?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn wire_error_to_io(err: WireError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

pub(super) fn send_worker_message(
    writer: &Arc<Mutex<TcpStream>>,
    message: &WorkerClientMessage,
    format: DaemonWireFormat,
) -> io::Result<()> {
    let mut guard = writer.lock().expect("writer mutex poisoned");
    write_worker_message(&mut guard, message, format)
}

pub(super) fn write_worker_message(
    stream: &mut TcpStream,
    message: &WorkerClientMessage,
    format: DaemonWireFormat,
) -> io::Result<()> {
    let bytes = encode_worker_client_line(message, format).map_err(wire_error_to_io)?;
    stream.write_all(&bytes)?;
    stream.flush()
}

pub(super) fn shutdown_worker_connection(writer: &Arc<Mutex<TcpStream>>) -> io::Result<()> {
    let guard = writer.lock().expect("writer mutex poisoned");
    guard.shutdown(Shutdown::Both)
}

pub(super) fn cleanup_stale_state(state_dir: &Path) {
    // Clean stale session files: mark sessions whose worker is dead.
    if let Ok(entries) = fs::read_dir(sessions_dir(state_dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(mut session) = read_json_file::<SessionSnapshot>(&path) else {
                continue;
            };
            if session.exit_code.is_some() {
                continue;
            }
            // Reconciliation, not cleanup: a worker whose PID has been reused
            // is just as gone as one whose PID vanished, and the record is
            // marked stale either way. Nothing here signals a process, so the
            // replacement is never touched (issue #558).
            if !identity_is_alive(&session.worker_identity()) {
                session.exit_code = Some(137);
                session.background = false;
                let _ = super::io_helpers::write_json_file(&path, &session);
            }
        }
    }

    // Clean dangling spec files: specs with no corresponding session snapshot
    // that are older than 10 seconds (grace period for worker startup).
    if let Ok(entries) = fs::read_dir(specs_dir(state_dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let session_id = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let snapshot_path = session_snapshot_path(state_dir, session_id);
            if snapshot_path.exists() {
                continue;
            }
            // Only remove if the spec is old enough (worker may still be starting).
            let is_stale = path
                .metadata()
                .and_then(|m| m.modified())
                .map(|modified| modified.elapsed().unwrap_or_default() > Duration::from_secs(10))
                .unwrap_or(true);
            if is_stale {
                let _ = fs::remove_file(&path);
            }
        }
    }

    // Clean stale daemon.json if it refers to a dead process.
    let daemon_path = daemon_info_path(state_dir);
    if let Ok(info) = read_json_file::<DaemonInfo>(&daemon_path) {
        if !identity_is_alive(&info.identity()) {
            let _ = fs::remove_file(&daemon_path);
        }
    }
}

#[allow(dead_code)]
pub(super) fn remove_spec_file(state_dir: &Path, session_id: &str) {
    let _ = fs::remove_file(spec_path(state_dir, session_id));
}

// ---------- GC IPC client wrappers (issue #135) ----------
//
// Thin convenience layer around `send_daemon_request` for the GC ops the
// session daemon now serves (replacing the standalone `gc_daemon`
// process). Auto-spawn the daemon on first use so the CLI works the
// same way it did against gc_daemon: `clud gc list` from a cold start
// brings the daemon up.

fn send_gc(state_dir: &Path, op: GcOp) -> io::Result<GcReply> {
    ensure_daemon(state_dir)?;
    match send_daemon_request(state_dir, &DaemonRequest::Gc { payload: op })? {
        DaemonResponse::Gc { reply } => Ok(reply),
        DaemonResponse::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected daemon response: {other:?}"),
        )),
    }
}

/// `gc.list` — fetch every tracked row.
pub fn gc_client_list(state_dir: &Path, kind: Option<&str>) -> io::Result<Vec<ListRow>> {
    match send_gc(
        state_dir,
        GcOp::List {
            kind: kind.map(String::from),
        },
    )? {
        GcReply::ListOk { rows } => Ok(rows),
        GcReply::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected gc reply: {other:?}"))),
    }
}

/// Outcome of a `gc.purge` IPC call. Bulk non-dry-run purges fan out
/// to the daemon's parallel purge pool and return as
/// `Started { dispatched, skipped }`; dry-run and the per-row
/// `DeleteById` paths complete synchronously and return
/// `Completed { removed, skipped }` (#268).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcPurgeOutcome {
    Completed { removed: usize, skipped: usize },
    Started { dispatched: usize, skipped: usize },
}

/// `gc.purge` — purge entries. `duration = None` -> purge all non-live-locked.
pub fn gc_client_purge(
    state_dir: &Path,
    duration: Option<&str>,
    kind: Option<&str>,
    dry_run: bool,
) -> io::Result<GcPurgeOutcome> {
    match send_gc(
        state_dir,
        GcOp::Purge {
            duration: duration.map(String::from),
            kind: kind.map(String::from),
            dry_run,
        },
    )? {
        GcReply::PurgeOk { removed, skipped } => Ok(GcPurgeOutcome::Completed { removed, skipped }),
        GcReply::PurgeStarted {
            dispatched,
            skipped,
        } => Ok(GcPurgeOutcome::Started {
            dispatched,
            skipped,
        }),
        GcReply::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected gc reply: {other:?}"))),
    }
}

/// `gc.reconcile` — walk the given repo's `.claude/worktrees/` and insert
/// new agent-* subdirs. Returns the number of newly-inserted rows.
pub fn gc_client_reconcile(state_dir: &Path, repo_root: &Path) -> io::Result<usize> {
    match send_gc(
        state_dir,
        GcOp::Reconcile {
            repo_root: repo_root.to_string_lossy().to_string(),
        },
    )? {
        GcReply::ReconcileOk { inserted } => Ok(inserted),
        GcReply::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected gc reply: {other:?}"))),
    }
}

/// `gc.insert` — insert a single row if not already present.
pub fn gc_client_insert(state_dir: &Path, input: &InsertInput) -> io::Result<()> {
    match send_gc(
        state_dir,
        GcOp::Insert {
            kind: input.kind.clone(),
            path: input.path.clone(),
            repo_root: input.repo_root.clone(),
            branch: input.branch.clone(),
            agent_id: input.agent_id.clone(),
            created_unix: Some(input.now_unix),
        },
    )? {
        GcReply::InsertOk { .. } => Ok(()),
        GcReply::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected gc reply: {other:?}"))),
    }
}

/// Register daemon-owned GC discovery roots without delaying foreground
/// startup. An old daemon closes the stream on the unknown operation; all
/// transport/version-skew failures intentionally degrade to no discovery for
/// this client rather than breaking the backend launch (#545/#546).
pub fn try_register_gc_watch(state_dir: &Path, roots: &[GcWatchRoot]) -> bool {
    if roots.is_empty() {
        return true;
    }
    let info = match read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)) {
        Ok(info) => info,
        Err(_) => return false,
    };
    let Ok(format) = daemon_wire_format_from_env() else {
        return false;
    };
    for root in roots {
        let mut stream = match TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], info.port)),
            Duration::from_millis(150),
        ) {
            Ok(stream) => stream,
            Err(_) => return false,
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
        let request = DaemonRequest::Gc {
            payload: GcOp::Watch {
                kind: root.kind.clone(),
                watch_dir: root.watch_dir.clone(),
                repo_root: root.repo_root.clone(),
            },
        };
        if write_daemon_request(&mut stream, &request, format).is_err() {
            return false;
        }
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
            return false;
        }
    }
    true
}

/// Issue #183: upsert a `repo_visits` row. Called by `clud` startup
/// when CWD is inside a git repo. Errors are swallowed by the caller —
/// failing to record a visit must never block a launch.
pub fn gc_client_record_repo_visit(
    state_dir: &Path,
    repo_root: &Path,
    cwd: &Path,
) -> io::Result<()> {
    match send_gc(
        state_dir,
        GcOp::RecordRepoVisit {
            repo_root: repo_root.to_string_lossy().to_string(),
            cwd: cwd.to_string_lossy().to_string(),
            now_unix: None,
        },
    )? {
        GcReply::RepoVisitOk => Ok(()),
        GcReply::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected gc reply: {other:?}"))),
    }
}

/// Issue #183: enumerate the `repo_visits` table for the dashboard /
/// `clud ui --json` payload.
pub fn gc_client_list_repo_visits(state_dir: &Path) -> io::Result<Vec<RepoVisit>> {
    match send_gc(state_dir, GcOp::ListRepoVisits)? {
        GcReply::RepoVisitsOk { rows } => Ok(rows),
        GcReply::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected gc reply: {other:?}"))),
    }
}

/// #1666 (DD-135): record one creation-ledger row. Internal API for the clud
/// helper that performed the create (#1667); never exposed to agents.
pub fn gc_client_insert_created(
    state_dir: &Path,
    entry: &crate::gc::CreatedEntry,
) -> io::Result<bool> {
    match send_gc(
        state_dir,
        GcOp::InsertCreated {
            entry: entry.clone(),
        },
    )? {
        GcReply::CreatedInsertOk { inserted } => Ok(inserted),
        GcReply::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected gc reply: {other:?}"))),
    }
}

/// How long `safe-rm` waits for the daemon's ledger answer before refusing.
pub const LEDGER_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// #1666: the session's creation-ledger rows naming or containing `path`.
/// Never spawns a daemon: `safe-rm` treats an unreachable daemon, an old
/// daemon that does not know the op, or a timeout as "ledger unavailable".
pub fn gc_client_query_created(
    state_dir: &Path,
    session_id: &str,
    path: &str,
    timeout: Duration,
) -> io::Result<Vec<crate::gc::CreatedEntry>> {
    let request = DaemonRequest::Gc {
        payload: GcOp::QueryCreated {
            session_id: session_id.to_string(),
            path: path.to_string(),
        },
    };
    match send_daemon_request_over_tcp(state_dir, &request, Some(timeout))? {
        DaemonResponse::Gc {
            reply: GcReply::CreatedRowsOk { rows },
        } => Ok(rows),
        DaemonResponse::Gc {
            reply: GcReply::Error { message },
        }
        | DaemonResponse::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!(
            "unexpected daemon response: {other:?}"
        ))),
    }
}

#[cfg(test)]
#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;

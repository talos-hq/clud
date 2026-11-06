use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_sdk::{
    read_daemon_identity_file, remove_daemon_identity_file, write_daemon_identity_file,
    BackendEndpointMux, LegacyClassification, MuxPoll,
};
use running_process::broker::builders::{CacheManifestBuilder, ServiceDefinitionBuilder};
use running_process::broker::protocol::CacheRootKind;
use running_process::broker::protocol::{encode_framed, Frame};
use running_process::broker::server::local_socket_name;
use running_process::NativeProcess;

use super::super::activity::DaemonActivity;
use super::super::api_session_lifecycle::ApiSessionLifecycle;
use super::super::client_leases::ClientLeaseRegistry;
use super::super::gc_service::RegistryMsg;
use super::super::proc_sampler::ProcSamplerHandle;
use super::endpoint::{daemon_identity_path, endpoint_for_state_dir};
use super::payload::answer_payload_frame;
use super::{
    running_process_disabled, CLUD_PAYLOAD_PROTOCOL, RUNNING_PROCESS_MIN_VERSION,
    RUNNING_PROCESS_SERVICE_NAME,
};

/// Handle to the running frame lane; cleans up the on-disk artifacts on
/// daemon shutdown. The accept thread itself is detached and blocked in
/// `accept()` — it dies with the process, which is the daemon's normal
/// exit path right after `run_daemon` returns.
pub(in crate::daemon) struct FrameLane {
    identity_path: PathBuf,
    endpoint_path: String,
    /// Identity this lane wrote to the sidecar: `(pid, started_at_unix_ms)`.
    owner: (u32, u64),
    /// `(dev, inode)` of the sidecar right after this lane wrote it. A
    /// superseding daemon replaces the file by atomic rename, so its inode
    /// differs. `None` where the host has no file identity.
    sidecar_id: Option<(u64, u64)>,
    /// `(dev, inode)` of the socket file right after this lane bound it.
    socket_id: Option<(u64, u64)>,
}

impl FrameLane {
    pub(super) fn new(
        identity_path: PathBuf,
        endpoint_path: String,
        daemon: &DaemonProcess,
        sidecar_id: Option<(u64, u64)>,
        socket_id: Option<(u64, u64)>,
    ) -> Self {
        Self {
            identity_path,
            endpoint_path,
            owner: (daemon.pid, daemon.started_at_unix_ms),
            sidecar_id,
            socket_id,
        }
    }

    /// Remove the sidecar and socket path, but only while the sidecar is
    /// still ours (pid + start time, and its inode when recorded). A
    /// successor always writes its sidecar before it binds, so a sidecar that
    /// is not ours means the socket at the endpoint is not ours either, even
    /// when the filesystem reused our socket's `(dev, inode)` for it (#2340).
    /// An unreadable sidecar removes neither file. The socket inode compare
    /// stays as an extra guard. There is no connect probe: our own listener
    /// is still live here, so a probe would always see "a listener".
    ///
    /// The stat-then-unlink window remains: a successor that replaces a file
    /// between our check and our `remove_file` loses it. The consequence is
    /// bounded: clients fall back to the TCP wire.
    pub(in crate::daemon) fn cleanup(&self) {
        if crate::platform::host::WINDOWS {
            // Named pipes vanish with their last handle; only the sidecar
            // is on disk, and without inodes the identity read is the guard.
            if sidecar_is_ours(&self.identity_path, self.owner, None) {
                remove_daemon_identity_file(&self.identity_path);
            }
            return;
        }
        if !sidecar_is_ours(&self.identity_path, self.owner, self.sidecar_id) {
            return;
        }
        let path = Path::new(&self.endpoint_path);
        if self.socket_id.is_some()
            && crate::platform::file_identity::file_identity(path) == self.socket_id
        {
            let _ = std::fs::remove_file(path);
        }
        remove_daemon_identity_file(&self.identity_path);
    }
}

/// Whether the sidecar is still ours: its inode (when recorded) matches
/// `sidecar_id` and it carries `owner`'s identity. Reads the file once.
fn sidecar_is_ours(
    identity_path: &Path,
    owner: (u32, u64),
    sidecar_id: Option<(u64, u64)>,
) -> bool {
    if sidecar_id.is_some()
        && crate::platform::file_identity::file_identity(identity_path) != sidecar_id
    {
        return false;
    }
    read_daemon_identity_file(identity_path).is_some_and(|d| (d.pid, d.started_at_unix_ms) == owner)
}

/// Remove the sidecar only while it is ours.
fn remove_sidecar_if_ours(identity_path: &Path, owner: (u32, u64), sidecar_id: Option<(u64, u64)>) {
    if sidecar_is_ours(identity_path, owner, sidecar_id) {
        remove_daemon_identity_file(identity_path);
    }
}

/// Start the broker v1 frame lane, best-effort.
///
/// Returns `None` (after at most one stderr note) when the lane is
/// disabled via `RUNNING_PROCESS_DISABLE=1` or fails to come up; the
/// daemon's TCP wire keeps working either way.
#[allow(clippy::too_many_arguments)]
pub(in crate::daemon) fn spawn_frame_lane(
    state_dir: &Path,
    workers: Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    gc_tx: Option<mpsc::Sender<RegistryMsg>>,
    shutdown_requested: Arc<AtomicBool>,
    proc_sampler: ProcSamplerHandle,
    client_leases: ClientLeaseRegistry,
    activity: DaemonActivity,
    api_lifecycle: Arc<ApiSessionLifecycle>,
) -> Option<FrameLane> {
    if running_process_disabled() {
        return None;
    }
    // Packaged `.servicedef` (soldr#722 pattern, #385 item 4): refresh
    // clud's service definition on every daemon bringup so the broker's
    // registry always points at the binary that is actually serving.
    // Best-effort and independent of the frame lane. Skipped under
    // `cfg!(test)` — unit tests spawn `run_daemon` in-process and must
    // not register the throwaway test executable in the user's real
    // service-definition directory; `install_service_definition` itself
    // is covered by a dedicated test against a temp dir.
    if !cfg!(test) {
        if let Err(err) = install_service_definition() {
            eprintln!("[clud] note: running-process servicedef install skipped: {err}");
        }
        if let Err(err) = register_this_build() {
            eprintln!("[clud] note: running-process v2 servicedef install skipped: {err}");
        }
        // Publish clud's CacheManifest (runtime/lock/config/log roots) so
        // peers can discover this daemon through the central registry
        // (#436, consumer-adoption-clud.md step 9). Best-effort and
        // independent of the frame lane; skipped under `cfg!(test)` for
        // the same reason as the servicedef install.
        if let Err(err) = publish_cache_manifest(state_dir) {
            eprintln!("[clud] note: running-process cache manifest publish skipped: {err}");
        }
    }
    match start_frame_lane(
        state_dir,
        workers,
        gc_tx,
        shutdown_requested,
        proc_sampler,
        client_leases,
        activity,
        api_lifecycle,
    ) {
        Ok(lane) => Some(lane),
        Err(err) => {
            eprintln!("[clud] note: running-process frame lane unavailable: {err}");
            None
        }
    }
}

/// Install/refresh `clud.servicedef` in the running-process
/// service-definition directory (`RUNNING_PROCESS_SERVICE_DEF_DIR`
/// override honored by running-process's `service_definition_dir`). The definition uses
/// SHARED_BROKER isolation, declares `min_version` 2.0.0, and points at
/// the current executable.
///
/// Built through [`ServiceDefinitionBuilder`] (#436 / #433 frozen
/// builders) so the broker boilerplate is defaulted and validated on
/// `install` instead of hand-spelled.
pub(in crate::daemon) fn install_service_definition() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    ServiceDefinitionBuilder::shared_broker(
        RUNNING_PROCESS_SERVICE_NAME,
        exe.to_string_lossy().into_owned(),
    )
    .min_version(RUNNING_PROCESS_MIN_VERSION)
    .allow_version(env!("CARGO_PKG_VERSION"))
    .label("consumer", "clud")
    .install()
    .map_err(|err| io::Error::other(err.to_string()))
}

/// Register the running daemon's build with the v2 broker registry and hold
/// its build directory's lease for the daemon's life, which also covers the
/// workers it starts from the same image (DD-204).
fn register_this_build() -> io::Result<()> {
    let home = crate::home::user_home().ok_or_else(|| io::Error::other("no home directory"))?;
    let exe = std::env::current_exe()?;
    let build = crate::version_dir::place(&home, &exe)?;
    install_service_definition_v2_in(
        &running_process::broker::protocol_v2::service_definition_dir_v2(),
        &build,
    )?;
    prune_service_definitions_v2(
        &running_process::broker::protocol_v2::service_definition_dir_v2(),
        &home,
    );
    Ok(())
}

/// Write `<service>.servicedef.v2` for `build`: its pinned image, the image
/// directory as the allow-list root, and exactly this version allowed, as
/// soldr's `soldr_daemon_service_definition` does.
pub(in crate::daemon) fn install_service_definition_v2_in(
    root: &Path,
    build: &crate::version_dir::VersionDir,
) -> io::Result<PathBuf> {
    use running_process::broker::protocol_v2::ServiceDefinitionBuilder as BuilderV2;
    let name = crate::version_dir::service_name(build)
        .ok_or_else(|| io::Error::other("build directory is not a valid service name"))?;
    let image = build.image();
    let image_dir = image
        .parent()
        .ok_or_else(|| io::Error::other("image has no parent"))?;
    BuilderV2::shared_broker(name, image.display().to_string())
        .per_version_binary_dir(image_dir.display().to_string())
        .min_version(env!("CARGO_PKG_VERSION"))
        .version_allow_list([env!("CARGO_PKG_VERSION")])
        .label("consumer", "clud")
        .install_in(root)
        .map_err(|err| io::Error::other(err.to_string()))
}

/// Remove clud's v2 definitions whose build directory the GC has collected.
/// Other consumers' files and anything unreadable stay.
pub(in crate::daemon) fn prune_service_definitions_v2(root: &Path, home: &Path) {
    use prost::Message as _;
    use running_process::broker::protocol_v2::{ServiceDefinition, SERVICE_DEF_V2_EXTENSION};
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let builds = home.join(crate::version_dir::ROOT_SUBDIR);
    for path in entries.flatten().map(|entry| entry.path()) {
        let is_ours = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(crate::version_dir::SERVICE_PREFIX)
                    && name.ends_with(&format!(".{SERVICE_DEF_V2_EXTENSION}"))
            });
        if !is_ours {
            continue;
        }
        let Some(definition) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| ServiceDefinition::decode(bytes.as_slice()).ok())
        else {
            continue;
        };
        let dir = Path::new(&definition.per_version_binary_dir);
        if dir.starts_with(&builds) && !dir.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Seal and publish clud's [`CacheManifest`] into the central registry
/// (#436 / #433 builders, consumer-adoption-clud.md step 9). Records the
/// daemon's runtime, lock, config, and log roots so peers can discover
/// the cache. `RUNNING_PROCESS_MANIFEST_DIR` (honored by the running-
/// process registry helpers) redirects the registry root for tests.
///
/// The roots map clud's on-disk layout (`paths.rs`) onto the broker's
/// [`CacheRootKind`] taxonomy:
/// - runtime  → `state_dir` (`CacheRuntime`)
/// - lock     → `state_dir/daemon.lock` (`CacheLocks`)
/// - config   → `~/.clud` (`CacheConfig`)
/// - log      → `state_dir/logs` (`CacheLogs`)
pub(in crate::daemon) fn publish_cache_manifest(state_dir: &Path) -> io::Result<PathBuf> {
    let runtime_root = state_dir.to_string_lossy().into_owned();
    let lock_root = state_dir.join("daemon.lock").to_string_lossy().into_owned();
    let log_root = state_dir.join("logs").to_string_lossy().into_owned();
    let config_root = crate::home::user_home()
        .map(|home| home.join(".clud"))
        .unwrap_or_else(|| state_dir.to_path_buf())
        .to_string_lossy()
        .into_owned();

    CacheManifestBuilder::new(RUNNING_PROCESS_SERVICE_NAME, env!("CARGO_PKG_VERSION"))
        .broker_instance("shared")
        .root(CacheRootKind::CacheRuntime, runtime_root)
        .root(CacheRootKind::CacheLocks, lock_root)
        .root(CacheRootKind::CacheConfig, config_root)
        .root(CacheRootKind::CacheLogs, log_root)
        .publish()
        .map_err(|err| io::Error::other(err.to_string()))
}

fn bind_listener(endpoint_path: &str) -> io::Result<interprocess::local_socket::Listener> {
    if !crate::platform::host::WINDOWS {
        // A previous daemon that died uncleanly leaves the socket file
        // behind; binding fails with AddrInUse unless it is removed.
        let _ = std::fs::remove_file(endpoint_path);
    }
    let name = local_socket_name(endpoint_path)?;
    ListenerOptions::new().name(name).create_sync()
}

/// Claim the endpoint for `daemon`: write the sidecar first (the ownership
/// claim, so a superseded daemon's cleanup never sees its own identity while
/// our socket is bound), then bind, recording both files' identities.
pub(super) fn claim_endpoint(
    state_dir: &Path,
    daemon: &DaemonProcess,
    endpoint_path: String,
) -> io::Result<(FrameLane, interprocess::local_socket::Listener)> {
    let identity_path = daemon_identity_path(state_dir);
    write_daemon_identity_file(&identity_path, daemon)?;
    let sidecar_id = crate::platform::file_identity::file_identity(&identity_path);
    let owner = (daemon.pid, daemon.started_at_unix_ms);
    let listener = match bind_listener(&endpoint_path) {
        Ok(listener) => listener,
        Err(err) => {
            remove_sidecar_if_ours(&identity_path, owner, sidecar_id);
            return Err(err);
        }
    };
    let socket_id = crate::platform::file_identity::file_identity(Path::new(&endpoint_path));
    let lane = FrameLane::new(identity_path, endpoint_path, daemon, sidecar_id, socket_id);
    Ok((lane, listener))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start_frame_lane(
    state_dir: &Path,
    workers: Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    gc_tx: Option<mpsc::Sender<RegistryMsg>>,
    shutdown_requested: Arc<AtomicBool>,
    proc_sampler: ProcSamplerHandle,
    client_leases: ClientLeaseRegistry,
    activity: DaemonActivity,
    api_lifecycle: Arc<ApiSessionLifecycle>,
) -> io::Result<FrameLane> {
    let endpoint = endpoint_for_state_dir(state_dir)?;
    let endpoint_path = endpoint.path.clone();

    let daemon = DaemonProcess::current_process(endpoint, None)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let (lane, listener) = claim_endpoint(state_dir, &daemon, endpoint_path)?;

    let mux = Arc::new(BackendEndpointMux::new(
        daemon,
        &[CLUD_PAYLOAD_PROTOCOL],
        // This endpoint is new with the adoption — it has no legacy
        // wire. The TCP listener keeps serving the legacy line formats.
        |_buf: &[u8]| LegacyClassification::NotLegacy,
    ));

    let state_dir = state_dir.to_path_buf();
    thread::Builder::new()
        .name("clud-rp-frame-lane".to_string())
        .spawn(move || loop {
            match listener.accept() {
                Ok(stream) => {
                    if shutdown_requested.load(Ordering::SeqCst) {
                        return;
                    }
                    let mux = Arc::clone(&mux);
                    let state_dir = state_dir.clone();
                    let workers = Arc::clone(&workers);
                    let gc_tx = gc_tx.clone();
                    let shutdown_requested = Arc::clone(&shutdown_requested);
                    let proc_sampler = proc_sampler.clone();
                    let client_leases = client_leases.clone();
                    let activity = activity.clone();
                    let api_lifecycle = Arc::clone(&api_lifecycle);
                    thread::spawn(move || {
                        let _connection_guard = activity.start_connection();
                        let mut stream = stream;
                        let _ = serve_connection(
                            &mut stream,
                            &mux,
                            &ConnectionServices {
                                state_dir: &state_dir,
                                workers: &workers,
                                gc_tx: gc_tx.as_ref(),
                                proc_sampler: Some(&proc_sampler),
                                client_leases: &client_leases,
                                api_lifecycle: &api_lifecycle,
                            },
                            &shutdown_requested,
                        );
                    });
                }
                Err(_) => {
                    if shutdown_requested.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        })?;

    Ok(lane)
}

/// Serve one accepted connection through the mux until the peer
/// disconnects: identity probes are answered by the SDK, payload frames
/// are decoded as `ClientToDaemon` prost bytes and dispatched through
/// the same [`dispatch_daemon_request`] the TCP wire uses.
///
/// Canonical accept-loop shape from running-process
/// `tests/broker/backend_sdk.rs::serve_connection` / `docs/INTEGRATE.md`.
/// The daemon-owned services one connection may dispatch against.
///
/// Bundled rather than passed as loose parameters: adding the lease registry
/// (#643) took `serve_connection` to eight arguments, and these five travel
/// together to every request handler anyway. A struct also means the next
/// service to land does not re-open this question.
struct ConnectionServices<'a> {
    state_dir: &'a Path,
    workers: &'a Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    gc_tx: Option<&'a mpsc::Sender<RegistryMsg>>,
    proc_sampler: Option<&'a ProcSamplerHandle>,
    client_leases: &'a ClientLeaseRegistry,
    api_lifecycle: &'a Arc<ApiSessionLifecycle>,
}

fn serve_connection<S, F>(
    stream: &mut S,
    mux: &BackendEndpointMux<F>,
    services: &ConnectionServices<'_>,
    shutdown_requested: &Arc<AtomicBool>,
) -> io::Result<()>
where
    S: Read + Write,
    F: Fn(&[u8]) -> LegacyClassification,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match mux.poll(&buf).map_err(io::Error::other)? {
            MuxPoll::NeedMoreBytes => {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    if buf.is_empty() {
                        return Ok(());
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "peer closed mid-frame",
                    ));
                }
                buf.extend_from_slice(&chunk[..read]);
            }
            MuxPoll::ProbeAnswered { reply, consumed } => {
                stream.write_all(&reply)?;
                stream.flush()?;
                buf.drain(..consumed);
            }
            MuxPoll::Payload { frame, consumed } => {
                buf.drain(..consumed);
                let response = answer_payload_frame(
                    &frame,
                    services.state_dir,
                    services.workers,
                    services.gc_tx,
                    services.proc_sampler,
                    services.client_leases,
                    services.api_lifecycle,
                );
                let is_shutdown = response.is_shutdown;
                let wire = encode_framed(&Frame::response_to(&frame, response.payload))
                    .map_err(io::Error::other)?;
                stream.write_all(&wire)?;
                stream.flush()?;
                if is_shutdown {
                    // Match the TCP lane: flag shutdown only after the
                    // ack bytes are on the wire so the requester always
                    // hears back.
                    shutdown_requested.store(true, Ordering::SeqCst);
                    return Ok(());
                }
            }
            MuxPoll::Legacy => {
                // This endpoint has no legacy wire; the detector always
                // says NotLegacy, so this verdict is unreachable.
                return Err(io::Error::other(
                    "unexpected legacy classification on frame-only endpoint",
                ));
            }
        }
    }
}

use super::super::io_helpers::read_json_file;
use super::super::paths::daemon_info_path;
use super::super::server::run_daemon_uninterrupted;
use super::super::types::DaemonInfo;
use super::endpoint::endpoint_for_state_dir;
use super::*;
use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
use running_process::broker::backend_sdk::{
    read_daemon_identity_file, write_daemon_identity_file, FrameClient,
};
use running_process::broker::protocol::{encode_framed, Frame};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

/// Serializes the env mutations in this module AND pins the
/// RUNNING_PROCESS_DISABLE state each test depends on.
struct EnvGuard {
    priors: Vec<(&'static str, Option<String>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn set(key: &'static str, value: &str) -> Self {
        Self::apply(vec![(key, Some(value.to_string()))])
    }

    fn unset(key: &'static str) -> Self {
        Self::apply(vec![(key, None)])
    }

    /// Set/unset several variables under ONE lock acquisition (the
    /// mutex is not reentrant — nesting two guards would deadlock).
    fn apply(vars: Vec<(&'static str, Option<String>)>) -> Self {
        let lock = Self::lock();
        let priors = vars
            .into_iter()
            .map(|(key, value)| {
                let prior = std::env::var(key).ok();
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
                (key, prior)
            })
            .collect();
        Self {
            priors,
            _lock: lock,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prior) in self.priors.drain(..) {
            match prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

// The 10s deadline absorbs Linux-ARM CI runner variance — the previous
// 5s was tight enough that the rp_broker tests timed out when daemon
// startup picked up extra overhead from `debug = "line-tables-only"`
// (larger binary, slightly slower image load) and the panic-hook
// installer that now runs at the top of `run_daemon`. The daemon itself
// is normally ready in <500ms on all platforms; the bump only matters
// when the runner is under load.
fn wait_for_daemon_ready(state_dir: &Path) -> DaemonInfo {
    let deadline = Instant::now() + Duration::from_secs(10);
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

/// Waits for the sidecar with a load-tolerant deadline. The daemon publishes
/// `daemon.json` only after the sidecar exists (#2239), so callers that have
/// already seen readiness normally return on the first read; the deadline only
/// matters for a sidecar that is genuinely never written, and then the
/// failure names the path and what the state dir holds.
fn wait_for_identity_sidecar(state_dir: &Path) -> DaemonProcess {
    let path = daemon_identity_path(state_dir);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(identity) = read_daemon_identity_file(&path) {
            return identity;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon identity sidecar: path={} parent_exists={} \
             state_dir_entries={:?}",
            path.display(),
            path.parent().is_some_and(Path::exists),
            path.parent()
                .and_then(|dir| std::fs::read_dir(dir).ok())
                .map(|rd| rd
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()),
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// #2239 regression: the moment `daemon.json` is readable and the TCP port
/// accepts, the identity sidecar must already exist. No polling allowed.
#[test]
fn tier1_identity_sidecar_exists_by_the_time_daemon_reports_ready() {
    let _env = EnvGuard::unset(RUNNING_PROCESS_DISABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().to_path_buf();
    let daemon_state_dir = state_dir.clone();
    let daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&daemon_state_dir));

    let info = wait_for_daemon_ready(&state_dir);
    let sidecar = read_daemon_identity_file(&daemon_identity_path(&state_dir));
    let mut stream = TcpStream::connect(("127.0.0.1", info.port)).unwrap();
    use std::io::{BufRead, BufReader, Write as _};
    let request = serde_json::to_string(&DaemonRequest::Shutdown {
        client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        expected_daemon: Some(info.identity()),
    })
    .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let joined = daemon_thread.join().unwrap();
    assert!(
        sidecar.is_some(),
        "daemon.json was published before the identity sidecar (exit={joined})"
    );
}

/// FROZEN golden bytes for clud's Frame v1 wire (#385 adoption).
///
/// `encode_framed(Frame::request(0x7C4C, <prost ClientToDaemon
/// shutdown, request_id "golden">).with_request_id(1))`. The outer
/// layout is `[u8 framing_version=1][u32 LE body_len][prost Frame]`.
/// If this test ever fails, the wire drifted — fix the code, never
/// the constant.
#[test]
fn golden_bytes_pin_clud_frame_v1_request_wire() {
    let payload = encode_frame_lane_request(
        &DaemonRequest::Shutdown {
            client_version: None,
            expected_daemon: None,
        },
        "golden",
    )
    .expect("encode shutdown payload");
    let frame = Frame::request(CLUD_PAYLOAD_PROTOCOL, payload).with_request_id(1);
    let wire = encode_framed(&frame).expect("encode framed");

    let expected_hex = "0115000000080118ccf801220b4200a20606676f6c64656e2801";
    let got_hex: String = wire.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        got_hex, expected_hex,
        "clud Frame v1 golden bytes drifted; the wire is frozen-forever"
    );
}

#[test]
fn payload_protocol_is_frozen_registered_consumer_id() {
    assert_eq!(CLUD_PAYLOAD_PROTOCOL, 0x7C4C);
    assert_ne!(
        CLUD_PAYLOAD_PROTOCOL, 0x7A63,
        "must not collide with zccache"
    );
}

/// End-to-end: a real daemon answers a `BackendHandle` identity
/// probe via the sidecar, serves clud payload frames through
/// `FrameClient`, and shuts down via the frame lane.
#[test]
fn tier1_frame_lane_serves_probe_and_clud_requests_end_to_end() {
    let _env = EnvGuard::unset(RUNNING_PROCESS_DISABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().to_path_buf();
    let daemon_state_dir = state_dir.clone();
    let daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&daemon_state_dir));

    wait_for_daemon_ready(&state_dir);
    let identity = wait_for_identity_sidecar(&state_dir);

    let handle = BackendHandle::probe_with_service(
        RUNNING_PROCESS_SERVICE_NAME,
        env!("CARGO_PKG_VERSION"),
        &identity.ipc_endpoint,
        &identity,
    )
    .expect("daemon must answer the identity probe");
    assert!(handle.is_alive());

    let mut client = FrameClient::connect(&identity.ipc_endpoint).expect("connect frame client");
    let payload =
        encode_frame_lane_request(&DaemonRequest::ListLiveCwds, "rt-1").expect("encode request");
    let response_frame = client
        .request(CLUD_PAYLOAD_PROTOCOL, payload)
        .expect("frame round trip");
    let response = decode_frame_lane_response(&response_frame.payload).expect("decode response");
    assert!(matches!(response, DaemonResponse::LiveCwds { .. }));

    let payload = encode_frame_lane_request(
        &DaemonRequest::Shutdown {
            client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            expected_daemon: Some(crate::process_identity::ProcessIdentity::new(
                std::process::id(),
                crate::process_identity::self_start_time(),
            )),
        },
        "rt-2",
    )
    .expect("encode shutdown");
    let response_frame = client
        .request(CLUD_PAYLOAD_PROTOCOL, payload)
        .expect("shutdown round trip");
    let response = decode_frame_lane_response(&response_frame.payload).expect("decode shutdown");
    assert!(matches!(
        response,
        DaemonResponse::ShutdownAck { pid } if pid == std::process::id()
    ));
    drop(client);

    assert_eq!(daemon_thread.join().unwrap(), 0);
    assert!(
        read_daemon_identity_file(&daemon_identity_path(&state_dir)).is_none(),
        "daemon should remove the identity sidecar during shutdown"
    );
}

/// RUNNING_PROCESS_DISABLE=1 must restore pre-adoption behavior:
/// no identity sidecar, no frame endpoint, TCP wire untouched.
#[test]
fn tier1_disable_env_skips_frame_lane_entirely() {
    let _env = EnvGuard::set(RUNNING_PROCESS_DISABLE_ENV, "1");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().to_path_buf();
    let daemon_state_dir = state_dir.clone();
    let daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&daemon_state_dir));

    let info = wait_for_daemon_ready(&state_dir);
    assert!(
        !daemon_identity_path(&state_dir).exists(),
        "disabled lane must not write an identity sidecar"
    );

    // TCP wire still works and shuts the daemon down. The request has to be
    // versioned and generation-scoped: this test is about the frame lane being
    // disabled, not about shutdown authority, and an unversioned request is now
    // refused on purpose.
    let mut stream = TcpStream::connect(("127.0.0.1", info.port)).unwrap();
    use std::io::{BufRead, BufReader, Write as _};
    let request = serde_json::to_string(&super::super::types::DaemonRequest::Shutdown {
        client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        expected_daemon: Some(info.identity()),
    })
    .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("shutdown_ack"));
    assert_eq!(daemon_thread.join().unwrap(), 0);
}

/// Client wiring (#385 item 1): `send_daemon_request` uses the
/// frame lane when the sidecar is present; `try_send_via_frame_lane`
/// honors the disable hatch and degrades to `None` (TCP fallback)
/// when the sidecar is missing.
#[test]
fn tier1_client_round_trips_via_frame_lane_and_falls_back_to_tcp() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().to_path_buf();
    let daemon_state_dir = state_dir.clone();
    let daemon_thread;
    {
        let _env = EnvGuard::unset(RUNNING_PROCESS_DISABLE_ENV);
        daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&daemon_state_dir));
        wait_for_daemon_ready(&state_dir);
        wait_for_identity_sidecar(&state_dir);

        // Direct frame-lane round trip.
        let response = try_send_via_frame_lane(&state_dir, &DaemonRequest::ListLiveCwds)
            .expect("frame lane must answer while the sidecar is live");
        assert!(matches!(response, DaemonResponse::LiveCwds { .. }));

        // The public client entry point goes through the same lane.
        let response =
            super::super::client::send_daemon_request(&state_dir, &DaemonRequest::ListLiveCwds)
                .expect("send_daemon_request");
        assert!(matches!(response, DaemonResponse::LiveCwds { .. }));
    }

    {
        // Disable hatch bypasses the lane entirely...
        let _env = EnvGuard::set(RUNNING_PROCESS_DISABLE_ENV, "1");
        assert!(
            try_send_via_frame_lane(&state_dir, &DaemonRequest::ListLiveCwds).is_none(),
            "disable hatch must bypass the frame lane"
        );
        // ...while the legacy TCP wire keeps working underneath.
        let response =
            super::super::client::send_daemon_request(&state_dir, &DaemonRequest::ListLiveCwds)
                .expect("TCP fallback under the disable hatch");
        assert!(matches!(response, DaemonResponse::LiveCwds { .. }));
    }

    let _env = EnvGuard::unset(RUNNING_PROCESS_DISABLE_ENV);
    // A missing sidecar degrades to None (caller falls back to TCP).
    std::fs::remove_file(daemon_identity_path(&state_dir)).unwrap();
    assert!(
        try_send_via_frame_lane(&state_dir, &DaemonRequest::ListLiveCwds).is_none(),
        "missing sidecar must miss the frame lane"
    );
    let response = super::super::client::send_daemon_request(
        &state_dir,
        &DaemonRequest::Shutdown {
            client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            expected_daemon: Some(crate::process_identity::ProcessIdentity::new(
                std::process::id(),
                crate::process_identity::self_start_time(),
            )),
        },
    )
    .expect("shutdown via TCP fallback");
    assert!(matches!(response, DaemonResponse::ShutdownAck { .. }));
    assert_eq!(daemon_thread.join().unwrap(), 0);
}

/// The `RUNNING_PROCESS_FAKE_BACKEND` seam redirects the client to
/// the seam endpoint even when the cached sidecar endpoint is bogus.
#[test]
fn tier1_fake_backend_seam_overrides_cached_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().to_path_buf();
    let daemon_state_dir = state_dir.clone();
    let daemon_thread;
    let real_endpoint_path;
    {
        let _env = EnvGuard::unset(RUNNING_PROCESS_DISABLE_ENV);
        daemon_thread = thread::spawn(move || run_daemon_uninterrupted(&daemon_state_dir));
        wait_for_daemon_ready(&state_dir);
        let identity = wait_for_identity_sidecar(&state_dir);
        real_endpoint_path = identity.ipc_endpoint.path.clone();

        // Poison the sidecar's endpoint so a Hello-skip dial would miss.
        let mut poisoned = identity.clone();
        poisoned.ipc_endpoint.path = format!("{real_endpoint_path}-bogus");
        write_daemon_identity_file(&daemon_identity_path(&state_dir), &poisoned)
            .expect("rewrite sidecar");
    }

    {
        let _env = EnvGuard::apply(vec![
            (RUNNING_PROCESS_DISABLE_ENV, None),
            ("RUNNING_PROCESS_FAKE_BACKEND", Some(real_endpoint_path)),
        ]);
        let response = try_send_via_frame_lane(&state_dir, &DaemonRequest::ListLiveCwds)
            .expect("seam must route around the poisoned cached endpoint");
        assert!(matches!(response, DaemonResponse::LiveCwds { .. }));
        let response = try_send_via_frame_lane(
            &state_dir,
            &DaemonRequest::Shutdown {
                client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                expected_daemon: Some(crate::process_identity::ProcessIdentity::new(
                    std::process::id(),
                    crate::process_identity::self_start_time(),
                )),
            },
        )
        .expect("shutdown via seam");
        assert!(matches!(response, DaemonResponse::ShutdownAck { .. }));
    }
    assert_eq!(daemon_thread.join().unwrap(), 0);
}

/// `.servicedef` packaging (#385 item 4): the install helper writes
/// a valid SHARED_BROKER definition for service "clud" pointing at
/// the current executable, into the (env-overridden) directory.
#[test]
fn install_service_definition_writes_valid_shared_broker_servicedef() {
    use prost::Message as _;
    use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
    use running_process::broker::server::validate_service_definition_for_service;

    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(
        "RUNNING_PROCESS_SERVICE_DEF_DIR",
        tmp.path().to_str().unwrap(),
    );
    let path = install_service_definition().expect("install servicedef");
    assert_eq!(path, tmp.path().join("clud.servicedef"));

    let bytes = std::fs::read(&path).unwrap();
    let definition = ServiceDefinition::decode(bytes.as_slice()).unwrap();
    assert_eq!(definition.service_name, RUNNING_PROCESS_SERVICE_NAME);
    assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
    assert_eq!(
        std::path::PathBuf::from(&definition.binary_path),
        std::env::current_exe().unwrap()
    );
    // #436 step 8: SHARED_BROKER + min_version "2.0.0" + consumer label.
    assert_eq!(definition.min_version, RUNNING_PROCESS_MIN_VERSION);
    assert_eq!(definition.min_version, "2.0.0");
    assert_eq!(
        definition.labels.get("consumer").map(String::as_str),
        Some("clud")
    );
    validate_service_definition_for_service(&definition, RUNNING_PROCESS_SERVICE_NAME)
        .expect("definition must validate");

    // Idempotent refresh: a second install overwrites in place.
    let again = install_service_definition().expect("reinstall servicedef");
    assert_eq!(again, path);
}

/// #436 step 1: the daemon-boundary wire selector maps the canonical
/// escape hatch. `RUNNING_PROCESS_DISABLE=1` → json-legacy; anything
/// else → prost-v1.
#[test]
fn wire_mode_select_maps_the_disable_hatch() {
    let _env = EnvGuard::set(RUNNING_PROCESS_DISABLE_ENV, "1");
    assert_eq!(WireMode::select(), WireMode::JsonLegacy);
    assert_eq!(WireMode::JsonLegacy.as_str(), "json-legacy");
    drop(_env);

    let _env = EnvGuard::unset(RUNNING_PROCESS_DISABLE_ENV);
    assert_eq!(WireMode::select(), WireMode::ProstV1);
    assert_eq!(WireMode::ProstV1.as_str(), "prost-v1");
    drop(_env);

    // A non-`1` value does NOT engage the hatch — prost-v1 stays the
    // default (matches `running_process_disabled` exact-match rule).
    let _env = EnvGuard::set(RUNNING_PROCESS_DISABLE_ENV, "true");
    assert_eq!(WireMode::select(), WireMode::ProstV1);
}

/// #436 step 9: `publish_cache_manifest` seals a manifest recording
/// clud's runtime, lock, config, and log roots and writes it to the
/// (env-redirected) central registry; it round-trips through
/// `read_manifest`.
#[test]
fn publish_cache_manifest_records_clud_roots_and_round_trips() {
    use running_process::broker::manifest::read_manifest;
    use running_process::broker::protocol::{CacheManifest, CacheRootKind};

    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("manifests");
    let state_dir = tmp.path().join("state");
    let _env = EnvGuard::set("RUNNING_PROCESS_MANIFEST_DIR", registry.to_str().unwrap());

    let path = publish_cache_manifest(&state_dir).expect("publish manifest");
    assert!(path.exists(), "manifest must be written to the registry");

    let manifest: CacheManifest = read_manifest(&path).expect("read manifest back");
    assert_eq!(manifest.service_name, RUNNING_PROCESS_SERVICE_NAME);
    assert_eq!(manifest.service_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest.broker_instance, "shared");

    // Exactly the four declared roots, mapped onto the broker taxonomy.
    let kinds: Vec<i32> = manifest.roots.iter().map(|root| root.kind).collect();
    assert!(kinds.contains(&(CacheRootKind::CacheRuntime as i32)));
    assert!(kinds.contains(&(CacheRootKind::CacheLocks as i32)));
    assert!(kinds.contains(&(CacheRootKind::CacheConfig as i32)));
    assert!(kinds.contains(&(CacheRootKind::CacheLogs as i32)));
    assert_eq!(manifest.roots.len(), 4);

    let runtime = manifest
        .roots
        .iter()
        .find(|root| root.kind == CacheRootKind::CacheRuntime as i32)
        .expect("runtime root present");
    assert_eq!(
        std::path::PathBuf::from(&runtime.path),
        state_dir,
        "runtime root must be the daemon state dir"
    );
    let log = manifest
        .roots
        .iter()
        .find(|root| root.kind == CacheRootKind::CacheLogs as i32)
        .expect("log root present");
    assert_eq!(
        std::path::PathBuf::from(&log.path),
        state_dir.join("logs"),
        "log root must be <state_dir>/logs"
    );

    // self_sha256 is sealed (non-empty) by the builder.
    assert!(
        !manifest.self_sha256.is_empty(),
        "manifest digest must be sealed"
    );
}

#[test]
fn running_process_disabled_requires_exact_value() {
    let _env = EnvGuard::set(RUNNING_PROCESS_DISABLE_ENV, "1");
    assert!(running_process_disabled());
    drop(_env);
    let _env = EnvGuard::set(RUNNING_PROCESS_DISABLE_ENV, "true");
    assert!(!running_process_disabled());
    drop(_env);
    let _env = EnvGuard::unset(RUNNING_PROCESS_DISABLE_ENV);
    assert!(!running_process_disabled());
}

/// Build a lane that owns a freshly written sidecar and a placeholder
/// socket file at the shared endpoint path (#2340).
fn lane_with_sidecar(state_dir: &Path, started_at_unix_ms: u64) -> super::frame_lane::FrameLane {
    let endpoint = endpoint_for_state_dir(state_dir).expect("endpoint");
    let endpoint_path = endpoint.path.clone();
    let mut daemon = DaemonProcess::current_process(endpoint, None).expect("identity");
    daemon.started_at_unix_ms = started_at_unix_ms;
    let identity_path = daemon_identity_path(state_dir);
    write_daemon_identity_file(&identity_path, &daemon).expect("write sidecar");
    // Replace, as a bind does, so a successor gets a new inode.
    let _ = std::fs::remove_file(&endpoint_path);
    std::fs::write(&endpoint_path, b"").expect("socket placeholder");
    let sidecar_id = crate::platform::file_identity::file_identity(&identity_path);
    let socket_id = crate::platform::file_identity::file_identity(Path::new(&endpoint_path));
    super::frame_lane::FrameLane::new(identity_path, endpoint_path, &daemon, sidecar_id, socket_id)
}

/// A lane over a really bound listener, as `start_frame_lane` builds it.
fn real_lane(
    state_dir: &Path,
    started_at_unix_ms: u64,
) -> (
    super::frame_lane::FrameLane,
    interprocess::local_socket::Listener,
) {
    let endpoint = endpoint_for_state_dir(state_dir).expect("endpoint");
    let endpoint_path = endpoint.path.clone();
    let mut daemon = DaemonProcess::current_process(endpoint, None).expect("identity");
    daemon.started_at_unix_ms = started_at_unix_ms;
    super::frame_lane::claim_endpoint(state_dir, &daemon, endpoint_path).expect("claim")
}

#[test]
fn lane_cleanup_removes_its_own_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let lane = lane_with_sidecar(tmp.path(), 1_000);
    let identity_path = daemon_identity_path(tmp.path());
    assert!(identity_path.exists());
    lane.cleanup();
    assert!(!identity_path.exists());
    if !crate::platform::host::WINDOWS {
        let endpoint = endpoint_for_state_dir(tmp.path()).unwrap();
        assert!(!Path::new(&endpoint.path).exists());
    }
}

#[test]
fn superseded_lane_cleanup_keeps_the_newer_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let older = lane_with_sidecar(tmp.path(), 1_000);
    // The superseding daemon rewrites the sidecar at the same path.
    let _newer = lane_with_sidecar(tmp.path(), 2_000);
    older.cleanup();
    let identity_path = daemon_identity_path(tmp.path());
    let kept = read_daemon_identity_file(&identity_path).expect("newer sidecar must survive");
    assert_eq!(kept.started_at_unix_ms, 2_000);
    if !crate::platform::host::WINDOWS {
        let endpoint = endpoint_for_state_dir(tmp.path()).unwrap();
        assert!(Path::new(&endpoint.path).exists());
    }
}

#[test]
fn live_lane_cleanup_removes_its_own_socket_and_sidecar() {
    if crate::platform::host::WINDOWS {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (lane, _listener) = real_lane(tmp.path(), 1_000);
    let endpoint = endpoint_for_state_dir(tmp.path()).unwrap();
    assert!(Path::new(&endpoint.path).exists());
    lane.cleanup();
    assert!(!Path::new(&endpoint.path).exists(), "own socket kept");
    assert!(
        !daemon_identity_path(tmp.path()).exists(),
        "own sidecar kept"
    );
}

#[test]
fn superseded_lane_cleanup_keeps_the_newer_bound_socket() {
    use interprocess::local_socket::traits::{Listener as _, Stream as _};
    use running_process::broker::server::local_socket_name;
    if crate::platform::host::WINDOWS {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (older, _older_listener) = real_lane(tmp.path(), 1_000);
    let (_newer, listener) = real_lane(tmp.path(), 2_000);
    let endpoint = endpoint_for_state_dir(tmp.path()).unwrap();
    older.cleanup();
    assert!(Path::new(&endpoint.path).exists(), "newer socket removed");
    let kept = read_daemon_identity_file(&daemon_identity_path(tmp.path()))
        .expect("newer sidecar must survive");
    assert_eq!(kept.started_at_unix_ms, 2_000);
    let connect = thread::spawn({
        let path = endpoint.path.clone();
        move || interprocess::local_socket::Stream::connect(local_socket_name(&path).unwrap())
    });
    listener.accept().expect("accept");
    connect.join().unwrap().expect("connect to newer socket");
}

/// Inode reuse: A's recorded socket identity equals B's real socket, but the
/// sidecar belongs to B, so A's cleanup must remove nothing (#2340).
#[test]
fn cleanup_keeps_a_socket_whose_inode_was_reused_by_the_successor() {
    use interprocess::local_socket::traits::{Listener as _, Stream as _};
    use running_process::broker::server::local_socket_name;
    if crate::platform::host::WINDOWS {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (_b, listener) = real_lane(tmp.path(), 2_000);
    let endpoint = endpoint_for_state_dir(tmp.path()).unwrap();
    let identity_path = daemon_identity_path(tmp.path());
    let mut daemon_a = DaemonProcess::current_process(endpoint.clone(), None).expect("identity");
    daemon_a.started_at_unix_ms = 1_000;
    let a = super::frame_lane::FrameLane::new(
        identity_path.clone(),
        endpoint.path.clone(),
        &daemon_a,
        crate::platform::file_identity::file_identity(&identity_path),
        crate::platform::file_identity::file_identity(Path::new(&endpoint.path)),
    );
    a.cleanup();
    assert!(
        Path::new(&endpoint.path).exists(),
        "successor socket removed"
    );
    let kept = read_daemon_identity_file(&identity_path).expect("successor sidecar must survive");
    assert_eq!(kept.started_at_unix_ms, 2_000);
    let connect = thread::spawn({
        let path = endpoint.path.clone();
        move || interprocess::local_socket::Stream::connect(local_socket_name(&path).unwrap())
    });
    listener.accept().expect("accept");
    connect
        .join()
        .unwrap()
        .expect("connect to successor socket");
}

/// DD-204: each build registers its own v2 definition, pinned to its image.
#[test]
fn two_builds_register_separate_v2_definitions_on_their_images() {
    use prost::Message as _;
    use running_process::broker::protocol::BrokerIsolation;
    use running_process::broker::protocol_v2::ServiceDefinition;

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let defs = tmp.path().join("defs");
    std::fs::create_dir_all(&defs).unwrap();
    let mut paths = Vec::new();
    for body in ["build one", "build two"] {
        let exe = tmp.path().join(body.replace(' ', "-")).join("clud");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, body).unwrap();
        let build = crate::version_dir::place(&home, &exe).unwrap();
        let path = install_service_definition_v2_in(&defs, &build).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let definition = ServiceDefinition::decode(bytes.as_slice()).unwrap();
        assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
        assert_eq!(
            std::path::PathBuf::from(&definition.binary_path),
            build.image()
        );
        assert!(std::path::Path::new(&definition.binary_path)
            .starts_with(&definition.per_version_binary_dir));
        assert_eq!(definition.version_allow_list, [env!("CARGO_PKG_VERSION")]);
        paths.push((path, build));
    }
    assert_ne!(paths[0].0, paths[1].0);
    assert!(paths[0].0.exists(), "a later build must not overwrite it");

    // A collected build's definition goes; a live one and a stranger's stay.
    let stranger = defs.join("zccache.servicedef.v2");
    std::fs::write(&stranger, b"not ours").unwrap();
    std::fs::remove_dir_all(&paths[0].1.root).unwrap();
    prune_service_definitions_v2(&defs, &home);
    assert!(!paths[0].0.exists());
    assert!(paths[1].0.exists());
    assert!(stranger.exists());
}

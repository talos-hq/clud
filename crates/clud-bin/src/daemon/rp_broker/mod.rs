//! running-process broker v1 frame lane for the clud daemon.
//!
//! Consumer adoption per zackees/running-process#385 and
//! running-process `docs/INTEGRATE.md`: alongside the legacy loopback
//! TCP line wire (`server.rs`), the daemon binds a local-socket
//! endpoint (named pipe on Windows, unix socket elsewhere) served by
//! running-process's [`BackendEndpointMux`]. That endpoint answers
//! `BackendHandle` identity probes and carries clud's own
//! request/response payloads opaquely inside frozen v1 `Frame`
//! envelopes under the registered consumer payload protocol
//! [`CLUD_PAYLOAD_PROTOCOL`] (`0x7C4C`).
//!
//! Frame payloads are the exact prost messages clud already speaks on
//! its TCP wire (`proto/clud_v1.proto` `ClientToDaemon` /
//! `DaemonToClient` via `wire_prost`), so the two lanes share one
//! schema and one dispatch function
//! ([`super::server::dispatch_daemon_request`]).
//!
//! Escape hatch: `RUNNING_PROCESS_DISABLE=1` skips this lane entirely —
//! no endpoint, no identity sidecar — restoring pre-adoption behavior
//! exactly. The lane is additionally best-effort: any bind/identity
//! failure logs one note and leaves the TCP wire authoritative.

use std::io;
use std::path::Path;

use running_process::broker::adopt::BrokerSession;
use running_process::broker::backend_sdk::read_daemon_identity_file;
use running_process::broker::client::ConnectBackendRequest;
use running_process::broker::doctor::default_broker_endpoint;

use super::types::{DaemonRequest, DaemonResponse};
use super::wire_prost::{WireFrame, CLUD_PROST_PAYLOAD_PROTOCOL};

mod endpoint;
#[cfg(test)]
pub(in crate::daemon) use endpoint::endpoint_for_state_dir;
mod errors;
mod frame_lane;
mod payload;
#[cfg(test)]
mod tests;
mod wire_mode;

pub(in crate::daemon) use endpoint::daemon_identity_path;
use errors::log_adopt_miss;
pub(super) use frame_lane::spawn_frame_lane;
#[cfg(test)]
pub(super) use frame_lane::{
    install_service_definition, install_service_definition_v2_in, prune_service_definitions_v2,
    publish_cache_manifest,
};
use wire_mode::running_process_disabled;
pub(super) use wire_mode::WireMode;

running_process::register_payload_protocol! {
    /// clud daemon's opaque Frame v1 request/response lane.
    ///
    /// Registered upstream in
    /// `running-process/src/broker/protocol/registry.rs` (consumer
    /// range `0x7000..=0x7EFF`; pairwise-distinct from zccache's
    /// `0x7A63`). FROZEN — never change this value.
    pub(crate) const CLUD_PAYLOAD_PROTOCOL: u32 = 0x7C4C;
}

/// Canonical running-process escape hatch. `=1` (exact) restores
/// pre-adoption behavior: no probe endpoint, no identity sidecar, no
/// frame lane.
pub(super) const RUNNING_PROCESS_DISABLE_ENV: &str = "RUNNING_PROCESS_DISABLE";

/// Logical service name clud registers and probes under.
pub(super) const RUNNING_PROCESS_SERVICE_NAME: &str = "clud";

/// Minimum clud daemon version the broker may negotiate for this service
/// (consumer-adoption guide step 8; zackees/running-process#436). Stamped
/// onto the `ServiceDefinition` so the broker refuses pre-2.0.0 backends
/// with `RefusalKind::VersionUnsupported`.
pub(super) const RUNNING_PROCESS_MIN_VERSION: &str = "2.0.0";

/// Encode a [`DaemonRequest`] as clud frame-lane payload bytes (prost
/// `ClientToDaemon`). Client-side helper for `FrameClient::request`.
pub(super) fn encode_frame_lane_request(
    request: &DaemonRequest,
    envelope_request_id: impl Into<String>,
) -> io::Result<Vec<u8>> {
    super::wire_prost::encode_daemon_request_prost(request, envelope_request_id)
        .map(|wire_frame| wire_frame.payload)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Decode clud frame-lane response payload bytes (prost
/// `DaemonToClient`). Client-side helper for `FrameClient::request`.
pub(super) fn decode_frame_lane_response(payload: &[u8]) -> io::Result<DaemonResponse> {
    super::wire_prost::decode_daemon_response(&WireFrame {
        payload_protocol: CLUD_PROST_PAYLOAD_PROTOCOL,
        payload: payload.to_vec(),
    })
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// One request/response round trip over the running-process frame lane
/// (`WireMode::ProstV1`).
///
/// Client-side fast path for `send_daemon_request`: selects the wire mode
/// ([`WireMode::select`]), reads the daemon identity sidecar, and adopts
/// the broker session through [`BrokerSession::adopt`] (#436 /
/// consumer-adoption-clud.md step 6). Adoption performs the Hello
/// handshake (`service_name = "clud"`, protocol min/max = 1,
/// `client_lib_name = "running-process"`, `wanted_version` = clud daemon
/// version) when a real broker is reached, and Hello-skips straight to
/// the sidecar's backend endpoint otherwise (`cached_backend_endpoint`
/// is set and `wanted_version == self_version` by construction). It then
/// exchanges one [`CLUD_PAYLOAD_PROTOCOL`] frame.
///
/// Returns `None` on ANY miss — `WireMode::JsonLegacy` (disable hatch),
/// missing sidecar, broker refusal, connect/encode/decode failure — so
/// the caller falls back to the legacy TCP wire, which stays the
/// authoritative path. Broker refusals are classified through
/// [`RefusalKind`] before degrading so the cause is logged, not
/// swallowed silently.
pub(super) fn try_send_via_frame_lane(
    state_dir: &Path,
    request: &DaemonRequest,
) -> Option<DaemonResponse> {
    if WireMode::select() == WireMode::JsonLegacy {
        return None;
    }
    let identity = read_daemon_identity_file(&daemon_identity_path(state_dir))?;
    let version = env!("CARGO_PKG_VERSION");
    // A broker endpoint is required by the request shape but only ever
    // dialed when the Hello-skip connect misses; an underivable default
    // is fine to substitute with a never-listening name.
    let broker_endpoint =
        default_broker_endpoint().unwrap_or_else(|_| String::from("clud-rp-no-broker"));
    let mut connect = ConnectBackendRequest::new(
        &broker_endpoint,
        RUNNING_PROCESS_SERVICE_NAME,
        version,
        version,
    );
    connect.client_lib_name = "running-process";
    connect.cached_backend_endpoint = Some(&identity.ipc_endpoint.path);

    let mut session = match BrokerSession::adopt(connect) {
        Ok(session) => session,
        Err(err) => {
            log_adopt_miss(&err);
            return None;
        }
    };
    let payload = encode_frame_lane_request(request, format!("cli-{}", std::process::id())).ok()?;
    let reply = session.request(CLUD_PAYLOAD_PROTOCOL, payload).ok()?;
    decode_frame_lane_response(&reply.payload).ok()
}

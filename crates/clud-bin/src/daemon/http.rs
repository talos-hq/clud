//! Issue #183: in-process HTTP dashboard.
//!
//! Binds a second loopback `tiny_http::Server` alongside the IPC TCP
//! listener (`daemon/server.rs`). Serves three routes:
//!
//! - `GET /` / `GET /index.html` — the embedded single-page dashboard.
//! - `GET /state.json` — one consolidated JSON document with daemon meta,
//!   live sessions, GC tracked entries, repo visits, and aggregate stats.
//! - `POST /proc/reap` — body `{pid, start_time}`; kills a sampled process
//!   subtree from the Processes tab (#340).
//! - `POST /gc/purge` — body `{id?, kind?}`; delegates to the existing
//!   `GcOp::Purge` IPC op and returns `{removed, skipped}`.
//!
//! Loopback-only with a per-daemon capability: browser requests must target
//! an allowed Host and present the capability established by `clud ui`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tiny_http::{Method, Request, Server};

use crate::dashboard_auth::{DashboardAccess, COOKIE_NAME};

use super::activity::DaemonActivity;
use super::gc_service::{GcRequestMsg, RegistryMsg, WORKER_REPLY_TIMEOUT};
use super::io_helpers::read_json_file;
use super::paths::{daemon_info_path, sessions_dir};
use super::process_utils::{identity_is_alive, pid_is_alive};
use super::runtime_config::TestRuntimeActivity;
use super::types::{
    CtrlCProfile, DaemonInfo, GcOp, GcReply, ListRow, RepoVisit, SessionKind, SessionSnapshot,
};
use crate::ctrl_c_track::{self, CtrlCEvent};
use crate::launch_log::{self, LaunchRecord};
use crate::session_registry::LiveSession;

#[path = "http_response.rs"]
pub(crate) mod http_response;
use http_response::{
    find_body_start, json_error_bytes, read_body, respond_capability_bootstrap, respond_html,
    respond_json, respond_text,
};

/// Supplier of live session-registry rows. Injected at the dashboard
/// boundary so production wires in the redb-backed reader while unit
/// tests pass a no-op stub. This avoids env-var coupling between
/// parallel tests in `daemon::http::tests` (issue #190 follow-up: the
/// initial implementation that read `CLUD_SESSION_DB` directly inside
/// `build_dashboard_state` raced with `build_state_with_empty_state_dir_returns_zeros`
/// on macOS x86 CI).
pub(super) type LiveSessionsProvider =
    std::sync::Arc<dyn Fn() -> Vec<LiveSession> + Send + Sync + 'static>;

/// Test-only public entry point: spawn the dashboard HTTP listener for
/// telemetry-only scenarios (no GC backend). Integration tests under
/// `tests/integration/diagnostics/telemetry_endpoint.rs` use this to wire up the server without
/// taking on the `gc_service::RegistryMsg` type that the full
/// `spawn_dashboard` signature otherwise leaks.
pub fn spawn_dashboard_telemetry_only(
    state_dir: PathBuf,
    ipc_port: u16,
    started_at_unix: i64,
    telemetry: TelemetryStore,
    dashboard_token: String,
) -> Option<u16> {
    let live_provider: LiveSessionsProvider = std::sync::Arc::new(Vec::new);
    let tool_telemetry = ToolTelemetryStore::new();
    spawn_dashboard(
        state_dir,
        None,
        ipc_port,
        started_at_unix,
        live_provider,
        telemetry,
        tool_telemetry,
        dashboard_token,
    )
}

/// Production provider: reads the redb session registry under the
/// cross-process advisory lock. Errors are swallowed so a registry
/// hiccup never blanks the dashboard for sessions that *do* have
/// JSON snapshots.
pub(super) fn default_live_sessions_provider() -> LiveSessionsProvider {
    std::sync::Arc::new(|| {
        crate::session_registry::list_live_sessions_under_lock().unwrap_or_default()
    })
}

/// Bundled single-page dashboard. Vanilla JS, no build step. Polls
/// `/state.json` every 5s and renders the three tabs (Sessions / GC /
/// Repos) plus per-row and per-kind purge controls.
const DASHBOARD_HTML: &str = include_str!("../../assets/dashboard/index.html");

/// Hard cap on a POST request body so a misbehaving client can't OOM the
/// daemon. The purge payload is two short JSON fields; 16 KiB is generous.
const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024;

/// Issue #469 (beta): per-PID cap on telemetry entries. A runaway logger
/// can't grow this past N — oldest entries get dropped first.
const TELEMETRY_PER_PID_CAP: usize = 500;
const TOOL_TELEMETRY_CAP: usize = 1000;

/// Issue #469 — one telemetry record submitted by `clud log`. Mirrors
/// `log_event::TelemetryPayload` plus the daemon-added receive timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryEntry {
    pub parent_pid: u32,
    pub time_ms: u64,
    pub received_at_ms: u64,
    pub cmd: String,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
}

/// `POST /telemetry/log` body — same shape as the entry minus the
/// server-side `received_at_ms` timestamp (daemon assigns it).
#[derive(Debug, Clone, Deserialize)]
pub struct TelemetryIngest {
    pub parent_pid: u32,
    pub time_ms: u64,
    pub cmd: String,
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Compact per-PID view returned inside `/state.json` — totals only, so
/// the polled summary stays bounded regardless of entry count. The
/// per-entry detail (with envs) lives behind `/telemetry/by-pid/<pid>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryPidSummary {
    pub parent_pid: u32,
    pub entry_count: usize,
    pub last_at_ms: u64,
}

/// Full per-PID payload returned by `GET /telemetry/by-pid/<pid>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryPidDetail {
    pub parent_pid: u32,
    pub entries: Vec<TelemetryEntry>,
}

/// In-memory telemetry sink shared between the HTTP listener and any
/// other daemon component that wants to read it. Lifetime = daemon
/// lifetime; restart wipes it (persistence is a follow-up).
#[derive(Debug, Default, Clone)]
pub struct TelemetryStore {
    inner: Arc<Mutex<TelemetryStoreInner>>,
}

#[derive(Debug, Default)]
struct TelemetryStoreInner {
    by_pid: HashMap<u32, VecDeque<TelemetryEntry>>,
}

impl TelemetryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one entry. Trims the per-PID ring buffer to `TELEMETRY_PER_PID_CAP`
    /// (drop-oldest).
    pub fn push(&self, entry: TelemetryEntry) {
        let mut guard = self.inner.lock().expect("telemetry store poisoned");
        let dq = guard.by_pid.entry(entry.parent_pid).or_default();
        dq.push_back(entry);
        while dq.len() > TELEMETRY_PER_PID_CAP {
            dq.pop_front();
        }
    }

    /// Per-PID summary keyed by parent_pid, sorted by last activity desc.
    pub fn summary(&self) -> Vec<TelemetryPidSummary> {
        let guard = self.inner.lock().expect("telemetry store poisoned");
        let mut rows: Vec<_> = guard
            .by_pid
            .iter()
            .map(|(pid, dq)| {
                let last_at_ms = dq.back().map(|e| e.received_at_ms).unwrap_or(0);
                TelemetryPidSummary {
                    parent_pid: *pid,
                    entry_count: dq.len(),
                    last_at_ms,
                }
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.last_at_ms));
        rows
    }

    /// Full per-PID detail or `None` if the PID has no entries.
    pub fn detail(&self, pid: u32) -> Option<TelemetryPidDetail> {
        let guard = self.inner.lock().expect("telemetry store poisoned");
        guard.by_pid.get(&pid).map(|dq| TelemetryPidDetail {
            parent_pid: pid,
            entries: dq.iter().cloned().collect(),
        })
    }
}

/// One `clud tool` invocation reported by the lightweight launcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallEntry {
    pub id: String,
    pub name: String,
    pub start_time_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolEventIngest {
    pub event: String,
    pub id: String,
    pub name: String,
    pub start_time_ms: u64,
    #[serde(default)]
    pub end_time_ms: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stderr_tail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAggregateBucket {
    pub label: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub total: usize,
    pub success: usize,
    pub failed: usize,
    pub running: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTelemetryView {
    pub entries: Vec<ToolCallEntry>,
    pub aggregate: Vec<ToolAggregateBucket>,
}

#[derive(Debug, Default, Clone)]
pub struct ToolTelemetryStore {
    inner: Arc<Mutex<ToolTelemetryStoreInner>>,
}

#[derive(Debug, Default)]
struct ToolTelemetryStoreInner {
    entries: VecDeque<ToolCallEntry>,
}

#[derive(Debug, Clone)]
struct DashboardTelemetryStores {
    telemetry: TelemetryStore,
    tool_telemetry: ToolTelemetryStore,
}

impl ToolTelemetryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_event(&self, event: ToolEventIngest) {
        let mut guard = self.inner.lock().expect("tool telemetry store poisoned");
        match event.event.as_str() {
            "start" => {
                if let Some(existing) = guard.entries.iter_mut().find(|entry| entry.id == event.id)
                {
                    existing.name = event.name;
                    existing.start_time_ms = event.start_time_ms;
                    return;
                }
                guard.entries.push_back(ToolCallEntry {
                    id: event.id,
                    name: event.name,
                    start_time_ms: event.start_time_ms,
                    end_time_ms: None,
                    exit_code: None,
                    stderr_tail: None,
                });
                while guard.entries.len() > TOOL_TELEMETRY_CAP {
                    guard.entries.pop_front();
                }
            }
            "finish" => {
                if let Some(existing) = guard.entries.iter_mut().find(|entry| entry.id == event.id)
                {
                    existing.name = event.name;
                    existing.start_time_ms = event.start_time_ms;
                    existing.end_time_ms = event.end_time_ms;
                    existing.exit_code = event.exit_code;
                    existing.stderr_tail = event.stderr_tail;
                } else {
                    guard.entries.push_back(ToolCallEntry {
                        id: event.id,
                        name: event.name,
                        start_time_ms: event.start_time_ms,
                        end_time_ms: event.end_time_ms,
                        exit_code: event.exit_code,
                        stderr_tail: event.stderr_tail,
                    });
                }
                while guard.entries.len() > TOOL_TELEMETRY_CAP {
                    guard.entries.pop_front();
                }
            }
            _ => {}
        }
    }

    pub fn view(&self) -> ToolTelemetryView {
        self.view_at(current_unix_millis())
    }

    fn view_at(&self, now_ms: u64) -> ToolTelemetryView {
        let guard = self.inner.lock().expect("tool telemetry store poisoned");
        let mut entries: Vec<_> = guard.entries.iter().cloned().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.start_time_ms));
        ToolTelemetryView {
            aggregate: tool_aggregate_at(&entries, now_ms),
            entries,
        }
    }
}

fn tool_aggregate_at(entries: &[ToolCallEntry], now_ms: u64) -> Vec<ToolAggregateBucket> {
    let mut buckets = Vec::new();
    push_tool_bucket(
        &mut buckets,
        "last 10s",
        now_ms.saturating_sub(10_000),
        now_ms,
    );
    push_tool_bucket(
        &mut buckets,
        "10-20s",
        now_ms.saturating_sub(20_000),
        now_ms.saturating_sub(10_000),
    );
    push_tool_bucket(
        &mut buckets,
        "20-30s",
        now_ms.saturating_sub(30_000),
        now_ms.saturating_sub(20_000),
    );
    for minute in 1..=10 {
        let end_ms = now_ms.saturating_sub(30_000 + ((minute - 1) * 60_000));
        let start_ms = end_ms.saturating_sub(60_000);
        push_tool_bucket(&mut buckets, &format!("{minute}m"), start_ms, end_ms);
    }

    for entry in entries {
        if entry.start_time_ms < now_ms.saturating_sub(10 * 60_000) || entry.start_time_ms > now_ms
        {
            continue;
        }
        if let Some(bucket) = buckets.iter_mut().find(|bucket| {
            entry.start_time_ms >= bucket.start_ms && entry.start_time_ms < bucket.end_ms
        }) {
            bucket.total += 1;
            match entry.exit_code {
                Some(0) => bucket.success += 1,
                Some(_) => bucket.failed += 1,
                None => bucket.running += 1,
            }
        }
    }
    buckets
}

fn push_tool_bucket(
    buckets: &mut Vec<ToolAggregateBucket>,
    label: &str,
    start_ms: u64,
    end_ms: u64,
) {
    buckets.push(ToolAggregateBucket {
        label: label.to_string(),
        start_ms,
        end_ms,
        total: 0,
        success: 0,
        failed: 0,
        running: 0,
    });
}

/// Aggregate document returned by `GET /state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardState {
    pub daemon: DaemonStateView,
    pub sessions: Vec<SessionView>,
    pub gc: Vec<ListRow>,
    pub repos: Vec<RepoVisit>,
    /// Recent cross-path Ctrl+C exit events. Each entry is one CLI
    /// process that observed Ctrl+C and recorded the elapsed wall-clock
    /// time from observation to process-exit. Capped at
    /// [`ctrl_c_track::DASHBOARD_EVENT_LIMIT`], newest first.
    #[serde(default)]
    pub ctrl_c_events: Vec<CtrlCEvent>,
    pub stats: Stats,
    /// Cached daemon process sample, consumed by the Processes dashboard tab.
    #[serde(default)]
    pub process_tree: serde_json::Value,
}

/// Meta about the daemon serving this dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStateView {
    pub pid: u32,
    pub ipc_port: u16,
    pub dashboard_port: Option<u16>,
    pub started_at_unix: i64,
    pub now_unix: i64,
    pub uptime_secs: u64,
    pub version: String,
}

/// Public-safe projection of `SessionSnapshot` — drops the *_pid fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub backend: Option<String>,
    pub launch_mode: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub repo_root: Option<String>,
    pub command: Vec<String>,
    pub clud_argv: Vec<String>,
    pub clud_pid: Option<u32>,
    pub created_at: Option<u64>,
    pub exited_at: Option<u64>,
    pub duration_ms: Option<u64>,
    pub detachable: bool,
    pub background: bool,
    pub attachable: bool,
    pub repeat_interval_secs: Option<u64>,
    pub repeat_next_run_at: Option<u64>,
    pub repeat_running: bool,
    pub exit_code: Option<i32>,
    /// Mirrors `LaunchRecord::failure_reason` (#998): why clud itself ended the
    /// launch, so the dashboard can say more than `exit 1`. Always `None` for
    /// daemon-hosted sessions, which do not carry one.
    #[serde(default)]
    pub failure_reason: Option<String>,
    pub worker_port: u16,
    pub live: bool,
    pub ctrl_c: Option<CtrlCProfileView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtrlCProfileView {
    pub cli_pid: Option<u32>,
    pub cli_observed_at_ms: Option<u64>,
    pub cli_handoff_at_ms: Option<u64>,
    pub cli_return_ready_at_ms: Option<u64>,
    pub cli_handoff_ms: Option<u64>,
    pub daemon_received_at_ms: Option<u64>,
    pub daemon_kill_started_at_ms: Option<u64>,
    pub daemon_kill_finished_at_ms: Option<u64>,
    pub daemon_kill_ms: Option<u64>,
    pub fast_path: bool,
}

/// Counts derived from the rest of the document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub session_count: usize,
    pub live_session_count: usize,
    pub gc_count: usize,
    pub gc_by_kind: HashMap<String, usize>,
    pub repo_count: usize,
}

/// Body of `POST /gc/purge`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PurgeRequest {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Response body of `POST /gc/purge`. The synchronous per-row delete
/// (`{id: N}`) populates `removed`; the bulk async purge (no `id`)
/// populates `dispatched`. `skipped` is always the count of candidates
/// the worker filtered out as live or non-purgeable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PurgeResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<usize>,
    /// Issue #268: tasks handed to the parallel purge pool. The
    /// matching filesystem removals and redb row deletes happen
    /// asynchronously; poll `/state.json` to watch counts drop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched: Option<usize>,
    pub skipped: usize,
}

/// Spawn the dashboard's HTTP listener in a background thread.
/// Returns the bound port (or `None` if the listener could not be brought
/// up — logged once and the daemon continues without a dashboard).
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_dashboard(
    state_dir: PathBuf,
    gc_tx: Option<mpsc::Sender<RegistryMsg>>,
    ipc_port: u16,
    started_at_unix: i64,
    live_sessions_provider: LiveSessionsProvider,
    telemetry: TelemetryStore,
    tool_telemetry: ToolTelemetryStore,
    dashboard_token: String,
) -> Option<u16> {
    spawn_dashboard_with_activity(
        state_dir,
        gc_tx,
        ipc_port,
        started_at_unix,
        live_sessions_provider,
        telemetry,
        tool_telemetry,
        dashboard_token,
        crate::dashboard_auth::generate_token(),
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_dashboard_with_activity(
    state_dir: PathBuf,
    gc_tx: Option<mpsc::Sender<RegistryMsg>>,
    ipc_port: u16,
    started_at_unix: i64,
    live_sessions_provider: LiveSessionsProvider,
    telemetry: TelemetryStore,
    tool_telemetry: ToolTelemetryStore,
    dashboard_token: String,
    api_token: String,
    test_activity: Option<TestRuntimeActivity>,
    activity: Option<DaemonActivity>,
) -> Option<u16> {
    let api_lifecycle = Arc::new(super::api_session_lifecycle::ApiSessionLifecycle::new(
        super::api_sessions::ApiSessionStore::new(state_dir.clone()),
    ));
    spawn_dashboard_with_activity_and_lifecycle(
        state_dir,
        gc_tx,
        ipc_port,
        started_at_unix,
        live_sessions_provider,
        telemetry,
        tool_telemetry,
        dashboard_token,
        api_token,
        test_activity,
        activity,
        api_lifecycle,
    )
}

/// Production passes the same manager to both IPC transports and HTTP. Test
/// callers use `spawn_dashboard_with_activity`, which owns an isolated one.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_dashboard_with_activity_and_lifecycle(
    state_dir: PathBuf,
    gc_tx: Option<mpsc::Sender<RegistryMsg>>,
    ipc_port: u16,
    started_at_unix: i64,
    live_sessions_provider: LiveSessionsProvider,
    telemetry: TelemetryStore,
    tool_telemetry: ToolTelemetryStore,
    dashboard_token: String,
    api_token: String,
    test_activity: Option<TestRuntimeActivity>,
    activity: Option<DaemonActivity>,
    api_lifecycle: Arc<super::api_session_lifecycle::ApiSessionLifecycle>,
) -> Option<u16> {
    let server = match Server::http("127.0.0.1:0") {
        Ok(s) => s,
        Err(err) => {
            eprintln!("[clud] note: dashboard listener failed to bind: {err}");
            return None;
        }
    };
    let port = match server.server_addr().to_ip() {
        Some(addr) => addr.port(),
        None => {
            eprintln!("[clud] note: dashboard listener has no IPv4 address");
            return None;
        }
    };
    let res = thread::Builder::new()
        .name("clud-dashboard-http".to_string())
        .spawn(move || {
            let access = DashboardAccess::new(dashboard_token);
            run_dashboard_loop(
                server,
                port,
                access,
                api_token,
                state_dir,
                gc_tx,
                ipc_port,
                started_at_unix,
                live_sessions_provider,
                DashboardTelemetryStores {
                    telemetry,
                    tool_telemetry,
                },
                test_activity,
                activity,
                api_lifecycle,
            )
        });
    match res {
        Ok(_) => Some(port),
        Err(err) => {
            eprintln!("[clud] note: dashboard thread spawn failed: {err}");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn run_dashboard_loop(
    server: Server,
    port: u16,
    access: DashboardAccess,
    api_token: String,
    state_dir: PathBuf,
    gc_tx: Option<mpsc::Sender<RegistryMsg>>,
    ipc_port: u16,
    started_at_unix: i64,
    live_sessions_provider: LiveSessionsProvider,
    stores: DashboardTelemetryStores,
    test_activity: Option<TestRuntimeActivity>,
    activity: Option<DaemonActivity>,
    api_lifecycle: Arc<super::api_session_lifecycle::ApiSessionLifecycle>,
) {
    // #1743: the session `gh` read broker. Its store opens on first use.
    let gh_broker = Arc::new(crate::gh_broker::service::GhBroker::for_state_dir(
        &state_dir,
    ));
    for request in server.incoming_requests() {
        let _connection_guard = activity.as_ref().map(DaemonActivity::start_connection);
        let _activity_guard = test_activity
            .as_ref()
            .map(TestRuntimeActivity::start_request);
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or(&url).to_string();
        let host = request_header(&request, "Host");
        if path.starts_with("/v1/") {
            let authorized = access.allows_host(host.as_deref(), port)
                && crate::dashboard_auth::allows_bearer(
                    &api_token,
                    request_header(&request, "Authorization").as_deref(),
                );
            if !authorized {
                respond_json(
                    request,
                    401,
                    br#"{"code":"unauthorized","message":"bearer authentication required"}"#,
                );
                continue;
            }
            match (method.clone(), path.as_str()) {
                (Method::Get, "/v1/health") => {
                    respond_json(request, 200, br#"{"status":"ok","api_version":"v1"}"#)
                }
                (Method::Get, "/v1/openapi.json") => respond_json(
                    request,
                    200,
                    &serde_json::to_vec(&openapi_document()).unwrap_or_else(|_| b"{}".to_vec()),
                ),
                _ => super::api_session_http::handle(
                    request,
                    method,
                    &path,
                    state_dir.clone(),
                    &api_lifecycle,
                ),
            }
            continue;
        }
        let cookie = request_header(&request, "Cookie");
        let query_token = query_parameter(&url, "token");
        if !access.allows_host(host.as_deref(), port)
            || !access.allows_token(query_token.as_deref(), cookie.as_deref())
        {
            respond_json(
                request,
                403,
                json_error_bytes("dashboard capability required").as_slice(),
            );
            continue;
        }
        if query_token.as_deref() == Some(access.token()) {
            respond_capability_bootstrap(request, &access, &path);
            continue;
        }
        // Telemetry detail route — `/telemetry/by-pid/<u32>`. Matched
        // first so the catch-all SPA fallback below never claims it.
        if method == Method::Get {
            if let Some(rest) = path.strip_prefix("/telemetry/by-pid/") {
                handle_telemetry_detail(request, rest, &stores.telemetry);
                continue;
            }
        }
        match (method, path.as_str()) {
            (Method::Get, "/state.json") => {
                handle_state(
                    request,
                    &state_dir,
                    gc_tx.as_ref(),
                    ipc_port,
                    started_at_unix,
                    live_sessions_provider.as_ref(),
                );
            }
            // Issue #471: telemetry summary lives at its own URL now
            // (was previously bundled into `/state.json#telemetry`).
            (Method::Get, "/telemetry") => {
                handle_telemetry_summary(request, &stores.telemetry);
            }
            (Method::Get, "/tools") => {
                handle_tools_summary(request, &stores.tool_telemetry);
            }
            (Method::Post, "/gc/purge") => {
                handle_purge(request, gc_tx.as_ref());
            }
            (Method::Post, "/proc/reap") => {
                http_proc_reap::handle_reap(request, &state_dir);
            }
            (Method::Post, "/telemetry/log") => {
                handle_telemetry_log(request, &stores.telemetry);
            }
            (Method::Post, "/tools/event") => {
                handle_tool_event(request, &stores.tool_telemetry);
            }
            (Method::Post, crate::gh_broker::READ_PATH) => {
                let guard = activity.as_ref().map(DaemonActivity::start_connection);
                spawn_gh_route(request, Arc::clone(&gh_broker), guard, GhRoute::Read);
            }
            (Method::Post, crate::gh_broker::WATCH_PATH) => {
                let guard = activity.as_ref().map(DaemonActivity::start_connection);
                spawn_gh_route(request, Arc::clone(&gh_broker), guard, GhRoute::Watch);
            }
            (Method::Get, crate::gh_broker::LEDGER_PATH) => {
                let (status, bytes) = gh_broker.handle_ledger();
                respond_json(request, status, &bytes);
            }
            (Method::Post, crate::gh_broker::INVALIDATE_PATH) => {
                let mut request = request;
                // An unreadable body still invalidates: globally.
                let body = read_body(&mut request).unwrap_or_default();
                let (status, bytes) = gh_broker.handle_invalidate(&body);
                respond_json(request, status, &bytes);
            }
            // Any other GET is an SPA route — serve the dashboard so the
            // History-API router takes over (refresh + deep-links).
            (Method::Get, _) => {
                respond_html(request, 200, DASHBOARD_HTML.as_bytes());
            }
            _ => {
                respond_text(request, 404, b"not found");
            }
        }
    }
}

const OPENAPI_JSON: &str = r##"{"openapi":"3.1.0","info":{"title":"clud daemon API","version":"v1"},"paths":{"/v1/health":{"get":{"responses":{"200":{"description":"healthy"},"401":{"$ref":"#/components/responses/Error"}}}},"/v1/openapi.json":{"get":{"responses":{"200":{"description":"schema"},"401":{"$ref":"#/components/responses/Error"}}}},"/v1/sessions":{"get":{"responses":{"200":{"description":"logical sessions","content":{"application/json":{"schema":{"type":"array","items":{"$ref":"#/components/schemas/Session"}}}}},"401":{"$ref":"#/components/responses/Error"}}},"post":{"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/CreateSession"}}}},"responses":{"201":{"description":"created","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Session"}}}},"400":{"$ref":"#/components/responses/Error"},"401":{"$ref":"#/components/responses/Error"},"409":{"$ref":"#/components/responses/Error"}}}},"/v1/sessions/{id}":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string"}}],"get":{"responses":{"200":{"description":"session","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Session"}}}},"404":{"$ref":"#/components/responses/Error"}}},"delete":{"responses":{"200":{"description":"terminated","content":{"application/json":{"schema":{"$ref":"#/components/schemas/TerminalResponse"}}}},"404":{"$ref":"#/components/responses/Error"},"409":{"$ref":"#/components/responses/Error"}}}},"/v1/sessions/{id}/interrupt":{"post":{"responses":{"200":{"description":"interrupted","content":{"application/json":{"schema":{"$ref":"#/components/schemas/InterruptResponse"}}}},"404":{"$ref":"#/components/responses/Error"},"409":{"$ref":"#/components/responses/Error"}}}},"/v1/sessions/{id}/turns":{"post":{"parameters":[{"name":"Idempotency-Key","in":"header","required":false,"schema":{"type":"string"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/TurnRequest"}}}},"responses":{"200":{"description":"idempotent replay","content":{"application/json":{"schema":{"$ref":"#/components/schemas/TurnResponse"}}}},"202":{"description":"started","content":{"application/json":{"schema":{"$ref":"#/components/schemas/TurnResponse"}}}},"400":{"$ref":"#/components/responses/Error"},"404":{"$ref":"#/components/responses/Error"},"409":{"$ref":"#/components/responses/Error"},"422":{"$ref":"#/components/responses/Error"}}}},"/v1/sessions/{id}/events":{"get":{"parameters":[{"name":"after","in":"query","schema":{"type":"integer","minimum":0}},{"name":"limit","in":"query","schema":{"type":"integer","minimum":1,"maximum":128}}],"responses":{"200":{"description":"bounded cursor events","content":{"application/json":{"schema":{"$ref":"#/components/schemas/EventsResponse"}}}},"400":{"$ref":"#/components/responses/Error"},"404":{"$ref":"#/components/responses/Error"}}}}},"components":{"schemas":{"CreateSession":{"type":"object","required":["backend","cwd"],"properties":{"backend":{"enum":["claude","codex"]},"cwd":{"type":"string","description":"existing directory; stored as canonical absolute path"},"name":{"type":"string"},"model":{"type":"string"},"safe":{"type":"boolean"}}},"Session":{"type":"object","required":["id","backend","cwd","state","generation"]},"TurnRequest":{"type":"object","required":["message"],"properties":{"message":{"type":"string","minLength":1,"maxLength":65536},"interrupt_running":{"type":"boolean"}}},"TurnResponse":{"type":"object","required":["session_id","turn_id","status"]},"InterruptResponse":{"type":"object","required":["status","forced"]},"TerminalResponse":{"type":"object","required":["status"]},"EventsResponse":{"type":"object","required":["events","next_cursor","retention_gap"],"properties":{"events":{"type":"array","items":{"$ref":"#/components/schemas/Event"}},"next_cursor":{"type":"integer"},"retention_gap":{"type":"boolean"}}},"Event":{"type":"object","required":["cursor","at_ms","kind","data"]},"Error":{"type":"object","required":["code","message"]}},"responses":{"Error":{"description":"stable API error","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}}"##;

fn openapi_document() -> serde_json::Value {
    let mut document: serde_json::Value =
        serde_json::from_str(OPENAPI_JSON).expect("embedded OpenAPI is valid JSON");
    let id_parameter =
        serde_json::json!({"name":"id","in":"path","required":true,"schema":{"type":"string"}});
    for path in [
        "/v1/sessions/{id}/interrupt",
        "/v1/sessions/{id}/turns",
        "/v1/sessions/{id}/events",
    ] {
        document["paths"][path]["parameters"] =
            serde_json::Value::Array(vec![id_parameter.clone()]);
    }
    document["components"]["securitySchemes"] = serde_json::json!({"bearerAuth":{"type":"http","scheme":"bearer","bearerFormat":"opaque daemon capability"}});
    document["security"] = serde_json::json!([{ "bearerAuth": [] }]);
    // The embedded skeleton keeps the hand-maintained route map readable; the
    // concrete durable-record shapes live here so clients can use the same
    // DTO fields actually serialized by the HTTP handler.
    let schemas = &mut document["components"]["schemas"];
    schemas["Session"]["properties"] = serde_json::json!({
        "schema_version":{"type":"integer"}, "id":{"type":"string"},
        "backend":{"type":"string","enum":["claude","codex"]}, "cwd":{"type":"string"},
        "name":{"type":"string"}, "resolved_settings":{"type":"object"},
        "provider_session_id":{"type":"string"}, "state":{"type":"string","enum":["starting","running","interrupting","idle","failed","terminated"]},
        "generation":{"type":"integer"}, "current_turn_id":{"type":"string"},
        "created_at_ms":{"type":"integer"}, "updated_at_ms":{"type":"integer"}, "last_error":{"type":"string"},
        "turns":{"type":"array","items":{"type":"object"}}, "events":{"type":"array","items":{"$ref":"#/components/schemas/Event"}},
        "next_event_cursor":{"type":"integer"}, "idempotency":{"type":"array","items":{"type":"object"}}
    });
    schemas["Event"]["properties"] = serde_json::json!({
        "cursor":{"type":"integer"}, "at_ms":{"type":"integer"}, "turn_id":{"type":"string"},
        "kind":{"type":"string"}, "data":{}
    });
    schemas["TurnResponse"]["properties"] = serde_json::json!({
        "session_id":{"type":"string"}, "turn_id":{"type":"string"}, "generation":{"type":"integer"}, "status":{"type":"string","enum":["started","replayed","busy"]}
    });
    schemas["InterruptResponse"]["properties"] = serde_json::json!({"status":{"type":"string","enum":["interrupted"]},"forced":{"type":"boolean"}});
    schemas["Error"]["properties"] =
        serde_json::json!({"code":{"type":"string"},"message":{"type":"string"}});
    for item in document["paths"]
        .as_object_mut()
        .into_iter()
        .flat_map(|paths| paths.values_mut())
    {
        for operation in ["get", "post", "delete"] {
            if let Some(responses) = item
                .get_mut(operation)
                .and_then(|value| value.get_mut("responses"))
            {
                responses["401"] = serde_json::json!({"$ref":"#/components/responses/Error"});
            }
        }
    }
    for path in ["/v1/sessions/{id}", "/v1/sessions/{id}/events"] {
        document["paths"][path]["get"]["responses"]["409"] = serde_json::json!({"$ref":"#/components/responses/Error","description":"corrupt_session"});
    }
    document
}

// ---------- route handlers ----------

fn handle_state(
    request: Request,
    state_dir: &Path,
    gc_tx: Option<&mpsc::Sender<RegistryMsg>>,
    ipc_port: u16,
    started_at_unix: i64,
    live_sessions_provider: &(dyn Fn() -> Vec<LiveSession> + Send + Sync),
) {
    let live_sessions = live_sessions_provider();
    match build_dashboard_state(state_dir, gc_tx, ipc_port, started_at_unix, live_sessions) {
        Ok(state) => match serde_json::to_vec(&state) {
            Ok(bytes) => respond_json(request, 200, &bytes),
            Err(err) => respond_json(
                request,
                500,
                json_error_bytes(&format!("serialize state failed: {err}")).as_slice(),
            ),
        },
        Err(err) => {
            respond_json(request, 500, json_error_bytes(&err.to_string()).as_slice());
        }
    }
}

fn handle_purge(mut request: Request, gc_tx: Option<&mpsc::Sender<RegistryMsg>>) {
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(err) => {
            respond_json(
                request,
                400,
                json_error_bytes(&format!("read body failed: {err}")).as_slice(),
            );
            return;
        }
    };
    let payload: PurgeRequest = if body.is_empty() {
        PurgeRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(p) => p,
            Err(err) => {
                respond_json(
                    request,
                    400,
                    json_error_bytes(&format!("invalid JSON: {err}")).as_slice(),
                );
                return;
            }
        }
    };

    let Some(tx) = gc_tx else {
        respond_json(
            request,
            503,
            json_error_bytes("gc registry unavailable").as_slice(),
        );
        return;
    };

    // Route the request: per-row delete uses the surgical `DeleteById`
    // IPC op so the on-disk and registry-row removal target exactly the
    // requested row regardless of how many siblings share its kind. The
    // bulk per-kind / per-age path keeps using `Purge`.
    let op = match payload.id {
        Some(id) => GcOp::DeleteById { id },
        None => GcOp::Purge {
            duration: None,
            kind: payload.kind.clone(),
            dry_run: false,
        },
    };

    match send_gc_op(tx, op) {
        Ok(reply) => respond_purge_reply(request, reply),
        Err(err) => respond_json(request, 500, json_error_bytes(&err).as_slice()),
    }
}

/// The broker routes that may block: a read waits on GitHub, a watch
/// waits for a change.
#[derive(Clone, Copy)]
enum GhRoute {
    Read,
    Watch,
}

/// `/gh/watch` calls blocked at once. Each holds a thread for up to 55 s; a
/// call over the limit is refused (503) and the waiter falls back to its
/// own interval polling.
const MAX_GH_WATCHES: usize = 32;
static GH_WATCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Holds one of the [`MAX_GH_WATCHES`] slots until dropped.
struct WatchSlot;

impl WatchSlot {
    fn take() -> Option<Self> {
        use std::sync::atomic::Ordering;
        GH_WATCHES
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_GH_WATCHES).then_some(n + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for WatchSlot {
    fn drop(&mut self) {
        GH_WATCHES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// #1743: answer one `/gh/read` or `/gh/watch` request on its own thread.
/// An upstream fetch takes seconds, a watch up to a minute, and concurrent
/// readers of one key must be able to join one fetch, so the dashboard loop
/// never waits on the broker.
fn spawn_gh_route(
    mut request: Request,
    broker: Arc<crate::gh_broker::service::GhBroker>,
    guard: Option<super::activity::ActiveWorkGuard>,
    route: GhRoute,
) {
    let slot = match route {
        GhRoute::Read => None,
        GhRoute::Watch => match WatchSlot::take() {
            Some(slot) => Some(slot),
            None => {
                respond_json(request, 503, &json_error_bytes("too many gh watches"));
                return;
            }
        },
    };
    let spawned = thread::Builder::new()
        .name("clud-gh-read".to_string())
        .spawn(move || {
            let _guard = guard;
            let _slot = slot;
            let (status, bytes) = match read_body(&mut request) {
                Ok(body) => {
                    let self_exe = std::env::current_exe().unwrap_or_default();
                    let home_key = if crate::platform::host::WINDOWS {
                        "USERPROFILE"
                    } else {
                        "HOME"
                    };
                    let home = std::env::var_os(home_key).map(PathBuf::from);
                    match route {
                        GhRoute::Read => broker.handle_http(&body, &self_exe, home.as_deref()),
                        GhRoute::Watch => broker.handle_watch(&body, &self_exe, home.as_deref()),
                    }
                }
                Err(err) => (400, json_error_bytes(&format!("read body failed: {err}"))),
            };
            respond_json(request, status, &bytes);
        });
    if let Err(err) = spawned {
        eprintln!("[clud] note: gh broker thread spawn failed: {err}");
    }
}

/// Issue #471: per-PID summary list at its own URL. Returns the same
/// `Vec<TelemetryPidSummary>` shape that the bundled
/// `/state.json#telemetry` field used to carry — no behavior change
/// for the SPA's existing render code beyond the fetch destination.
fn handle_telemetry_summary(request: Request, telemetry: &TelemetryStore) {
    let summary = telemetry.summary();
    match serde_json::to_vec(&summary) {
        Ok(bytes) => respond_json(request, 200, &bytes),
        Err(err) => respond_json(
            request,
            500,
            json_error_bytes(&format!("serialize failed: {err}")).as_slice(),
        ),
    }
}

fn handle_telemetry_log(mut request: Request, telemetry: &TelemetryStore) {
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(err) => {
            respond_json(
                request,
                400,
                json_error_bytes(&format!("read body failed: {err}")).as_slice(),
            );
            return;
        }
    };
    let payload: TelemetryIngest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(err) => {
            respond_json(
                request,
                400,
                json_error_bytes(&format!("invalid JSON: {err}")).as_slice(),
            );
            return;
        }
    };
    let received_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    telemetry.push(TelemetryEntry {
        parent_pid: payload.parent_pid,
        time_ms: payload.time_ms,
        received_at_ms,
        cmd: payload.cmd,
        cwd: payload.cwd,
        env: payload.env,
    });
    respond_json(request, 200, b"{}");
}

fn handle_tools_summary(request: Request, tool_telemetry: &ToolTelemetryStore) {
    let view = tool_telemetry.view();
    match serde_json::to_vec(&view) {
        Ok(bytes) => respond_json(request, 200, &bytes),
        Err(err) => respond_json(
            request,
            500,
            json_error_bytes(&format!("serialize failed: {err}")).as_slice(),
        ),
    }
}

fn handle_tool_event(mut request: Request, tool_telemetry: &ToolTelemetryStore) {
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(err) => {
            respond_json(
                request,
                400,
                json_error_bytes(&format!("read body failed: {err}")).as_slice(),
            );
            return;
        }
    };
    let payload: ToolEventIngest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(err) => {
            respond_json(
                request,
                400,
                json_error_bytes(&format!("invalid JSON: {err}")).as_slice(),
            );
            return;
        }
    };
    tool_telemetry.push_event(payload);
    respond_json(request, 200, b"{}");
}

fn handle_telemetry_detail(request: Request, pid_str: &str, telemetry: &TelemetryStore) {
    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => {
            respond_json(
                request,
                400,
                json_error_bytes(&format!("invalid pid: {pid_str}")).as_slice(),
            );
            return;
        }
    };
    let detail = telemetry.detail(pid).unwrap_or(TelemetryPidDetail {
        parent_pid: pid,
        entries: Vec::new(),
    });
    match serde_json::to_vec(&detail) {
        Ok(bytes) => respond_json(request, 200, &bytes),
        Err(err) => respond_json(
            request,
            500,
            json_error_bytes(&format!("serialize failed: {err}")).as_slice(),
        ),
    }
}

fn respond_purge_reply(request: Request, reply: GcReply) {
    match reply {
        GcReply::PurgeOk { removed, skipped } => {
            let body = serde_json::to_vec(&PurgeResponse {
                removed: Some(removed),
                dispatched: None,
                skipped,
            })
            .unwrap_or_else(|_| b"{}".to_vec());
            respond_json(request, 200, &body);
        }
        GcReply::PurgeStarted {
            dispatched,
            skipped,
        } => {
            let body = serde_json::to_vec(&PurgeResponse {
                removed: None,
                dispatched: Some(dispatched),
                skipped,
            })
            .unwrap_or_else(|_| b"{}".to_vec());
            respond_json(request, 200, &body);
        }
        GcReply::Error { message } => {
            respond_json(request, 500, json_error_bytes(&message).as_slice());
        }
        other => {
            respond_json(
                request,
                500,
                json_error_bytes(&format!("unexpected reply: {other:?}")).as_slice(),
            );
        }
    }
}

// ---------- state aggregation ----------

#[path = "http_dashboard_state.rs"]
mod http_dashboard_state;
use http_dashboard_state::{build_dashboard_state, send_gc_op};
#[path = "http_info.rs"]
mod http_info;
#[path = "http_proc_reap.rs"]
mod http_proc_reap;
pub use http_info::{read_api_info, read_dashboard_info, read_dashboard_port};

/// Public view of `daemon.json` used by the `clud ui` CLI.
#[derive(Debug, Clone)]
pub struct DashboardInfo {
    pub pid: u32,
    pub ipc_port: u16,
    pub dashboard_port: Option<u16>,
    pub dashboard_token: Option<String>,
}

pub fn dashboard_url_from_info(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/?token={token}")
}

/// Extract a request header without retaining a reference to `Request`.
fn request_header(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_string())
}

fn query_parameter(url: &str, name: &str) -> Option<String> {
    url.split_once('?')?.1.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value.to_string())
    })
}

/// Fetch `/state.json` from the running dashboard. Used by `clud ui --json`.
pub fn fetch_state_json(port: u16, token: &str) -> io::Result<String> {
    use std::io::Write;
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let req = format!(
        "GET /state.json HTTP/1.0\r\nHost: localhost:{port}\r\nCookie: {COOKIE_NAME}={token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;
    let mut buf = Vec::with_capacity(4096);
    stream.read_to_end(&mut buf)?;
    // Split off the HTTP headers; we only return the body.
    let body_start = find_body_start(&buf).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "dashboard response had no headers terminator",
        )
    })?;
    let body = &buf[body_start..];
    String::from_utf8(body.to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

fn current_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;

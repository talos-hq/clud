//! Captured JSONL execution for one durable API-session generation.
//!
//! This is intentionally not an HTTP handler or lifecycle-control state
//! machine. It only launches an already canonical headless `LaunchPlan`,
//! drains it independently of consumers, and seals durable turn metadata.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use running_process::{NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode};
use serde_json::{json, Value};

use crate::command::LaunchPlan;
use crate::process_identity::ProcessIdentity;
use crate::win_creation_flags::invisible_helper_creationflags;

use super::api_sessions::{
    ApiSessionBackend, ApiSessionStore, ApiSessionStoreError, ApiTurnRecord, ApiTurnState,
};
use super::headless_adapter::{parse_backend_event, BackendEvent};
use super::paths::api_turn_log_path;
use super::types::unix_millis_now;

const RAW_LOG_MAX_BYTES: u64 = 1024 * 1024;
const EVENT_LINE_MAX_BYTES: usize = 8 * 1024;

/// Captured API turns do not post dashboard telemetry. Never pass the
/// dashboard capability to a subprocess whose complete output is
/// persisted in an API-readable raw log.
///
/// `client_env` is the creating client's environment, layered over the
/// daemon's by `io_helpers::child_env_from` (#933/#1157). An empty slice
/// is the documented fallback and is what every call site passes today:
/// an API session is created over HTTP and its durable record carries no
/// environment, so there is nothing to forward yet. #933 owns adding the
/// field; until then this is byte-for-byte the previous behaviour.
/// Read by the MCP bridge's `board_*` tools (docs/architecture/agent-board.md).
const BOARD_ID_ENV: &str = "CLUD_BOARD_ID";

fn api_turn_env(client_env: &[(String, String)]) -> Vec<(String, String)> {
    super::io_helpers::child_env_from(client_env)
        .into_iter()
        .filter(|(key, _)| {
            key != crate::log_event::ENV_DAEMON_HTTP_TOKEN
                && key != crate::log_event::ENV_DAEMON_HTTP_SERVER
        })
        .collect()
}

/// The exact `ProcessConfig` a captured turn spawns with. Extracted so
/// the client-env routing is testable: this is the value handed to
/// `NativeProcess::new`, not a reconstruction of it.
///
/// The API session id becomes the agent's bulletin-board address, so the
/// client that created the session can message the agent it is running.
fn turn_process_config(
    command: Vec<String>,
    cwd: std::path::PathBuf,
    client_env: &[(String, String)],
    session_id: &str,
) -> ProcessConfig {
    let mut env = api_turn_env(client_env);
    env.retain(|(key, _)| key != BOARD_ID_ENV);
    env.push((BOARD_ID_ENV.to_string(), session_id.to_string()));
    ProcessConfig {
        command: crate::subprocess::command_spec_for_subprocess(command),
        cwd: Some(cwd),
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: invisible_helper_creationflags(),
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiTurnLaunchError {
    #[error("{0}")]
    Store(ApiSessionStoreError),
    #[error("{0}")]
    Io(io::Error),
    #[error("{0}")]
    InvalidPlan(String),
}

impl From<ApiSessionStoreError> for ApiTurnLaunchError {
    fn from(value: ApiSessionStoreError) -> Self {
        Self::Store(value)
    }
}
impl From<io::Error> for ApiTurnLaunchError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
/// Starts a subprocess-only API turn. `plan` must be produced by the typed
/// headless adapter; this boundary rejects a missing or mismatched cwd rather
/// than inheriting the daemon's working directory.
///
/// Forwards no client environment: [`launch_captured_turn_with_client_env`]
/// with an empty slice, i.e. the pre-#1209 daemon-env behaviour.
pub fn launch_captured_turn(
    store: ApiSessionStore,
    session_id: &str,
    plan: LaunchPlan,
) -> Result<Arc<NativeProcess>, ApiTurnLaunchError> {
    launch_captured_turn_with_client_env(store, session_id, plan, &[])
}

/// [`launch_captured_turn`], with the creating client's environment layered
/// over the daemon's (#933/#1157). A caller with no client env in hand
/// passes `&[]` and gets the pre-#1209 daemon-env behaviour unchanged; the
/// live wiring waits on #933 adding an env to the durable API session
/// record, since that record is the only place a client env could come
/// from once the HTTP request that created it has returned.
pub fn launch_captured_turn_with_client_env(
    store: ApiSessionStore,
    session_id: &str,
    plan: LaunchPlan,
    client_env: &[(String, String)],
) -> Result<Arc<NativeProcess>, ApiTurnLaunchError> {
    let session = store.get(session_id)?;
    let plan_cwd = plan.cwd.as_ref().map(Path::new).ok_or_else(|| {
        ApiTurnLaunchError::InvalidPlan("headless turn plan omitted cwd".to_string())
    })?;
    if plan_cwd != session.cwd.as_path() {
        return Err(ApiTurnLaunchError::InvalidPlan(
            "headless turn plan cwd differs from persisted API session cwd".to_string(),
        ));
    }
    let backend = match session.backend {
        ApiSessionBackend::Claude => crate::backend::Backend::Claude,
        ApiSessionBackend::Codex => crate::backend::Backend::Codex,
    };
    if plan.backend != backend {
        return Err(ApiTurnLaunchError::InvalidPlan(
            "headless turn backend differs from logical session backend".to_string(),
        ));
    }
    let turn_id = format!(
        "turn-{}-{}",
        session.generation.saturating_add(1),
        unix_millis_now()
    );
    let turn = store.begin_turn(session_id, turn_id)?;
    launch_claimed_turn_with_client_env(store, session_id, plan, turn, client_env)
}

/// Starts an already-durably-claimed generation.  Lifecycle admission must
/// claim before process creation so duplicate concurrent requests cannot spawn
/// two children between an idempotency check and ledger write.
///
/// Forwards no client environment: [`launch_claimed_turn_with_client_env`]
/// with an empty slice, i.e. the pre-#1209 daemon-env behaviour.
pub fn launch_claimed_turn(
    store: ApiSessionStore,
    session_id: &str,
    plan: LaunchPlan,
    turn: ApiTurnRecord,
) -> Result<Arc<NativeProcess>, ApiTurnLaunchError> {
    launch_claimed_turn_with_client_env(store, session_id, plan, turn, &[])
}

/// [`launch_claimed_turn`], with the creating client's environment layered
/// over the daemon's (#933/#1157). A caller with no client env in hand
/// passes `&[]` and gets the pre-#1209 daemon-env behaviour unchanged; the
/// live wiring waits on #933 adding an env to the durable API session
/// record, since that record is the only place a client env could come
/// from once the HTTP request that created it has returned.
pub fn launch_claimed_turn_with_client_env(
    store: ApiSessionStore,
    session_id: &str,
    plan: LaunchPlan,
    turn: ApiTurnRecord,
    client_env: &[(String, String)],
) -> Result<Arc<NativeProcess>, ApiTurnLaunchError> {
    let session = store.get(session_id)?;
    let plan_cwd = plan.cwd.as_ref().map(Path::new).ok_or_else(|| {
        ApiTurnLaunchError::InvalidPlan("headless turn plan omitted cwd".to_string())
    })?;
    if plan_cwd != session.cwd.as_path() {
        return Err(ApiTurnLaunchError::InvalidPlan(
            "headless turn plan cwd differs from persisted API session cwd".to_string(),
        ));
    }
    let backend = match session.backend {
        ApiSessionBackend::Claude => crate::backend::Backend::Claude,
        ApiSessionBackend::Codex => crate::backend::Backend::Codex,
    };
    if plan.backend != backend {
        return Err(ApiTurnLaunchError::InvalidPlan(
            "headless turn backend differs from logical session backend".to_string(),
        ));
    }
    if session.current_turn_id.as_deref() != Some(turn.id.as_str()) {
        return Err(ApiTurnLaunchError::InvalidPlan(
            "claimed API turn is no longer current".to_string(),
        ));
    }
    let process = Arc::new(NativeProcess::new(turn_process_config(
        plan.command,
        session.cwd.clone(),
        client_env,
        session_id,
    )));
    if let Err(error) = process.start() {
        let _ = store.finish_turn(
            session_id,
            &turn.id,
            ApiTurnState::Failed,
            Some("spawn_failed".to_string()),
        );
        return Err(ApiTurnLaunchError::Io(io::Error::other(error.to_string())));
    }
    if let Some(pid) = process.pid() {
        let _ = store.set_turn_root_identity(
            session_id,
            &turn.id,
            ProcessIdentity::observe(pid).unwrap_or_else(|| ProcessIdentity::pid_only(pid)),
        );
    }
    let drain_process = Arc::clone(&process);
    let drain_store = store.clone();
    let drain_session = session_id.to_string();
    let drain_turn = turn.id.clone();
    let log = api_turn_log_path(store.state_dir(), &drain_session, turn.generation);
    let drain_handle = thread::spawn(move || {
        drain_jsonl(
            drain_process,
            drain_store,
            drain_session,
            drain_turn,
            backend,
            log,
        )
    });
    let wait_process = Arc::clone(&process);
    let wait_store = store;
    let wait_session = session_id.to_string();
    let wait_turn = turn.id;
    thread::spawn(move || {
        let code = wait_process.wait(None).unwrap_or(1);
        let _ = drain_handle.join();
        let state = if code == 0 {
            ApiTurnState::Completed
        } else {
            ApiTurnState::Failed
        };
        let _ = wait_store.finish_turn(
            &wait_session,
            &wait_turn,
            state,
            Some(format!("exit_{code}")),
        );
    });
    Ok(process)
}

fn drain_jsonl(
    process: Arc<NativeProcess>,
    store: ApiSessionStore,
    session_id: String,
    turn_id: String,
    backend: crate::backend::Backend,
    log_path: std::path::PathBuf,
) {
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => observe_line(
                &store,
                &session_id,
                &turn_id,
                backend,
                &event.line,
                &log_path,
            ),
            ReadStatus::Timeout => {
                if process.returncode().is_some() {
                    break;
                }
            }
            ReadStatus::Eof => break,
        }
    }
    // Exit is not end of output: the last lines can reach the capture queue
    // after the exit code on a loaded machine. `wait` runs running-process's
    // bounded capture drain, so the queue is complete once it returns (#2159).
    let _ = process.wait(None);
    for event in process.drain_combined() {
        observe_line(
            &store,
            &session_id,
            &turn_id,
            backend,
            &event.line,
            &log_path,
        );
    }
}

fn observe_line(
    store: &ApiSessionStore,
    session_id: &str,
    turn_id: &str,
    backend: crate::backend::Backend,
    bytes: &[u8],
    log_path: &Path,
) {
    append_raw_line(log_path, bytes);
    let line = String::from_utf8_lossy(bytes).to_string();
    let raw = truncate(&line);
    let _ = store.append_event(
        session_id,
        Some(turn_id.to_string()),
        "raw_jsonl".to_string(),
        json!({"line": raw}),
    );
    match parse_backend_event(backend, &line) {
        BackendEvent::ProviderSessionId(id) => {
            let _ = store.set_provider_session_id(session_id, id.clone());
            let _ = store.append_event(
                session_id,
                Some(turn_id.to_string()),
                "provider_identity".to_string(),
                json!({"provider_session_id": id}),
            );
        }
        BackendEvent::Opaque(value) => {
            let _ = store.append_event(
                session_id,
                Some(turn_id.to_string()),
                "backend_event".to_string(),
                bounded_value(value),
            );
        }
        BackendEvent::Malformed { line, error } => {
            let _ = store.append_event(
                session_id,
                Some(turn_id.to_string()),
                "backend_malformed".to_string(),
                json!({"line": truncate(&line), "error": error}),
            );
        }
    }
}

fn append_raw_line(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(path).map(|m| m.len()).unwrap_or(0) >= RAW_LOG_MAX_BYTES {
        let _ = fs::rename(path, path.with_extension("jsonl.1"));
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(bytes);
        let _ = file.write_all(b"\n");
    }
}
fn truncate(value: &str) -> String {
    value.chars().take(EVENT_LINE_MAX_BYTES).collect()
}
fn bounded_value(value: Value) -> Value {
    if serde_json::to_vec(&value)
        .map(|b| b.len())
        .unwrap_or(usize::MAX)
        <= EVENT_LINE_MAX_BYTES
    {
        value
    } else {
        json!({"truncated": true})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::api_sessions::{CreateApiSession, ResolvedApiSessionSettings};
    use tempfile::TempDir;
    fn store() -> (TempDir, ApiSessionStore, String) {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store
            .create(CreateApiSession {
                backend: ApiSessionBackend::Claude,
                cwd: temp.path().to_path_buf(),
                name: None,
                resolved_settings: ResolvedApiSessionSettings {
                    model: None,
                    safe: true,
                    model_provider: None,
                    harness: None,
                    routing_mode: None,
                },
            })
            .unwrap();
        (temp, store, record.id)
    }
    #[test]
    fn observes_identity_opaque_and_malformed_without_unbounded_events() {
        let (temp, store, id) = store();
        let turn = store.begin_turn(&id, "turn-a".to_string()).unwrap();
        let log = api_turn_log_path(temp.path(), &id, turn.generation);
        observe_line(
            &store,
            &id,
            &turn.id,
            crate::backend::Backend::Claude,
            br#"{"type":"system","subtype":"init","session_id":"provider-a"}"#,
            &log,
        );
        observe_line(
            &store,
            &id,
            &turn.id,
            crate::backend::Backend::Claude,
            br#"{"type":"future"}"#,
            &log,
        );
        observe_line(
            &store,
            &id,
            &turn.id,
            crate::backend::Backend::Claude,
            b"not json",
            &log,
        );
        for _ in 0..600 {
            observe_line(
                &store,
                &id,
                &turn.id,
                crate::backend::Backend::Claude,
                br#"{"type":"progress"}"#,
                &log,
            );
        }
        let record = store.get(&id).unwrap();
        assert_eq!(record.provider_session_id.as_deref(), Some("provider-a"));
        assert_eq!(
            record.events.len(),
            super::super::api_sessions::DEFAULT_EVENT_LIMIT
        );
        assert!(log.exists());
    }

    #[test]
    fn codex_identity_is_persisted_before_turn_completion() {
        let (temp, store, id) = store();
        let turn = store.begin_turn(&id, "turn-codex".to_string()).unwrap();
        let log = api_turn_log_path(temp.path(), &id, turn.generation);
        observe_line(
            &store,
            &id,
            &turn.id,
            crate::backend::Backend::Codex,
            br#"{"type":"thread.started","thread_id":"thread-a"}"#,
            &log,
        );
        let record = store.get(&id).unwrap();
        assert_eq!(record.provider_session_id.as_deref(), Some("thread-a"));
        assert_eq!(record.current_turn_id.as_deref(), Some(turn.id.as_str()));
    }

    #[test]
    fn raw_log_rotates_before_appending_next_line() {
        let (temp, store, id) = store();
        let turn = store.begin_turn(&id, "turn-log".to_string()).unwrap();
        let log = api_turn_log_path(temp.path(), &id, turn.generation);
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, vec![b'x'; RAW_LOG_MAX_BYTES as usize]).unwrap();
        observe_line(
            &store,
            &id,
            &turn.id,
            crate::backend::Backend::Claude,
            br#"{"type":"future"}"#,
            &log,
        );
        assert!(log.with_extension("jsonl.1").exists());
        assert_eq!(fs::read_to_string(&log).unwrap(), "{\"type\":\"future\"}\n");
    }

    #[test]
    fn captured_api_turn_environment_excludes_dashboard_capabilities() {
        assert!(!api_turn_env(&[])
            .iter()
            .any(|(key, _)| key == crate::log_event::ENV_DAEMON_HTTP_TOKEN
                || key == crate::log_event::ENV_DAEMON_HTTP_SERVER));
    }

    fn value_of<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// A literal pair list as the owned shape a client env is carried in.
    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    /// RED for #1209: `api_turn_env` called `io_helpers::child_env()` with
    /// no client env, so an API turn spawned with the daemon's frozen
    /// environment even when the creating client's was known. Asserted on
    /// the `ProcessConfig` that is literally handed to `NativeProcess`.
    #[test]
    fn an_api_turn_created_with_a_client_env_spawns_with_that_envs_path() {
        let home = TempDir::new().unwrap();
        let home_str = home.path().to_string_lossy().into_owned();
        let client = pairs(&[
            ("PATH", "/client/only/bin"),
            ("HOME", home_str.as_str()),
            ("USERPROFILE", home_str.as_str()),
            ("CLUD_TEST_CLIENT_ONLY", "1"),
        ]);
        let cwd = home.path().to_path_buf();
        let config = turn_process_config(vec!["claude".to_string()], cwd, &client, "s1");
        // Borrowed, not moved out: a partial move of `config.env` would stop
        // compiling the day `ProcessConfig` grows a `Drop` impl upstream.
        let env = config
            .env
            .as_deref()
            .expect("captured turns pass an explicit env");
        let path = value_of(env, "PATH").expect("PATH must be present");
        assert!(
            path.ends_with("/client/only/bin"),
            "the client's PATH must reach the turn; got {path}"
        );
        assert_eq!(value_of(env, "CLUD_TEST_CLIENT_ONLY"), Some("1"));
        let token = crate::log_event::ENV_DAEMON_HTTP_TOKEN;
        let server = crate::log_event::ENV_DAEMON_HTTP_SERVER;
        assert_eq!(value_of(env, token), None);
        assert_eq!(value_of(env, server), None);
    }

    /// The empty slice must mean "exactly today's behaviour", not "empty
    /// environment" - every call site passes it until #933 gives the
    /// durable API session record an env. The assertion below scans all
    /// values instead of doing an exact-match `PATH` lookup — the live
    /// environment's PATH key is spelled `Path` on Windows, and
    /// `shim_session::prepend_to_path` matches it case-insensitively, so an
    /// exact-match `PATH` entry is absent there.
    #[test]
    fn no_client_env_falls_back_to_the_daemon_environment() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path().to_path_buf();
        let config = turn_process_config(vec!["claude".to_string()], cwd, &[], "s1");
        let env = config
            .env
            .as_deref()
            .expect("captured turns pass an explicit env");
        assert_eq!(value_of(env, "IN_CLUD"), Some("1"));
        assert!(
            !env.iter().any(|(_, v)| v.contains("/client/only/bin")),
            "the empty slice must not pick up the sibling test's synthetic client PATH"
        );
    }

    #[test]
    fn the_session_id_is_the_turns_board_address() {
        let temp = TempDir::new().unwrap();
        let client = vec![(BOARD_ID_ENV.to_string(), "stale".to_string())];
        let config = turn_process_config(
            vec!["claude".to_string()],
            temp.path().to_path_buf(),
            &client,
            "sess-42",
        );
        let env = config.env.as_deref().unwrap();
        assert_eq!(value_of(env, BOARD_ID_ENV), Some("sess-42"));
        assert_eq!(env.iter().filter(|(k, _)| k == BOARD_ID_ENV).count(), 1);
    }
}

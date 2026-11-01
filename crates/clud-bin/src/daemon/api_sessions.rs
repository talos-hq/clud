//! Durable logical API sessions, distinct from worker [`SessionSnapshot`]s.
//!
//! A worker snapshot describes one process and is intentionally retired when
//! that process dies. An API session instead owns the durable conversation
//! identity and a succession of short-lived turns. This module is storage and
//! state-machine infrastructure only: it never starts a child or exposes HTTP.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::process_identity::ProcessIdentity;

use super::io_helpers::{new_session_id, read_json_file, write_json_file};
use super::paths::{
    api_session_lock_path, api_session_path, api_sessions_create_lock_path, api_sessions_dir,
};
use super::types::unix_millis_now;

/// Retention is deliberately small: event polling is a convenience stream,
/// not an unbounded transcript database. Raw provider output belongs to the
/// later turn-execution log sink.
pub const DEFAULT_EVENT_LIMIT: usize = 512;
pub const DEFAULT_IDEMPOTENCY_LIMIT: usize = 128;
pub const MAX_EVENT_DATA_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiSessionBackend {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiSessionState {
    Starting,
    Running,
    Interrupting,
    Idle,
    Failed,
    Terminated,
}

impl ApiSessionState {
    pub fn accepts_turns(self) -> bool {
        matches!(self, Self::Starting | Self::Idle | Self::Failed)
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Interrupting)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiTurnState {
    Starting,
    Running,
    Completed,
    Interrupted,
    Failed,
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedApiSessionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub safe: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateApiSession {
    pub backend: ApiSessionBackend,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub resolved_settings: ResolvedApiSessionSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiTurnRecord {
    pub id: String,
    pub generation: u64,
    pub state: ApiTurnState,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_identity: Option<ProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiSessionEvent {
    pub cursor: u64,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub turn_id: String,
    pub request_fingerprint: String,
    pub created_at_ms: u64,
}

/// The durable, lock-protected result of claiming a next logical turn.
///
/// Keeping the idempotency ledger update in this transition means a retry
/// after a daemon restart can replay the original turn without ever creating
/// another provider subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginApiTurn {
    Started(ApiTurnRecord),
    Replayed(String),
    Busy,
    Terminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiSessionRecord {
    #[serde(default)]
    pub schema_version: u32,
    pub id: String,
    pub backend: ApiSessionBackend,
    /// Canonical absolute path, fixed at create time for every later resume.
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub resolved_settings: ResolvedApiSessionSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    pub state: ApiSessionState,
    #[serde(default)]
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn_id: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub turns: Vec<ApiTurnRecord>,
    #[serde(default)]
    pub events: VecDeque<ApiSessionEvent>,
    #[serde(default)]
    pub next_event_cursor: u64,
    #[serde(default)]
    pub idempotency: VecDeque<IdempotencyRecord>,
    /// Operator injections, oldest first (see `docs/architecture/session-inject.md`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injections: Vec<super::api_session_inject::ApiInjectionRecord>,
}

impl ApiSessionRecord {
    fn normalize_defaults(&mut self) {
        if self.schema_version == 0 {
            self.schema_version = 1;
        }
        if self.next_event_cursor == 0 {
            self.next_event_cursor = self
                .events
                .back()
                .map_or(1, |event| event.cursor.saturating_add(1));
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiSessionStoreError {
    #[error("{0}")]
    Io(io::Error),
    #[error("API session '{0}' not found")]
    NotFound(String),
    #[error("API session '{session_id}' is corrupt: {message}")]
    Corrupt { session_id: String, message: String },
    #[error("invalid cwd '{}': {message}", path.display())]
    InvalidCwd { path: PathBuf, message: String },
    #[error("cannot {operation} API session in {state:?} state")]
    InvalidTransition {
        state: ApiSessionState,
        operation: &'static str,
    },
    #[error(
        "API session event is {bytes} bytes; limit is {}",
        MAX_EVENT_DATA_BYTES
    )]
    EventTooLarge { bytes: usize },
    #[error("idempotency key '{key}' was reused for a different request")]
    IdempotencyConflict { key: String },
}

impl From<io::Error> for ApiSessionStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone)]
pub struct ApiSessionStore {
    state_dir: PathBuf,
    event_limit: usize,
    idempotency_limit: usize,
}

impl ApiSessionStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            event_limit: DEFAULT_EVENT_LIMIT,
            idempotency_limit: DEFAULT_IDEMPOTENCY_LIMIT,
        }
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    #[cfg(test)]
    fn with_limits(
        state_dir: impl Into<PathBuf>,
        event_limit: usize,
        idempotency_limit: usize,
    ) -> Self {
        Self {
            state_dir: state_dir.into(),
            event_limit,
            idempotency_limit,
        }
    }

    pub fn create(
        &self,
        request: CreateApiSession,
    ) -> Result<ApiSessionRecord, ApiSessionStoreError> {
        let cwd = canonical_api_cwd(&request.cwd)?;
        let _lock = self.lock_create()?;
        let now = unix_millis_now();
        let id = (0..16)
            .map(|_| new_session_id())
            .find(|candidate| !api_session_path(&self.state_dir, candidate).exists())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "could not allocate API session id",
                )
            })?;
        let record = ApiSessionRecord {
            schema_version: 1,
            id,
            backend: request.backend,
            cwd,
            name: request.name,
            resolved_settings: request.resolved_settings,
            provider_session_id: None,
            state: ApiSessionState::Starting,
            generation: 0,
            current_turn_id: None,
            created_at_ms: now,
            updated_at_ms: now,
            last_error: None,
            turns: Vec::new(),
            events: VecDeque::new(),
            next_event_cursor: 1,
            idempotency: VecDeque::new(),
            injections: Vec::new(),
        };
        self.write(&record)?;
        Ok(record)
    }

    pub fn get(&self, session_id: &str) -> Result<ApiSessionRecord, ApiSessionStoreError> {
        validate_session_id(session_id)?;
        self.read(session_id)
    }

    /// Lists valid records only. Corrupt files are deliberately not silently
    /// treated as sessions; callers can inspect `reconcile_after_restart` for
    /// the quarantined IDs.
    pub fn list(&self) -> Result<Vec<ApiSessionRecord>, ApiSessionStoreError> {
        let dir = api_sessions_dir(&self.state_dir);
        let mut records = Vec::new();
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(records),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if let Ok(record) = self.read(id) {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.created_at_ms);
        Ok(records)
    }

    pub fn begin_turn(
        &self,
        session_id: &str,
        turn_id: String,
    ) -> Result<ApiTurnRecord, ApiSessionStoreError> {
        self.mutate(session_id, |record| {
            if !record.state.accepts_turns() {
                return Err(ApiSessionStoreError::InvalidTransition {
                    state: record.state,
                    operation: "begin a turn for",
                });
            }
            record.generation = record.generation.saturating_add(1);
            let turn = ApiTurnRecord {
                id: turn_id,
                generation: record.generation,
                state: ApiTurnState::Starting,
                created_at_ms: unix_millis_now(),
                completed_at_ms: None,
                disposition: None,
                root_identity: None,
            };
            record.current_turn_id = Some(turn.id.clone());
            record.turns.push(turn.clone());
            record.state = ApiSessionState::Running;
            Ok(turn)
        })
    }

    /// Atomically checks a request key and claims the next generation.  This
    /// is the lifecycle controller's only admission path for a new process.
    pub fn begin_idempotent_turn(
        &self,
        session_id: &str,
        turn_id: String,
        key: Option<String>,
        fingerprint: String,
    ) -> Result<BeginApiTurn, ApiSessionStoreError> {
        let limit = self.idempotency_limit;
        self.mutate(session_id, |record| {
            if let Some(key) = key.as_ref() {
                if let Some(existing) = record.idempotency.iter().find(|entry| entry.key == *key) {
                    return if existing.request_fingerprint == fingerprint {
                        Ok(BeginApiTurn::Replayed(existing.turn_id.clone()))
                    } else {
                        Err(ApiSessionStoreError::IdempotencyConflict { key: key.clone() })
                    };
                }
            }
            if record.state == ApiSessionState::Terminated {
                return Ok(BeginApiTurn::Terminated);
            }
            if !record.state.accepts_turns() {
                return Ok(BeginApiTurn::Busy);
            }
            record.generation = record.generation.saturating_add(1);
            let turn = ApiTurnRecord {
                id: turn_id,
                generation: record.generation,
                state: ApiTurnState::Starting,
                created_at_ms: unix_millis_now(),
                completed_at_ms: None,
                disposition: None,
                root_identity: None,
            };
            record.current_turn_id = Some(turn.id.clone());
            record.turns.push(turn.clone());
            record.state = ApiSessionState::Running;
            if let Some(key) = key {
                record.idempotency.push_back(IdempotencyRecord {
                    key,
                    turn_id: turn.id.clone(),
                    request_fingerprint: fingerprint,
                    created_at_ms: unix_millis_now(),
                });
                while record.idempotency.len() > limit {
                    record.idempotency.pop_front();
                }
            }
            Ok(BeginApiTurn::Started(turn))
        })
    }

    /// Records the durable side of a graceful-interrupt request before the
    /// process signal is sent.  A late waiter may then only seal this exact
    /// turn; it cannot make a later generation idle.
    pub fn begin_interrupt(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<(), ApiSessionStoreError> {
        self.mutate(session_id, |record| {
            if record.current_turn_id.as_deref() != Some(turn_id) {
                return Ok(());
            }
            record.state = ApiSessionState::Interrupting;
            Ok(())
        })
    }

    pub fn set_provider_session_id(
        &self,
        session_id: &str,
        provider_session_id: String,
    ) -> Result<ApiSessionRecord, ApiSessionStoreError> {
        self.mutate(session_id, |record| {
            record.provider_session_id = Some(provider_session_id);
            Ok(record.clone())
        })
    }

    pub fn set_turn_root_identity(
        &self,
        session_id: &str,
        turn_id: &str,
        identity: ProcessIdentity,
    ) -> Result<(), ApiSessionStoreError> {
        self.mutate(session_id, |record| {
            let turn = record
                .turns
                .iter_mut()
                .find(|turn| turn.id == turn_id)
                .ok_or_else(|| ApiSessionStoreError::NotFound(turn_id.to_string()))?;
            turn.root_identity = Some(identity);
            turn.state = ApiTurnState::Running;
            Ok(())
        })
    }

    pub fn append_event(
        &self,
        session_id: &str,
        turn_id: Option<String>,
        kind: String,
        data: Value,
    ) -> Result<ApiSessionEvent, ApiSessionStoreError> {
        let bytes = serde_json::to_vec(&data)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .len();
        if bytes > MAX_EVENT_DATA_BYTES {
            return Err(ApiSessionStoreError::EventTooLarge { bytes });
        }
        let limit = self.event_limit;
        self.mutate(session_id, |record| {
            let event = ApiSessionEvent {
                cursor: record.next_event_cursor,
                at_ms: unix_millis_now(),
                turn_id,
                kind,
                data,
            };
            record.next_event_cursor = record.next_event_cursor.saturating_add(1);
            record.events.push_back(event.clone());
            while record.events.len() > limit {
                record.events.pop_front();
            }
            Ok(event)
        })
    }

    pub fn events_after(
        &self,
        session_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<ApiSessionEvent>, ApiSessionStoreError> {
        Ok(self
            .read(session_id)?
            .events
            .into_iter()
            .filter(|event| event.cursor > after)
            .take(limit.min(self.event_limit))
            .collect())
    }

    pub fn finish_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        state: ApiTurnState,
        disposition: Option<String>,
    ) -> Result<ApiSessionRecord, ApiSessionStoreError> {
        self.mutate(session_id, |record| {
            if record.current_turn_id.as_deref() != Some(turn_id) {
                // A terminal lifecycle seal wins over the asynchronous
                // waiter. It may observe the child exit after `kill()`
                // returned, but must never turn a terminal record back into
                // a failed/resumable one.
                if record
                    .turns
                    .iter()
                    .any(|turn| turn.id == turn_id && turn.state == ApiTurnState::Killed)
                {
                    return Ok(record.clone());
                }
                if let Some(turn) = record.turns.iter_mut().find(|turn| {
                    turn.id == turn_id
                        && matches!(
                            turn.state,
                            ApiTurnState::Completed
                                | ApiTurnState::Interrupted
                                | ApiTurnState::Failed
                                | ApiTurnState::Killed
                        )
                }) {
                    // An interrupt signal may make the waiter observe a
                    // nonzero exit first.  Preserve the lifecycle-requested
                    // disposition without ever changing a newer session's
                    // current-generation state.
                    if state == ApiTurnState::Interrupted {
                        turn.state = state;
                        turn.disposition = disposition;
                        turn.completed_at_ms = Some(unix_millis_now());
                    }
                    return Ok(record.clone());
                }
            }
            let Some(turn) = record.turns.iter_mut().find(|turn| turn.id == turn_id) else {
                return Err(ApiSessionStoreError::NotFound(turn_id.to_string()));
            };
            turn.state = state;
            turn.disposition = disposition;
            turn.completed_at_ms = Some(unix_millis_now());
            if record.current_turn_id.as_deref() == Some(turn_id) {
                record.current_turn_id = None;
                record.state = match state {
                    ApiTurnState::Completed | ApiTurnState::Interrupted => ApiSessionState::Idle,
                    ApiTurnState::Failed => ApiSessionState::Failed,
                    ApiTurnState::Killed => ApiSessionState::Terminated,
                    ApiTurnState::Starting | ApiTurnState::Running => ApiSessionState::Running,
                };
            }
            Ok(record.clone())
        })
    }

    /// Atomically makes a logical session terminal before its process is
    /// signalled.  Returning the sealed generation lets the lifecycle use
    /// its identity for best-effort tree cleanup without leaving a window for
    /// the asynchronous controller waiter to record `exit_-9` as a resumable
    /// failure.
    pub fn begin_terminal_kill(
        &self,
        session_id: &str,
    ) -> Result<Option<ApiTurnRecord>, ApiSessionStoreError> {
        self.mutate(session_id, |record| {
            let sealed = record
                .current_turn_id
                .as_deref()
                .and_then(|turn_id| record.turns.iter_mut().find(|turn| turn.id == turn_id))
                .map(|turn| {
                    turn.state = ApiTurnState::Killed;
                    turn.disposition = Some("terminal_kill_pending".to_string());
                    turn.completed_at_ms = Some(unix_millis_now());
                    turn.clone()
                });
            record.current_turn_id = None;
            record.state = ApiSessionState::Terminated;
            Ok(sealed)
        })
    }

    /// Refines the durable terminal-kill diagnostic after bounded process
    /// cleanup.  It deliberately cannot change a sealed turn or session back
    /// into a resumable state.
    pub fn refine_terminal_kill(
        &self,
        session_id: &str,
        turn_id: &str,
        disposition: &str,
    ) -> Result<ApiSessionRecord, ApiSessionStoreError> {
        self.mutate(session_id, |record| {
            if let Some(turn) = record.turns.iter_mut().find(|turn| turn.id == turn_id) {
                if turn.state == ApiTurnState::Killed {
                    turn.disposition = Some(disposition.to_string());
                }
            }
            record.current_turn_id = None;
            record.state = ApiSessionState::Terminated;
            Ok(record.clone())
        })
    }

    pub fn terminate(&self, session_id: &str) -> Result<ApiSessionRecord, ApiSessionStoreError> {
        self.mutate(session_id, |record| {
            record.state = ApiSessionState::Terminated;
            record.current_turn_id = None;
            Ok(record.clone())
        })
    }

    pub fn remember_idempotency(
        &self,
        session_id: &str,
        key: String,
        turn_id: String,
        request_fingerprint: String,
    ) -> Result<IdempotencyRecord, ApiSessionStoreError> {
        let limit = self.idempotency_limit;
        self.mutate(session_id, |record| {
            if let Some(existing) = record.idempotency.iter().find(|entry| entry.key == key) {
                if existing.request_fingerprint == request_fingerprint {
                    return Ok(existing.clone());
                }
                return Err(ApiSessionStoreError::IdempotencyConflict { key });
            }
            let entry = IdempotencyRecord {
                key,
                turn_id,
                request_fingerprint,
                created_at_ms: unix_millis_now(),
            };
            record.idempotency.push_back(entry.clone());
            while record.idempotency.len() > limit {
                record.idempotency.pop_front();
            }
            Ok(entry)
        })
    }

    /// Conservative restart recovery: no new daemon trusts a persisted PID as
    /// ownership of a child. Any active generation becomes explicitly failed
    /// and may only resume later through its captured provider identity.
    pub fn reconcile_after_restart(&self) -> Result<Vec<String>, ApiSessionStoreError> {
        let mut recovered = Vec::new();
        for record in self.list()? {
            if !record.state.is_active() {
                continue;
            }
            let id = record.id.clone();
            self.mutate(&id, |record| {
                record.state = ApiSessionState::Failed;
                record.current_turn_id = None;
                record.last_error = Some("daemon restarted while a turn was active; provider process ownership was not recovered".to_string());
                if let Some(turn) = record.turns.last_mut().filter(|turn| matches!(turn.state, ApiTurnState::Starting | ApiTurnState::Running)) {
                    turn.state = ApiTurnState::Failed;
                    turn.completed_at_ms = Some(unix_millis_now());
                    turn.disposition = Some("daemon_restart".to_string());
                    // Keep the identity as diagnostic metadata only; no code in
                    // this store probes or signals it after restart.
                }
                Ok(())
            })?;
            recovered.push(id);
        }
        Ok(recovered)
    }

    /// Moves unreadable state out of the active directory. A corrupt record is
    /// never overwritten, preserving evidence for manual recovery.
    pub fn quarantine_corrupt(
        &self,
        session_id: &str,
    ) -> Result<Option<PathBuf>, ApiSessionStoreError> {
        let path = api_session_path(&self.state_dir, session_id);
        match self.read(session_id) {
            Ok(_) => Ok(None),
            Err(ApiSessionStoreError::Corrupt { .. }) => {
                let target = path.with_extension(format!("corrupt-{}", unix_millis_now()));
                fs::rename(path, &target)?;
                Ok(Some(target))
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn mutate<T>(
        &self,
        session_id: &str,
        action: impl FnOnce(&mut ApiSessionRecord) -> Result<T, ApiSessionStoreError>,
    ) -> Result<T, ApiSessionStoreError> {
        validate_session_id(session_id)?;
        let _lock = self.lock_session(session_id)?;
        let mut record = self.read(session_id)?;
        let result = action(&mut record)?;
        record.updated_at_ms = unix_millis_now();
        self.write(&record)?;
        Ok(result)
    }

    fn read(&self, session_id: &str) -> Result<ApiSessionRecord, ApiSessionStoreError> {
        validate_session_id(session_id)?;
        let path = api_session_path(&self.state_dir, session_id);
        let mut record: ApiSessionRecord = match read_json_file(&path) {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ApiSessionStoreError::NotFound(session_id.to_string()))
            }
            Err(error) => {
                return Err(ApiSessionStoreError::Corrupt {
                    session_id: session_id.to_string(),
                    message: error.to_string(),
                })
            }
        };
        if record.id != session_id {
            return Err(ApiSessionStoreError::Corrupt {
                session_id: session_id.to_string(),
                message: "filename and record id differ".to_string(),
            });
        }
        if !record.cwd.is_absolute() {
            return Err(ApiSessionStoreError::Corrupt {
                session_id: session_id.to_string(),
                message: "persisted cwd is not absolute".to_string(),
            });
        }
        record.normalize_defaults();
        Ok(record)
    }

    fn write(&self, record: &ApiSessionRecord) -> Result<(), ApiSessionStoreError> {
        write_json_file(&api_session_path(&self.state_dir, &record.id), record).map_err(Into::into)
    }

    fn lock_create(&self) -> Result<File, ApiSessionStoreError> {
        lock_file(&api_sessions_create_lock_path(&self.state_dir))
    }
    fn lock_session(&self, session_id: &str) -> Result<File, ApiSessionStoreError> {
        lock_file(&api_session_lock_path(&self.state_dir, session_id))
    }
}

fn validate_session_id(session_id: &str) -> Result<(), ApiSessionStoreError> {
    if session_id.is_empty()
        || session_id.contains(['/', '\\'])
        || session_id == "."
        || session_id == ".."
    {
        return Err(ApiSessionStoreError::NotFound(session_id.to_string()));
    }
    Ok(())
}

fn lock_file(path: &Path) -> Result<File, ApiSessionStoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn canonical_api_cwd(path: &Path) -> Result<PathBuf, ApiSessionStoreError> {
    if !path.is_absolute() {
        return Err(ApiSessionStoreError::InvalidCwd {
            path: path.to_path_buf(),
            message: "cwd must be an absolute path".to_string(),
        });
    }
    fs::canonicalize(path)
        .map_err(|error| ApiSessionStoreError::InvalidCwd {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
        .and_then(|path| {
            if !path.is_absolute() {
                return Err(ApiSessionStoreError::InvalidCwd {
                    path,
                    message: "canonical path was not absolute".to_string(),
                });
            }
            if !path.is_dir() {
                return Err(ApiSessionStoreError::InvalidCwd {
                    path,
                    message: "canonical path is not a directory".to_string(),
                });
            }
            Ok(path)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn request(cwd: &Path) -> CreateApiSession {
        CreateApiSession {
            backend: ApiSessionBackend::Claude,
            cwd: cwd.to_path_buf(),
            name: Some("work".to_string()),
            resolved_settings: ResolvedApiSessionSettings {
                model: Some("test".to_string()),
                safe: true,
                model_provider: Some("claude".to_string()),
                harness: Some("claude".to_string()),
                routing_mode: Some("direct".to_string()),
            },
        }
    }

    #[test]
    fn creates_canonical_immutable_logical_record_separate_from_worker_snapshots() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path().join("cwd");
        fs::create_dir(&cwd).unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(&cwd)).unwrap();
        assert!(record.cwd.is_absolute());
        assert_eq!(record.cwd, fs::canonicalize(&cwd).unwrap());
        assert!(
            api_session_path(temp.path(), &record.id).starts_with(temp.path().join("api-sessions"))
        );
        assert_ne!(
            api_session_path(temp.path(), &record.id),
            super::super::paths::session_snapshot_path(temp.path(), &record.id)
        );
    }

    #[test]
    fn rejects_nonexistent_cwd_before_any_record_is_written() {
        let temp = TempDir::new().unwrap();
        let error = ApiSessionStore::new(temp.path())
            .create(request(&temp.path().join("missing")))
            .unwrap_err();
        assert!(matches!(error, ApiSessionStoreError::InvalidCwd { .. }));
        assert!(ApiSessionStore::new(temp.path()).list().unwrap().is_empty());
    }

    #[test]
    fn rejects_relative_cwd_before_canonicalization() {
        let temp = TempDir::new().unwrap();
        // A relative path is rejected before existence or canonicalization is
        // considered, so the fixture must not create anything under the
        // checkout (which is read-only in the Linux validation stack).
        let relative = Path::new("relative-cwd");
        let error = ApiSessionStore::new(temp.path())
            .create(request(relative))
            .unwrap_err();
        assert!(matches!(error, ApiSessionStoreError::InvalidCwd { .. }));
    }

    #[test]
    fn rejects_file_cwd_before_any_record_is_written() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("not-a-directory");
        fs::write(&file, "x").unwrap();
        let error = ApiSessionStore::new(temp.path())
            .create(request(&file))
            .unwrap_err();
        assert!(matches!(error, ApiSessionStoreError::InvalidCwd { .. }));
        assert!(ApiSessionStore::new(temp.path()).list().unwrap().is_empty());
    }

    #[test]
    fn rejects_session_id_path_traversal_before_touching_storage() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        assert!(matches!(
            store.get("../daemon"),
            Err(ApiSessionStoreError::NotFound(_))
        ));
        assert!(matches!(
            store.begin_turn("..\\daemon", "turn".to_string()),
            Err(ApiSessionStoreError::NotFound(_))
        ));
    }

    #[test]
    fn older_defaulted_record_loads_and_normalizes_cursor() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let path = api_session_path(temp.path(), "sess-old");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "id": "sess-old", "backend": "claude", "cwd": cwd,
                "resolved_settings": {}, "state": "idle", "created_at_ms": 1,
                "updated_at_ms": 1
            }))
            .unwrap(),
        )
        .unwrap();
        let record = ApiSessionStore::new(temp.path()).get("sess-old").unwrap();
        assert_eq!(record.schema_version, 1);
        assert_eq!(record.next_event_cursor, 1);
        assert!(record.events.is_empty());
    }

    #[test]
    fn event_and_idempotency_retention_are_bounded_and_cursor_monotonic() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::with_limits(temp.path(), 2, 2);
        let record = store.create(request(temp.path())).unwrap();
        for n in 0..3 {
            store
                .append_event(
                    &record.id,
                    None,
                    "output".to_string(),
                    Value::String(n.to_string()),
                )
                .unwrap();
            store
                .remember_idempotency(
                    &record.id,
                    format!("key-{n}"),
                    format!("turn-{n}"),
                    format!("hash-{n}"),
                )
                .unwrap();
        }
        let loaded = store.get(&record.id).unwrap();
        assert_eq!(
            loaded
                .events
                .iter()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(
            loaded
                .idempotency
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            vec!["key-1", "key-2"]
        );
        assert_eq!(store.events_after(&record.id, 2, 50).unwrap()[0].cursor, 3);
    }

    #[test]
    fn turns_transition_to_idle_without_losing_provider_identity() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(temp.path())).unwrap();
        let turn = store.begin_turn(&record.id, "turn-1".to_string()).unwrap();
        store
            .set_provider_session_id(&record.id, "provider-1".to_string())
            .unwrap();
        let final_record = store
            .finish_turn(
                &record.id,
                &turn.id,
                ApiTurnState::Completed,
                Some("completed".to_string()),
            )
            .unwrap();
        assert_eq!(final_record.state, ApiSessionState::Idle);
        assert_eq!(
            final_record.provider_session_id.as_deref(),
            Some("provider-1")
        );
        assert_eq!(final_record.generation, 1);
    }

    #[test]
    fn terminal_kill_seal_wins_over_late_controller_exit() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(temp.path())).unwrap();
        let turn = match store
            .begin_idempotent_turn(
                &record.id,
                "turn-1".to_string(),
                Some("request-1".to_string()),
                "fingerprint-1".to_string(),
            )
            .unwrap()
        {
            BeginApiTurn::Started(turn) => turn,
            other => panic!("expected turn claim, got {other:?}"),
        };

        let sealed = store.begin_terminal_kill(&record.id).unwrap().unwrap();
        assert_eq!(sealed.id, turn.id);
        assert_eq!(sealed.state, ApiTurnState::Killed);
        let late = store
            .finish_turn(
                &record.id,
                &turn.id,
                ApiTurnState::Failed,
                Some("exit_-9".to_string()),
            )
            .unwrap();
        assert_eq!(late.state, ApiSessionState::Terminated);
        assert_eq!(late.current_turn_id, None);
        assert_eq!(late.turns[0].state, ApiTurnState::Killed);
        assert_eq!(
            late.turns[0].disposition.as_deref(),
            Some("terminal_kill_pending")
        );

        let refined = store
            .refine_terminal_kill(&record.id, &turn.id, "terminal_kill")
            .unwrap();
        assert_eq!(refined.state, ApiSessionState::Terminated);
        assert_eq!(refined.current_turn_id, None);
        assert_eq!(
            refined.turns[0].disposition.as_deref(),
            Some("terminal_kill")
        );
        assert!(matches!(
            store
                .begin_idempotent_turn(
                    &record.id,
                    "turn-ignored".to_string(),
                    Some("request-1".to_string()),
                    "fingerprint-1".to_string(),
                )
                .unwrap(),
            BeginApiTurn::Replayed(id) if id == turn.id
        ));
        assert!(matches!(
            store
                .begin_idempotent_turn(
                    &record.id,
                    "turn-2".to_string(),
                    Some("request-2".to_string()),
                    "fingerprint-2".to_string(),
                )
                .unwrap(),
            BeginApiTurn::Terminated
        ));
    }

    #[test]
    fn stale_turn_completion_cannot_change_the_current_generation_state() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(temp.path())).unwrap();
        let first = store.begin_turn(&record.id, "turn-1".to_string()).unwrap();
        store
            .finish_turn(
                &record.id,
                &first.id,
                ApiTurnState::Completed,
                Some("exit_0".to_string()),
            )
            .unwrap();
        let second = store.begin_turn(&record.id, "turn-2".to_string()).unwrap();
        let loaded = store
            .finish_turn(
                &record.id,
                &first.id,
                ApiTurnState::Failed,
                Some("late_exit_1".to_string()),
            )
            .unwrap();
        assert_eq!(loaded.current_turn_id.as_deref(), Some(second.id.as_str()));
        assert_eq!(loaded.state, ApiSessionState::Running);
        assert_eq!(loaded.turns[1].state, ApiTurnState::Starting);
    }

    #[test]
    fn idempotency_replay_is_stable_but_conflicting_reuse_fails() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(temp.path())).unwrap();
        let first = store
            .remember_idempotency(
                &record.id,
                "same".to_string(),
                "turn-1".to_string(),
                "digest-a".to_string(),
            )
            .unwrap();
        assert_eq!(
            store
                .remember_idempotency(
                    &record.id,
                    "same".to_string(),
                    "turn-ignored".to_string(),
                    "digest-a".to_string()
                )
                .unwrap(),
            first
        );
        assert!(matches!(
            store.remember_idempotency(
                &record.id,
                "same".to_string(),
                "turn-2".to_string(),
                "digest-b".to_string()
            ),
            Err(ApiSessionStoreError::IdempotencyConflict { .. })
        ));
    }

    #[test]
    fn atomic_turn_claim_replays_after_restart_and_never_overlaps_a_generation() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(temp.path())).unwrap();
        let started = store
            .begin_idempotent_turn(
                &record.id,
                "turn-1".to_string(),
                Some("retry".to_string()),
                "body-a".to_string(),
            )
            .unwrap();
        assert!(matches!(started, BeginApiTurn::Started(ref turn) if turn.generation == 1));
        let restarted_store = ApiSessionStore::new(temp.path());
        assert_eq!(
            restarted_store
                .begin_idempotent_turn(
                    &record.id,
                    "turn-ignored".to_string(),
                    Some("retry".to_string()),
                    "body-a".to_string()
                )
                .unwrap(),
            BeginApiTurn::Replayed("turn-1".to_string())
        );
        assert_eq!(
            restarted_store
                .begin_idempotent_turn(&record.id, "turn-2".to_string(), None, "body-b".to_string())
                .unwrap(),
            BeginApiTurn::Busy
        );
        assert!(matches!(
            restarted_store.begin_idempotent_turn(
                &record.id,
                "turn-conflict".to_string(),
                Some("retry".to_string()),
                "body-b".to_string()
            ),
            Err(ApiSessionStoreError::IdempotencyConflict { .. })
        ));
    }

    #[test]
    fn interrupt_transition_precedes_signal_and_terminal_sessions_refuse_claims() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(temp.path())).unwrap();
        let turn = store.begin_turn(&record.id, "turn-1".to_string()).unwrap();
        store.begin_interrupt(&record.id, &turn.id).unwrap();
        assert_eq!(
            store.get(&record.id).unwrap().state,
            ApiSessionState::Interrupting
        );
        store
            .finish_turn(
                &record.id,
                &turn.id,
                ApiTurnState::Interrupted,
                Some("graceful_interrupt".to_string()),
            )
            .unwrap();
        store.terminate(&record.id).unwrap();
        assert_eq!(
            store
                .begin_idempotent_turn(&record.id, "turn-2".to_string(), None, "body".to_string())
                .unwrap(),
            BeginApiTurn::Terminated
        );
    }

    #[test]
    fn interrupt_disposition_wins_a_racing_waiter_without_touching_a_new_generation() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(temp.path())).unwrap();
        let first = store.begin_turn(&record.id, "turn-1".to_string()).unwrap();
        store.begin_interrupt(&record.id, &first.id).unwrap();
        store
            .finish_turn(
                &record.id,
                &first.id,
                ApiTurnState::Failed,
                Some("exit_130".to_string()),
            )
            .unwrap();
        store
            .finish_turn(
                &record.id,
                &first.id,
                ApiTurnState::Interrupted,
                Some("graceful_interrupt".to_string()),
            )
            .unwrap();
        let second = store.begin_turn(&record.id, "turn-2".to_string()).unwrap();
        let record = store.get(&record.id).unwrap();
        assert_eq!(record.turns[0].state, ApiTurnState::Interrupted);
        assert_eq!(
            record.turns[0].disposition.as_deref(),
            Some("graceful_interrupt")
        );
        assert_eq!(record.current_turn_id.as_deref(), Some(second.id.as_str()));
        assert_eq!(record.state, ApiSessionState::Running);
    }

    #[test]
    fn restart_marks_active_turn_failed_without_acting_on_stale_identity() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        let record = store.create(request(temp.path())).unwrap();
        let turn = store.begin_turn(&record.id, "turn-1".to_string()).unwrap();
        store
            .mutate(&record.id, |record| {
                record.turns[0].root_identity = Some(ProcessIdentity::new(999_999, 42));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store.reconcile_after_restart().unwrap(),
            vec![record.id.clone()]
        );
        let recovered = store.get(&record.id).unwrap();
        assert_eq!(recovered.state, ApiSessionState::Failed);
        assert_eq!(recovered.turns[0].state, ApiTurnState::Failed);
        assert_eq!(
            recovered.turns[0].root_identity,
            Some(ProcessIdentity::new(999_999, 42))
        );
        assert_eq!(turn.generation, 1);
    }

    #[test]
    fn corrupt_record_is_reported_and_can_be_quarantined_without_overwrite() {
        let temp = TempDir::new().unwrap();
        let path = api_session_path(temp.path(), "sess-bad");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        let store = ApiSessionStore::new(temp.path());
        assert!(matches!(
            store.get("sess-bad"),
            Err(ApiSessionStoreError::Corrupt { .. })
        ));
        let quarantined = store.quarantine_corrupt("sess-bad").unwrap().unwrap();
        assert!(quarantined.exists());
        assert!(!path.exists());
    }
}

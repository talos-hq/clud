//! Operator injection into a running daemon API session.
//!
//! Contract and per-harness matrix: `docs/architecture/session-inject.md`.
//! An injection is recorded on the durable session record before delivery
//! is attempted. It becomes a user turn: immediately when the session can
//! accept one, otherwise at the next turn boundary (drained by the
//! lifecycle's end-of-turn observer). `interrupt` replaces the running
//! turn instead of waiting.

use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::api_session_lifecycle::{ApiSessionLifecycle, LifecycleError, LifecycleReply};
use super::api_sessions::{
    ApiSessionRecord, ApiSessionState, ApiSessionStore, ApiSessionStoreError,
};
use super::types::unix_millis_now;
use crate::backend::HarnessSelection;
use crate::command::LaunchPlan;

/// Upper bound on how long the end-of-turn drain waits for the controller
/// to seal the turn whose process just exited.
const SEAL_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionState {
    Queued,
    Delivered,
    Failed,
}

impl InjectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }
}

/// One durable injection entry on the session record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiInjectionRecord {
    pub seq: u64,
    pub client_msg_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    #[serde(default)]
    pub interrupt: bool,
    pub state: InjectionState,
    pub submitted_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectRequest {
    pub message: String,
    pub client_msg_id: String,
    pub interrupt: bool,
    pub from: Option<String>,
    pub ticket: Option<String>,
}

/// The `{accepted, state, seq}` result every accepted injection gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InjectOutcome {
    pub accepted: bool,
    pub session_id: String,
    pub seq: u64,
    pub state: InjectionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectError {
    NotFound,
    /// The session is terminated; nothing can be delivered.
    Sealed,
    /// The session's harness cannot take daemon turns; carries its name.
    HarnessNotSupported(String),
    /// `client_msg_id` reused with a different message.
    Conflict,
    Invalid(&'static str),
    Store(String),
}

impl From<ApiSessionStoreError> for InjectError {
    fn from(error: ApiSessionStoreError) -> Self {
        match error {
            ApiSessionStoreError::NotFound(_) => Self::NotFound,
            other => Self::Store(other.to_string()),
        }
    }
}

/// Fail closed for a harness the daemon cannot run a headless turn for.
pub fn check_harness(record: &ApiSessionRecord) -> Result<(), InjectError> {
    match record
        .resolved_settings
        .harness
        .as_deref()
        .map(|raw| (raw, HarnessSelection::from_settings_str(raw)))
    {
        None
        | Some((
            _,
            Some(HarnessSelection::Default | HarnessSelection::Claude | HarnessSelection::Codex),
        )) => Ok(()),
        Some((raw, _)) => Err(InjectError::HarnessNotSupported(raw.to_string())),
    }
}

fn outcome(session_id: &str, entry: &ApiInjectionRecord, duplicate: bool) -> InjectOutcome {
    InjectOutcome {
        accepted: entry.state != InjectionState::Failed,
        session_id: session_id.to_string(),
        seq: entry.seq,
        state: entry.state,
        turn_id: entry.turn_id.clone(),
        error: entry.error.clone(),
        duplicate,
    }
}

impl ApiSessionStore {
    /// Durably record an injection (or find its earlier copy by
    /// `client_msg_id`). Returns the entry and whether it was a duplicate.
    pub fn record_injection(
        &self,
        session_id: &str,
        request: &InjectRequest,
    ) -> Result<(ApiInjectionRecord, bool), InjectError> {
        let mut result = Err(InjectError::NotFound);
        let mutated = self.mutate(session_id, |record| {
            result = record_injection_in(record, request);
            Ok(())
        });
        mutated?;
        let (entry, duplicate) = result?;
        if !duplicate {
            let _ = self.append_event(
                session_id,
                None,
                "injection_queued".to_string(),
                serde_json::json!({"seq": entry.seq, "client_msg_id": entry.client_msg_id,
                    "from": entry.from, "interrupt": entry.interrupt}),
            );
        }
        Ok((entry, duplicate))
    }

    /// Seal one injection as delivered (with its turn) or failed.
    pub fn settle_injection(
        &self,
        session_id: &str,
        seq: u64,
        settled: Result<String, String>,
    ) -> Result<ApiInjectionRecord, InjectError> {
        let entry = self.mutate(session_id, |record| {
            let Some(entry) = record.injections.iter_mut().find(|entry| entry.seq == seq) else {
                return Err(ApiSessionStoreError::NotFound(format!("injection {seq}")));
            };
            if entry.state == InjectionState::Queued {
                match &settled {
                    Ok(turn_id) => {
                        entry.state = InjectionState::Delivered;
                        entry.turn_id = Some(turn_id.clone());
                        entry.delivered_at_ms = Some(unix_millis_now());
                    }
                    Err(error) => {
                        entry.state = InjectionState::Failed;
                        entry.error = Some(error.clone());
                    }
                }
            }
            Ok(entry.clone())
        })?;
        let kind = match entry.state {
            InjectionState::Delivered => "injection_delivered",
            _ => "injection_failed",
        };
        let _ = self.append_event(
            session_id,
            entry.turn_id.clone(),
            kind.to_string(),
            serde_json::json!({"seq": entry.seq, "error": entry.error}),
        );
        Ok(entry)
    }
}

/// Pure state transition behind [`ApiSessionStore::record_injection`].
pub(super) fn record_injection_in(
    record: &mut ApiSessionRecord,
    request: &InjectRequest,
) -> Result<(ApiInjectionRecord, bool), InjectError> {
    if request.message.trim().is_empty() {
        return Err(InjectError::Invalid("message must not be empty"));
    }
    if request.client_msg_id.trim().is_empty() {
        return Err(InjectError::Invalid("client_msg_id must not be empty"));
    }
    if let Some(existing) = record
        .injections
        .iter()
        .find(|entry| entry.client_msg_id == request.client_msg_id)
    {
        return if existing.message == request.message {
            Ok((existing.clone(), true))
        } else {
            Err(InjectError::Conflict)
        };
    }
    if record.state == ApiSessionState::Terminated {
        return Err(InjectError::Sealed);
    }
    check_harness(record)?;
    let entry = ApiInjectionRecord {
        seq: record.injections.last().map_or(1, |last| last.seq + 1),
        client_msg_id: request.client_msg_id.clone(),
        message: request.message.clone(),
        from: request.from.clone(),
        ticket: request.ticket.clone(),
        interrupt: request.interrupt,
        state: InjectionState::Queued,
        submitted_at_ms: unix_millis_now(),
        delivered_at_ms: None,
        turn_id: None,
        error: None,
    };
    record.injections.push(entry.clone());
    Ok((entry, false))
}

/// The oldest injection still waiting for a turn.
pub(super) fn next_queued(record: &ApiSessionRecord) -> Option<&ApiInjectionRecord> {
    record
        .injections
        .iter()
        .find(|entry| entry.state == InjectionState::Queued)
}

/// Builds the resume (or first) turn plan for a message.
pub type PlanBuilder = fn(&ApiSessionRecord, String) -> Result<LaunchPlan, String>;

impl ApiSessionLifecycle {
    /// Accept an injection: record it, then deliver now (idle session or
    /// `interrupt`) or leave it queued for the end-of-turn drain.
    pub fn inject(
        &self,
        session_id: &str,
        request: InjectRequest,
        build: PlanBuilder,
    ) -> Result<InjectOutcome, InjectError> {
        let store = self.store();
        let (entry, duplicate) = store.record_injection(session_id, &request)?;
        if duplicate || entry.state != InjectionState::Queued {
            return Ok(outcome(session_id, &entry, duplicate));
        }
        let entry = if entry.interrupt {
            self.deliver(session_id, &entry, build, true)?
        } else {
            // Persisted before this read: if the running turn sealed in the
            // meantime, this call delivers; otherwise its drain will.
            let record = store.get(session_id)?;
            if record.state.accepts_turns() {
                self.deliver_next_queued(session_id, build);
            }
            current_entry(&store.get(session_id)?, entry.seq)
        };
        Ok(outcome(session_id, &entry, false))
    }

    /// Start a turn for the oldest queued injection, if the session is free.
    pub fn deliver_next_queued(&self, session_id: &str, build: PlanBuilder) {
        let Ok(record) = self.store().get(session_id) else {
            return;
        };
        if !record.state.accepts_turns() {
            return;
        }
        if let Some(entry) = next_queued(&record).cloned() {
            let _ = self.deliver(session_id, &entry, build, false);
        }
    }

    /// Called by the end-of-turn observer once `turn_id`'s process exited:
    /// wait for the controller's durable seal, then drain one injection.
    pub(super) fn drain_after_turn(&self, session_id: &str, turn_id: &str, build: PlanBuilder) {
        let started = Instant::now();
        loop {
            let Ok(record) = self.store().get(session_id) else {
                return;
            };
            if record
                .current_turn_id
                .as_deref()
                .is_some_and(|id| id != turn_id)
            {
                // A newer turn owns the session; its own observer drains.
                return;
            }
            if record.state.accepts_turns() {
                break;
            }
            if record.state == ApiSessionState::Terminated || started.elapsed() > SEAL_WAIT {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        self.deliver_next_queued(session_id, build);
    }

    fn deliver(
        &self,
        session_id: &str,
        entry: &ApiInjectionRecord,
        build: PlanBuilder,
        replace: bool,
    ) -> Result<ApiInjectionRecord, InjectError> {
        let store = self.store();
        let record = store.get(session_id)?;
        let plan = match build(&record, entry.message.clone()) {
            Ok(plan) => plan,
            Err(error) => return store.settle_injection(session_id, entry.seq, Err(error)),
        };
        let key = format!("inject-{}", entry.seq);
        let fingerprint = format!("inject:v1:{}:{}", entry.seq, entry.client_msg_id);
        let settled = match self.submit(session_id, plan, Some(key), fingerprint, replace) {
            Ok(LifecycleReply::Started { turn_id, .. } | LifecycleReply::Replayed { turn_id }) => {
                Ok(turn_id)
            }
            // Someone else's turn is running; its drain delivers this one.
            Ok(_) => return Ok(current_entry(&store.get(session_id)?, entry.seq)),
            Err(LifecycleError::Terminated) => Err("session terminated".to_string()),
            Err(error) => Err(format!("{error:?}")),
        };
        store.settle_injection(session_id, entry.seq, settled)
    }
}

#[derive(Deserialize)]
struct InjectDto {
    message: String,
    #[serde(default)]
    client_msg_id: Option<String>,
    #[serde(default)]
    interrupt: bool,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    ticket: Option<String>,
}

/// Bound on one injected message; matches the turn endpoint's limit.
const MAX_INJECT_BYTES: usize = 64 * 1024;

/// `POST /v1/sessions/{id}/inject`. Returns the request untouched for any
/// other route so the caller can continue dispatching.
pub(super) fn handle_http(
    mut request: tiny_http::Request,
    method: &tiny_http::Method,
    path: &str,
    lifecycle: &ApiSessionLifecycle,
) -> Option<tiny_http::Request> {
    use super::http::http_response::{read_body, respond_json};
    let id = path
        .strip_prefix("/v1/sessions/")
        .and_then(|rest| rest.strip_suffix("/inject"))
        .filter(|id| !id.is_empty() && !id.contains('/'));
    let (Some(id), tiny_http::Method::Post) = (id, method) else {
        return Some(request);
    };
    let reply = |request, status: u16, code: &str, message: &str| {
        let body = serde_json::json!({"code": code, "message": message});
        respond_json(request, status, body.to_string().as_bytes());
    };
    let Some(body) = read_body(&mut request)
        .ok()
        .and_then(|body| serde_json::from_slice::<InjectDto>(&body).ok())
    else {
        reply(
            request,
            400,
            "invalid_request",
            "expected message and optional client_msg_id, interrupt, from, ticket",
        );
        return None;
    };
    if body.message.len() > MAX_INJECT_BYTES {
        reply(
            request,
            400,
            "invalid_request",
            "message is larger than 64 KiB",
        );
        return None;
    }
    let client_msg_id = body
        .client_msg_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("auto-{}", super::types::unix_millis_now()));
    let inject = InjectRequest {
        message: body.message,
        client_msg_id,
        interrupt: body.interrupt,
        from: body.from,
        ticket: body.ticket,
    };
    match lifecycle.inject(id, inject, super::api_session_http::turn_plan) {
        Ok(out) => respond_json(request, 202, &serde_json::to_vec(&out).unwrap_or_default()),
        Err(InjectError::NotFound) => reply(request, 404, "not_found", "no daemon API session has this id; interactive PTY sessions and foreground `clud -p` runs cannot take injections"),
        Err(InjectError::Sealed) => reply(request, 409, "session_sealed", "session is terminated; injection rejected"),
        Err(InjectError::HarnessNotSupported(harness)) => reply(
            request,
            422,
            "harness_not_supported",
            &format!("harness '{harness}' cannot take injected turns; supported: claude, codex"),
        ),
        Err(InjectError::Conflict) => reply(request, 409, "injection_conflict", "client_msg_id was reused with a different message"),
        Err(InjectError::Invalid(message)) => reply(request, 400, "invalid_request", message),
        Err(InjectError::Store(message)) => reply(request, 500, "internal_error", &message),
    }
    None
}

fn current_entry(record: &ApiSessionRecord, seq: u64) -> ApiInjectionRecord {
    record
        .injections
        .iter()
        .find(|entry| entry.seq == seq)
        .cloned()
        .expect("recorded injections are never removed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::api_sessions::{
        ApiSessionBackend, CreateApiSession, ResolvedApiSessionSettings,
    };
    use tempfile::TempDir;

    fn session(temp: &TempDir, harness: Option<&str>) -> (ApiSessionStore, ApiSessionRecord) {
        let store = ApiSessionStore::new(temp.path());
        let record = store
            .create(CreateApiSession {
                backend: ApiSessionBackend::Claude,
                cwd: temp.path().to_path_buf(),
                name: None,
                resolved_settings: ResolvedApiSessionSettings {
                    model: None,
                    safe: false,
                    model_provider: None,
                    harness: harness.map(str::to_string),
                    routing_mode: None,
                },
            })
            .unwrap();
        (store, record)
    }

    fn request(id: &str, message: &str) -> InjectRequest {
        InjectRequest {
            message: message.to_string(),
            client_msg_id: id.to_string(),
            interrupt: false,
            from: Some("operator".to_string()),
            ticket: Some("T-1".to_string()),
        }
    }

    fn no_plan(_: &ApiSessionRecord, _: String) -> Result<LaunchPlan, String> {
        Err("no plan in unit tests".to_string())
    }

    #[test]
    fn same_client_msg_id_is_recorded_once() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        let (first, dup) = store
            .record_injection(&record.id, &request("m1", "hi"))
            .unwrap();
        assert!(!dup);
        let (again, dup) = store
            .record_injection(&record.id, &request("m1", "hi"))
            .unwrap();
        assert!(dup);
        assert_eq!(first.seq, again.seq);
        assert_eq!(store.get(&record.id).unwrap().injections.len(), 1);
    }

    #[test]
    fn reused_client_msg_id_with_new_text_conflicts() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store
            .record_injection(&record.id, &request("m1", "hi"))
            .unwrap();
        assert_eq!(
            store.record_injection(&record.id, &request("m1", "other")),
            Err(InjectError::Conflict)
        );
    }

    #[test]
    fn injection_into_running_turn_stays_queued_in_seq_order() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store.begin_turn(&record.id, "turn-1".to_string()).unwrap();
        let lifecycle = ApiSessionLifecycle::new(store.clone());
        let a = lifecycle
            .inject(&record.id, request("a", "first"), no_plan)
            .unwrap();
        let b = lifecycle
            .inject(&record.id, request("b", "second"), no_plan)
            .unwrap();
        assert_eq!(
            (a.state, a.seq, a.accepted),
            (InjectionState::Queued, 1, true)
        );
        assert_eq!((b.state, b.seq), (InjectionState::Queued, 2));
        let stored = store.get(&record.id).unwrap();
        assert_eq!(next_queued(&stored).unwrap().client_msg_id, "a");
        let entry = &stored.injections[0];
        assert_eq!(entry.from.as_deref(), Some("operator"));
        assert_eq!(entry.ticket.as_deref(), Some("T-1"));
    }

    #[test]
    fn queued_then_settled_moves_to_delivered_with_turn() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        let (entry, _) = store
            .record_injection(&record.id, &request("m", "x"))
            .unwrap();
        let settled = store
            .settle_injection(&record.id, entry.seq, Ok("turn-9".to_string()))
            .unwrap();
        assert_eq!(settled.state, InjectionState::Delivered);
        assert_eq!(settled.turn_id.as_deref(), Some("turn-9"));
        assert!(next_queued(&store.get(&record.id).unwrap()).is_none());
        // A late second settle cannot rewrite a delivered entry.
        let again = store
            .settle_injection(&record.id, entry.seq, Err("late".to_string()))
            .unwrap();
        assert_eq!(again.state, InjectionState::Delivered);
    }

    #[test]
    fn idle_session_delivery_failure_is_reported_not_dropped() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        let lifecycle = ApiSessionLifecycle::new(store.clone());
        let out = lifecycle
            .inject(&record.id, request("m", "x"), no_plan)
            .unwrap();
        assert_eq!(out.state, InjectionState::Failed);
        assert!(!out.accepted);
        assert_eq!(out.error.as_deref(), Some("no plan in unit tests"));
    }

    #[test]
    fn sealed_session_rejects_injection() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store.terminate(&record.id).unwrap();
        assert_eq!(
            store.record_injection(&record.id, &request("m", "x")),
            Err(InjectError::Sealed)
        );
        assert!(store.get(&record.id).unwrap().injections.is_empty());
    }

    #[test]
    fn unsupported_harness_is_rejected_by_name() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, Some("deepseek"));
        assert_eq!(
            store.record_injection(&record.id, &request("m", "x")),
            Err(InjectError::HarnessNotSupported("deepseek".to_string()))
        );
    }

    #[test]
    fn unknown_session_is_not_found() {
        let temp = TempDir::new().unwrap();
        let store = ApiSessionStore::new(temp.path());
        assert_eq!(
            store.record_injection("sess-missing", &request("m", "x")),
            Err(InjectError::NotFound)
        );
    }
}

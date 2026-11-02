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
    /// Claimed by one delivery attempt (#2523): the claim re-checks
    /// supersession in the same record mutation, so a directive replaced
    /// before this point is never submitted. Once claimed, a later directive
    /// only marks `superseded_by_seq`; the turn still starts and settles it.
    Delivering,
    Delivered,
    Failed,
    /// Terminal: a later directive replaced this one before it reached the
    /// agent, so it is never delivered (#2523). `superseded_by_seq` names
    /// the replacement; the entry stays in history.
    Superseded,
}

impl InjectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Delivering => "delivering",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }
}

/// What an injection means for the session's goal state (#2523).
///
/// A `Directive` supersedes every earlier directive: the session's current
/// directive set is the single newest directive not marked superseded (see
/// [`current_directive`]). A `Note` is informational and never supersedes.
/// Records written before `kind` existed deserialize as `Directive`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionKind {
    #[default]
    Directive,
    Note,
}

/// Hex sha256 of an injected message, recorded as provenance (#2523).
pub fn message_sha256(message: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(message.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    /// Absent on records written before #2523; such entries are directives.
    #[serde(default)]
    pub kind: InjectionKind,
    /// Hex sha256 of `message`; `None` on records written before #2523.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_sha256: Option<String>,
    /// Seq of the later directive that replaced this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by_seq: Option<u64>,
}

/// The session's current directive: the newest live directive (queued,
/// delivering or delivered) whose replacement, if any, is not itself live.
/// A `failed` or `superseded` directive is never in force, and a failed
/// replacement does not retire the directive it tried to replace (#2523).
/// Legacy records (no supersession marks) still resolve to their newest one.
pub fn current_directive(record: &ApiSessionRecord) -> Option<&ApiInjectionRecord> {
    let live = |entry: &ApiInjectionRecord| {
        entry.kind == InjectionKind::Directive
            && matches!(
                entry.state,
                InjectionState::Queued | InjectionState::Delivering | InjectionState::Delivered
            )
    };
    let replaced_by_live = |entry: &ApiInjectionRecord| {
        entry.superseded_by_seq.is_some_and(|by| {
            record
                .injections
                .iter()
                .any(|other| other.seq == by && live(other))
        })
    };
    record
        .injections
        .iter()
        .rev()
        .find(|entry| live(entry) && !replaced_by_live(entry))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectRequest {
    pub message: String,
    pub client_msg_id: String,
    pub interrupt: bool,
    pub from: Option<String>,
    pub ticket: Option<String>,
    pub kind: InjectionKind,
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
    /// `client_msg_id` reused with a different message or kind.
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
            self.announce_superseded(session_id, entry.seq);
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

    /// Log `injection_superseded` for every entry that `seq` replaced while
    /// it was still queued, so the replacement is observable in the stream.
    fn announce_superseded(&self, session_id: &str, seq: u64) {
        let Ok(record) = self.get(session_id) else {
            return;
        };
        for entry in record.injections.iter().filter(|entry| {
            entry.state == InjectionState::Superseded && entry.superseded_by_seq == Some(seq)
        }) {
            let written = self.append_event(
                session_id,
                None,
                "injection_superseded".to_string(),
                serde_json::json!({"seq": entry.seq, "superseded_by_seq": seq}),
            );
            if let Err(error) = written {
                self.note_unannounced(session_id, entry.seq, &error);
            }
        }
    }

    /// The event log could not take `injection_superseded`: say so on the
    /// entry itself, so the replacement is still visible on the record.
    fn note_unannounced(&self, session_id: &str, seq: u64, error: &ApiSessionStoreError) {
        let _ = self.mutate(session_id, |record| {
            if let Some(entry) = record.injections.iter_mut().find(|entry| entry.seq == seq) {
                entry.error = Some(format!("injection_superseded event not written: {error}"));
            }
            Ok(())
        });
    }

    /// Claim a queued injection for one delivery attempt (#2523). The check
    /// and the transition share one record mutation, so an entry superseded
    /// (or claimed by another drain) after the caller read it is refused.
    /// Returns the claimed entry, or `None` when it is no longer deliverable.
    pub fn claim_injection(
        &self,
        session_id: &str,
        seq: u64,
    ) -> Result<Option<ApiInjectionRecord>, InjectError> {
        Ok(self.mutate(session_id, |record| {
            Ok(record
                .injections
                .iter_mut()
                .find(|entry| entry.seq == seq)
                .and_then(|entry| claim_in(entry).then(|| entry.clone())))
        })?)
    }

    /// Return a claimed entry to the queue when no turn was started (another
    /// turn holds the session). A directive replaced while it was claimed
    /// becomes `superseded` instead, since it never reached the agent.
    fn release_claim(&self, session_id: &str, seq: u64) -> Result<ApiInjectionRecord, InjectError> {
        Ok(self.mutate(session_id, |record| {
            let Some(entry) = record.injections.iter_mut().find(|entry| entry.seq == seq) else {
                return Err(ApiSessionStoreError::NotFound(format!("injection {seq}")));
            };
            if entry.state == InjectionState::Delivering {
                entry.state = if entry.superseded_by_seq.is_some() {
                    InjectionState::Superseded
                } else {
                    InjectionState::Queued
                };
            }
            Ok(entry.clone())
        })?)
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
            settle_in(entry, &settled);
            Ok(entry.clone())
        })?;
        let kind = match (entry.state, entry.turn_id.is_some()) {
            (InjectionState::Delivered, _) | (InjectionState::Superseded, true) => {
                "injection_delivered"
            }
            (InjectionState::Superseded, false) => "injection_superseded",
            _ => "injection_failed",
        };
        let _ = self.append_event(
            session_id,
            entry.turn_id.clone(),
            kind.to_string(),
            serde_json::json!({"seq": entry.seq, "error": entry.error,
                "superseded_by_seq": entry.superseded_by_seq}),
        );
        Ok(entry)
    }
}

/// Pure claim check behind [`ApiSessionStore::claim_injection`]: only a
/// `queued` entry nobody replaced may move to `delivering`.
pub(super) fn claim_in(entry: &mut ApiInjectionRecord) -> bool {
    let claimable = entry.state == InjectionState::Queued && entry.superseded_by_seq.is_none();
    if claimable {
        entry.state = InjectionState::Delivering;
    }
    claimable
}

/// Pure settle transition. A started turn is always recorded (`turn_id`,
/// `delivered_at_ms`), even on an entry already marked superseded, so the
/// record shows both facts; `superseded_by_seq` is never cleared (#2523).
fn settle_in(entry: &mut ApiInjectionRecord, settled: &Result<String, String>) {
    match (entry.state, settled) {
        (InjectionState::Queued | InjectionState::Delivering, Ok(turn_id)) => {
            entry.state = InjectionState::Delivered;
            entry.turn_id = Some(turn_id.clone());
            entry.delivered_at_ms = Some(unix_millis_now());
        }
        (InjectionState::Superseded, Ok(turn_id)) if entry.turn_id.is_none() => {
            entry.turn_id = Some(turn_id.clone());
            entry.delivered_at_ms = Some(unix_millis_now());
        }
        (InjectionState::Queued | InjectionState::Delivering, Err(error)) => {
            entry.state = InjectionState::Failed;
            entry.error = Some(error.clone());
        }
        _ => {}
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
        return if existing.message == request.message && existing.kind == request.kind {
            Ok((existing.clone(), true))
        } else {
            Err(InjectError::Conflict)
        };
    }
    if record.state == ApiSessionState::Terminated {
        return Err(InjectError::Sealed);
    }
    check_harness(record)?;
    let seq = record.injections.last().map_or(1, |last| last.seq + 1);
    if request.kind == InjectionKind::Directive {
        for earlier in record.injections.iter_mut() {
            if earlier.kind == InjectionKind::Directive && earlier.superseded_by_seq.is_none() {
                earlier.superseded_by_seq = Some(seq);
                // Same mutation as the new entry: no window in which a
                // drain can pick the stale directive after it was replaced.
                if earlier.state == InjectionState::Queued {
                    earlier.state = InjectionState::Superseded;
                }
            }
        }
    }
    let entry = ApiInjectionRecord {
        seq,
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
        kind: request.kind,
        message_sha256: Some(message_sha256(&request.message)),
        superseded_by_seq: None,
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
        // Claim-then-submit: the lock covers only the claim, never the turn.
        let Some(entry) = store.claim_injection(session_id, entry.seq)? else {
            return Ok(current_entry(&store.get(session_id)?, entry.seq));
        };
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
            Ok(_) => return store.release_claim(session_id, entry.seq),
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
    #[serde(default)]
    kind: InjectionKind,
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
            "expected message and optional client_msg_id, interrupt, from, ticket, kind (directive|note)",
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
        kind: body.kind,
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
        Err(InjectError::Conflict) => reply(request, 409, "injection_conflict", "client_msg_id was reused with a different message or kind"),
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
                origin: crate::session_origin::SessionOrigin::Test,
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
            kind: InjectionKind::Directive,
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
        // Notes: two directives would supersede (see the test below).
        let note = |id: &str, message: &str| InjectRequest {
            kind: InjectionKind::Note,
            ..request(id, message)
        };
        let a = lifecycle
            .inject(&record.id, note("a", "first"), no_plan)
            .unwrap();
        let b = lifecycle
            .inject(&record.id, note("b", "second"), no_plan)
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

    #[test]
    fn kind_defaults_to_directive_and_hash_matches_message() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store
            .record_injection(&record.id, &request("k-1", "use v2"))
            .unwrap();
        let stored = store.get(&record.id).unwrap();
        let entry = &stored.injections[0];
        assert_eq!(entry.kind, InjectionKind::Directive);
        assert_eq!(
            entry.message_sha256.as_deref(),
            Some(message_sha256("use v2").as_str())
        );
        assert_eq!(
            message_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let json = serde_json::to_value(entry).unwrap();
        assert_eq!(json["kind"], "directive");
        let back: ApiInjectionRecord = serde_json::from_value(json).unwrap();
        assert_eq!(&back, entry);
    }

    #[test]
    fn second_directive_supersedes_first_and_both_stay_in_history() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store
            .record_injection(&record.id, &request("d-1", "use v1"))
            .unwrap();
        store
            .record_injection(&record.id, &request("d-2", "use v2"))
            .unwrap();
        let stored = store.get(&record.id).unwrap();
        assert_eq!(stored.injections.len(), 2);
        assert_eq!(stored.injections[0].superseded_by_seq, Some(2));
        assert_eq!(stored.injections[1].superseded_by_seq, None);
        assert_eq!(current_directive(&stored).unwrap().seq, 2);
    }

    #[test]
    fn note_never_supersedes_a_directive() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store
            .record_injection(&record.id, &request("d-1", "use v1"))
            .unwrap();
        let mut note = request("n-1", "fyi: CI is slow");
        note.kind = InjectionKind::Note;
        store.record_injection(&record.id, &note).unwrap();
        let stored = store.get(&record.id).unwrap();
        assert_eq!(stored.injections[0].superseded_by_seq, None);
        assert_eq!(stored.injections[1].kind, InjectionKind::Note);
        assert_eq!(current_directive(&stored).unwrap().seq, 1);
    }

    #[test]
    fn legacy_record_without_kind_or_hash_deserializes_as_directive() {
        let legacy = serde_json::json!({
            "seq": 1, "client_msg_id": "old", "message": "use v1",
            "interrupt": false, "state": "delivered", "submitted_at_ms": 5
        });
        let entry: ApiInjectionRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(entry.kind, InjectionKind::Directive);
        assert_eq!(entry.message_sha256, None);
        assert_eq!(entry.superseded_by_seq, None);
        let temp = TempDir::new().unwrap();
        let (_, mut record) = session(&temp, None);
        record.injections.push(entry);
        assert_eq!(current_directive(&record).unwrap().seq, 1);
        record_injection_in(&mut record, &request("new", "use v2")).unwrap();
        assert_eq!(record.injections[0].superseded_by_seq, Some(2));
        assert_eq!(current_directive(&record).unwrap().seq, 2);
    }

    #[test]
    fn superseded_queued_directive_is_never_delivered_to_the_turn() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store.begin_turn(&record.id, "turn-1".to_string()).unwrap();
        let lifecycle = ApiSessionLifecycle::new(store.clone());
        let stale = lifecycle
            .inject(&record.id, request("d-1", "use v1"), no_plan)
            .unwrap();
        assert_eq!(stale.state, InjectionState::Queued);
        lifecycle
            .inject(&record.id, request("d-2", "use v2"), no_plan)
            .unwrap();
        let stored = store.get(&record.id).unwrap();
        let first = &stored.injections[0];
        assert_eq!(first.state, InjectionState::Superseded);
        assert_eq!(first.superseded_by_seq, Some(2));
        assert_eq!(first.message, "use v1", "history stays readable");
        assert_eq!(first.turn_id, None);
        // The drain's choice: the replacement, never the stale directive.
        assert_eq!(next_queued(&stored).unwrap().client_msg_id, "d-2");
        let events = store.events_after(&record.id, 0, 1000).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == "injection_superseded"
                && event.data["seq"] == 1
                && event.data["superseded_by_seq"] == 2));
        // A late settle cannot resurrect it as delivered.
        let late = store
            .settle_injection(&record.id, 1, Ok("turn-2".to_string()))
            .unwrap();
        assert_eq!(late.state, InjectionState::Superseded);
    }

    #[test]
    fn queued_note_is_unaffected_by_a_later_directive() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store.begin_turn(&record.id, "turn-1".to_string()).unwrap();
        let lifecycle = ApiSessionLifecycle::new(store.clone());
        let mut note = request("n-1", "fyi: CI is slow");
        note.kind = InjectionKind::Note;
        lifecycle.inject(&record.id, note, no_plan).unwrap();
        lifecycle
            .inject(&record.id, request("d-1", "use v2"), no_plan)
            .unwrap();
        let stored = store.get(&record.id).unwrap();
        assert_eq!(stored.injections[0].state, InjectionState::Queued);
        assert_eq!(stored.injections[0].superseded_by_seq, None);
        assert_eq!(next_queued(&stored).unwrap().client_msg_id, "n-1");
    }

    fn must_not_build(_: &ApiSessionRecord, message: String) -> Result<LaunchPlan, String> {
        panic!("a superseded directive reached plan building: {message}");
    }

    #[test]
    fn directive_superseded_between_read_and_claim_is_not_submitted() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        let lifecycle = ApiSessionLifecycle::new(store.clone());
        // The drain's read: d-1 is the oldest queued entry.
        store
            .record_injection(&record.id, &request("d-1", "use v1"))
            .unwrap();
        let read = next_queued(&store.get(&record.id).unwrap())
            .cloned()
            .unwrap();
        assert_eq!(read.state, InjectionState::Queued);
        // The interleaved request replaces it before the drain claims.
        store
            .record_injection(&record.id, &request("d-2", "use v2"))
            .unwrap();
        // The drain proceeds with its stale copy; the claim refuses it.
        let after = lifecycle
            .deliver(&record.id, &read, must_not_build, false)
            .unwrap();
        assert_eq!(after.state, InjectionState::Superseded);
        assert_eq!(after.superseded_by_seq, Some(2));
        assert_eq!(after.turn_id, None);
        // A stale-but-still-queued copy (mark without state change) is refused too.
        let mut entry = read.clone();
        entry.superseded_by_seq = Some(2);
        assert!(!claim_in(&mut entry));
        assert_eq!(entry.state, InjectionState::Queued);
    }

    #[test]
    fn claim_is_exclusive_between_two_drains() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store
            .record_injection(&record.id, &request("d-1", "use v1"))
            .unwrap();
        let first = store.claim_injection(&record.id, 1).unwrap();
        assert_eq!(first.unwrap().state, InjectionState::Delivering);
        assert_eq!(store.claim_injection(&record.id, 1).unwrap(), None);
    }

    #[test]
    fn released_claim_of_a_replaced_directive_becomes_superseded() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store
            .record_injection(&record.id, &request("d-1", "use v1"))
            .unwrap();
        store.claim_injection(&record.id, 1).unwrap().unwrap();
        store
            .record_injection(&record.id, &request("d-2", "use v2"))
            .unwrap();
        let released = store.release_claim(&record.id, 1).unwrap();
        assert_eq!(released.state, InjectionState::Superseded);
        assert_eq!(released.superseded_by_seq, Some(2));
    }

    #[test]
    fn delivered_then_superseded_directive_keeps_turn_and_mark() {
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store
            .record_injection(&record.id, &request("d-1", "use v1"))
            .unwrap();
        store.claim_injection(&record.id, 1).unwrap().unwrap();
        // Replaced after the claim, while its turn is being submitted.
        store
            .record_injection(&record.id, &request("d-2", "use v2"))
            .unwrap();
        let settled = store
            .settle_injection(&record.id, 1, Ok("turn-7".to_string()))
            .unwrap();
        assert_eq!(settled.state, InjectionState::Delivered);
        assert_eq!(settled.turn_id.as_deref(), Some("turn-7"));
        assert_eq!(settled.superseded_by_seq, Some(2));
        assert!(settled.delivered_at_ms.is_some());
        // Even a superseded-state entry keeps a turn that really started.
        let late = store
            .settle_injection(&record.id, 2, Ok("turn-8".to_string()))
            .unwrap();
        assert_eq!(late.turn_id.as_deref(), Some("turn-8"));
        let mut entry = store.get(&record.id).unwrap().injections[0].clone();
        entry.state = InjectionState::Superseded;
        entry.turn_id = None;
        settle_in(&mut entry, &Ok("turn-9".to_string()));
        assert_eq!(
            (
                entry.state,
                entry.turn_id.as_deref(),
                entry.superseded_by_seq
            ),
            (InjectionState::Superseded, Some("turn-9"), Some(2))
        );
    }

    #[test]
    fn current_directive_skips_failed_and_returns_the_live_one() {
        let temp = TempDir::new().unwrap();
        let (_, mut record) = session(&temp, None);
        record_injection_in(&mut record, &request("d-1", "use v1")).unwrap();
        record.injections[0].state = InjectionState::Delivered;
        record_injection_in(&mut record, &request("d-2", "use v2")).unwrap();
        record.injections[1].state = InjectionState::Failed;
        // d-2 never reached the agent, so d-1 is still the one in force.
        assert_eq!(current_directive(&record).unwrap().seq, 1);
        // A queued predecessor replaced by a failed one: nothing in force.
        let (_, mut record) = session(&temp, None);
        record_injection_in(&mut record, &request("d-1", "use v1")).unwrap();
        record_injection_in(&mut record, &request("d-2", "use v2")).unwrap();
        assert_eq!(record.injections[0].state, InjectionState::Superseded);
        record.injections[1].state = InjectionState::Failed;
        assert_eq!(current_directive(&record), None);
    }

    #[test]
    fn conflict_message_names_message_and_kind() {
        let source = include_str!("api_session_inject.rs");
        let needle = concat!("reused with a different message", " or kind\"");
        assert!(source.contains(needle));
        let temp = TempDir::new().unwrap();
        let (store, record) = session(&temp, None);
        store
            .record_injection(&record.id, &request("m1", "hi"))
            .unwrap();
        let mut note = request("m1", "hi");
        note.kind = InjectionKind::Note;
        assert_eq!(
            store.record_injection(&record.id, &note),
            Err(InjectError::Conflict)
        );
    }
}

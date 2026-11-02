//! Serialized lifecycle controls for durable API sessions (#1043).
//! Legacy attach Ctrl-C and `DaemonRequest::Interrupt` do not use this module.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use running_process::NativeProcess;
use sysinfo::Signal;

use crate::command::LaunchPlan;

use super::activity::DaemonActivity;
use super::api_sessions::{ApiSessionState, ApiSessionStore, ApiTurnState, BeginApiTurn};
use super::api_turn_controller::launch_claimed_turn;
use super::process_utils::signal_process_tree_as;

pub const DEFAULT_INTERRUPT_GRACE: Duration = Duration::from_secs(5);
/// A terminal request must never inherit an unbounded process-table scan or
/// controller wait. The process may be slow to disappear, but the API caller
/// receives a durable terminal result within this bound.
pub const TERMINAL_KILL_TIMEOUT: Duration = Duration::from_secs(2);
pub const TREE_SIGNAL_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleReply {
    Started { generation: u64, turn_id: String },
    Replayed { turn_id: String },
    SessionBusy,
    Interrupted { forced: bool },
    Terminated,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    NotFound,
    Terminated,
    ResumeUnavailable,
    IdempotencyConflict,
    KillTimeout,
    Launch(String),
}

#[derive(Clone)]
pub struct ApiSessionLifecycle {
    store: ApiSessionStore,
    active: Arc<Mutex<HashMap<String, Arc<NativeProcess>>>>,
    /// Logical sessions do not contend with each other. The map is only held
    /// long enough to find/create a session's own mutation gate.
    gates: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    grace: Duration,
    activity: Option<DaemonActivity>,
}

impl ApiSessionLifecycle {
    pub fn new(store: ApiSessionStore) -> Self {
        Self {
            store,
            active: Arc::new(Mutex::new(HashMap::new())),
            gates: Arc::new(Mutex::new(HashMap::new())),
            grace: DEFAULT_INTERRUPT_GRACE,
            activity: None,
        }
    }
    pub(super) fn with_activity(store: ApiSessionStore, activity: DaemonActivity) -> Self {
        Self {
            activity: Some(activity),
            ..Self::new(store)
        }
    }
    pub(super) fn store(&self) -> ApiSessionStore {
        self.store.clone()
    }
    fn gate_for(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.gates
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
    #[expect(
        clippy::too_many_lines,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    pub fn submit(
        &self,
        session_id: &str,
        plan: LaunchPlan,
        key: Option<String>,
        fingerprint: String,
        replace: bool,
    ) -> Result<LifecycleReply, LifecycleError> {
        let gate = self.gate_for(session_id);
        let _gate = gate.lock().unwrap_or_else(|p| p.into_inner());
        let session = self
            .store
            .get(session_id)
            .map_err(|_| LifecycleError::NotFound)?;
        if session.state == ApiSessionState::Terminated {
            return Err(LifecycleError::Terminated);
        }
        if let Some(key) = key.as_ref() {
            if let Some(entry) = session.idempotency.iter().find(|entry| entry.key == *key) {
                return if entry.request_fingerprint == fingerprint {
                    Ok(LifecycleReply::Replayed {
                        turn_id: entry.turn_id.clone(),
                    })
                } else {
                    Err(LifecycleError::IdempotencyConflict)
                };
            }
        }
        // Keep this guard in a statement-local scope. In an `if let`
        // scrutinee Rust retains temporary values for the whole branch, so
        // restoring the handle below would re-lock `active` on the same
        // thread before returning SessionBusy.
        let active_process = {
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            active.remove(session_id)
        };
        if let Some(process) = active_process {
            // The active-map entry is the admission proof. Do not call into
            // `NativeProcess::returncode` while holding this session gate:
            // on a loaded Windows runner that probe can block the duplicate
            // request indefinitely. The observer owns stale-handle removal;
            // a just-completed handle may therefore produce one short busy
            // retry, never a synchronous liveness probe.
            if !replace
                && matches!(
                    session.state,
                    ApiSessionState::Running | ApiSessionState::Interrupting
                )
            {
                self.active
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(session_id.to_string(), process);
                return Ok(LifecycleReply::SessionBusy);
            }
            if replace {
                self.stop(session_id, &process);
            }
        } else if matches!(
            session.state,
            ApiSessionState::Running | ApiSessionState::Interrupting
        ) {
            return Ok(LifecycleReply::SessionBusy);
        }
        if session.generation > 0 && session.provider_session_id.is_none() {
            return Err(LifecycleError::ResumeUnavailable);
        }
        let turn_id = format!(
            "turn-{}-{}",
            session.generation.saturating_add(1),
            super::types::unix_millis_now()
        );
        let turn = match self
            .store
            .begin_idempotent_turn(session_id, turn_id, key, fingerprint)
        {
            Ok(BeginApiTurn::Started(turn)) => turn,
            Ok(BeginApiTurn::Replayed(turn_id)) => return Ok(LifecycleReply::Replayed { turn_id }),
            Ok(BeginApiTurn::Busy) => return Ok(LifecycleReply::SessionBusy),
            Ok(BeginApiTurn::Terminated) => return Err(LifecycleError::Terminated),
            Err(super::api_sessions::ApiSessionStoreError::IdempotencyConflict { .. }) => {
                return Err(LifecycleError::IdempotencyConflict)
            }
            Err(error) => return Err(LifecycleError::Launch(error.to_string())),
        };
        let process = match launch_claimed_turn(self.store.clone(), session_id, plan, turn.clone())
        {
            Ok(process) => process,
            Err(error) => {
                let _ = self.store.finish_turn(
                    session_id,
                    &turn.id,
                    ApiTurnState::Failed,
                    Some("launch_failed".to_string()),
                );
                return Err(LifecycleError::Launch(error.to_string()));
            }
        };
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session_id.to_string(), Arc::clone(&process));
        // The controller owns durable sealing. This observer only releases
        // transient ownership/idle state; pointer equality prevents an old
        // generation from removing a newer one for the same logical ID.
        let active = Arc::clone(&self.active);
        let observed = Arc::clone(&process);
        let observed_id = session_id.to_string();
        let activity = self.activity.as_ref().map(DaemonActivity::start_job);
        let lifecycle = self.clone();
        let observed_turn = turn.id.clone();
        thread::spawn(move || {
            let _activity = activity;
            while observed.returncode().is_none() {
                thread::sleep(Duration::from_millis(25));
            }
            {
                let mut active = active.lock().unwrap_or_else(|p| p.into_inner());
                if active
                    .get(&observed_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &observed))
                {
                    active.remove(&observed_id);
                }
            }
            // Turn boundary: hand the next queued operator injection, if
            // any, its turn (docs/architecture/session-inject.md).
            lifecycle.drain_after_turn(
                &observed_id,
                &observed_turn,
                super::api_session_http::turn_plan,
            );
        });
        Ok(LifecycleReply::Started {
            generation: turn.generation,
            turn_id: turn.id,
        })
    }
    pub fn interrupt(&self, session_id: &str) -> Result<LifecycleReply, LifecycleError> {
        let gate = self.gate_for(session_id);
        let _gate = gate.lock().unwrap_or_else(|p| p.into_inner());
        let process = self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(session_id)
            .ok_or(LifecycleError::NotFound)?;
        Ok(LifecycleReply::Interrupted {
            forced: self.stop(session_id, &process),
        })
    }
    pub fn kill(&self, session_id: &str) -> Result<LifecycleReply, LifecycleError> {
        let gate = self.gate_for(session_id);
        let _gate = gate.lock().unwrap_or_else(|p| p.into_inner());
        // Seal terminal intent before any OS signal. The controller waiter
        // may observe the resulting nonzero exit immediately, but its late
        // `finish_turn(exit_-9)` cannot overwrite this terminal record.
        let sealed_turn = self
            .store
            .begin_terminal_kill(session_id)
            .map_err(|_| LifecycleError::NotFound)?;
        let process = {
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            active.remove(session_id)
        };
        if let Some(process) = process {
            if let Some(identity) = sealed_turn.as_ref().and_then(|turn| turn.root_identity) {
                signal_tree_bounded(identity, Signal::Kill);
            }
            let root_kill_timed_out = kill_root_bounded(Arc::clone(&process));
            let started = Instant::now();
            while process.returncode().is_none() && started.elapsed() < TERMINAL_KILL_TIMEOUT {
                thread::sleep(Duration::from_millis(25));
            }
            let timed_out = root_kill_timed_out || process.returncode().is_none();
            if let Some(turn) = sealed_turn.as_ref() {
                let disposition = if timed_out {
                    "terminal_kill_timeout"
                } else {
                    "terminal_kill"
                };
                let _ = self
                    .store
                    .refine_terminal_kill(session_id, &turn.id, disposition);
            }
            return if timed_out {
                Err(LifecycleError::KillTimeout)
            } else {
                Ok(LifecycleReply::Terminated)
            };
        }
        if let Some(turn) = sealed_turn {
            let _ = self
                .store
                .refine_terminal_kill(session_id, &turn.id, "terminal_kill");
        }
        Ok(LifecycleReply::Terminated)
    }
    fn stop(&self, session_id: &str, process: &Arc<NativeProcess>) -> bool {
        let record = match self.store.get(session_id) {
            Ok(record) => record,
            Err(_) => return true,
        };
        let Some(turn_id) = record.current_turn_id.clone() else {
            return true;
        };
        let _ = self.store.begin_interrupt(session_id, &turn_id);
        let _ = self.store.append_event(
            session_id,
            Some(turn_id.clone()),
            "interrupt_requested".to_string(),
            serde_json::json!({"grace_ms": self.grace.as_millis()}),
        );
        if let Some(identity) = record
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .and_then(|turn| turn.root_identity)
        {
            signal_process_tree_as(&identity, Signal::Interrupt);
        }
        let started = Instant::now();
        while process.returncode().is_none() && started.elapsed() < self.grace {
            thread::sleep(Duration::from_millis(25));
        }
        let forced = process.returncode().is_none();
        if forced {
            if let Some(identity) = record
                .turns
                .iter()
                .find(|turn| turn.id == turn_id)
                .and_then(|turn| turn.root_identity)
            {
                signal_process_tree_as(&identity, Signal::Kill);
            }
            let _ = process.kill();
        }
        let disposition = if forced {
            "forced_interrupt"
        } else {
            "graceful_interrupt"
        };
        let _ = self.store.finish_turn(
            session_id,
            &turn_id,
            ApiTurnState::Interrupted,
            Some(disposition.to_string()),
        );
        forced
    }
}

/// `sysinfo` process enumeration is OS-owned and has no cancellation API.
/// Move it off the serialized API mutation path; callers continue with the
/// root-process kill even when tree enumeration exceeds its diagnostic budget.
fn signal_tree_bounded(identity: crate::process_identity::ProcessIdentity, signal: Signal) {
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        signal_process_tree_as(&identity, signal);
        let _ = sent.send(());
    });
    let _ = received.recv_timeout(TREE_SIGNAL_TIMEOUT);
}

fn kill_root_bounded(process: Arc<NativeProcess>) -> bool {
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = process.kill();
        let _ = sent.send(());
    });
    received.recv_timeout(TREE_SIGNAL_TIMEOUT).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn mutation_gates_are_shared_per_session_but_not_globally() {
        let temp = TempDir::new().unwrap();
        let lifecycle = ApiSessionLifecycle::new(ApiSessionStore::new(temp.path()));
        assert!(Arc::ptr_eq(
            &lifecycle.gate_for("one"),
            &lifecycle.gate_for("one")
        ));
        assert!(!Arc::ptr_eq(
            &lifecycle.gate_for("one"),
            &lifecycle.gate_for("two")
        ));
    }

    #[test]
    fn activity_enabled_lifecycle_keeps_the_daemon_job_tracker_available() {
        let temp = TempDir::new().unwrap();
        let activity = DaemonActivity::new(Instant::now());
        let lifecycle =
            ApiSessionLifecycle::with_activity(ApiSessionStore::new(temp.path()), activity.clone());
        assert!(lifecycle.activity.is_some());
        assert_eq!(activity.snapshot(Instant::now()).active_jobs, 0);
    }
}

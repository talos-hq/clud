//! Per-session flags the user sets and the command hook reads (#1984).
//!
//! A dictionary keyed by the harness `session_id`, then by flag name. A
//! `UserPromptSubmit` hook writes it; PreToolUse and `WorktreeCreate` hooks
//! read it. Subagent payloads carry the parent session's id, so one entry
//! covers every agent in a session.
//!
//! The requests ride the checkout-claim TCP lane: one JSON line out, one
//! back, tagged `session_flag_op` so the server can tell them apart. The map
//! is in memory only; a daemon restart forgets every flag, which leaves each
//! control in its default (refusing) state.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::io_helpers::read_json_file;
use super::paths::daemon_info_path;
use super::types::DaemonInfo;

const SESSION_FLAG_RPC_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "session_flag_op", rename_all = "snake_case")]
pub(super) enum SessionFlagRequest {
    Set {
        session_id: String,
        key: String,
        value: Value,
    },
    Get {
        session_id: String,
        key: String,
    },
    LoopPlanMode {
        session_id: String,
        event: crate::loop_plan_mode::Event,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "session_flag_op", rename_all = "snake_case")]
pub(super) enum SessionFlagReply {
    Set,
    Get { value: Option<Value> },
}

#[derive(Clone, Default)]
pub(super) struct SessionFlagRegistry {
    flags: Arc<Mutex<HashMap<String, HashMap<String, Value>>>>,
}

impl SessionFlagRegistry {
    pub(super) fn respond(&self, request: SessionFlagRequest) -> SessionFlagReply {
        let mut flags = self.flags.lock().expect("session flags poisoned");
        match request {
            SessionFlagRequest::LoopPlanMode { session_id, event } => {
                let session = flags.entry(session_id).or_default();
                let key = crate::loop_plan_mode::FLAG_KEY;
                if let Some(value) = crate::loop_plan_mode::transition(session.remove(key), event) {
                    session.insert(key.to_string(), value);
                }
                SessionFlagReply::Set
            }
            SessionFlagRequest::Set {
                session_id,
                key,
                value,
            } => {
                flags.entry(session_id).or_default().insert(key, value);
                SessionFlagReply::Set
            }
            SessionFlagRequest::Get { session_id, key } => SessionFlagReply::Get {
                value: flags
                    .get(&session_id)
                    .and_then(|session| session.get(&key))
                    .cloned(),
            },
        }
    }
}

/// Answer one session-flag line on `stream`.
pub(super) fn handle_session_flag_connection(
    stream: &mut TcpStream,
    flags: &SessionFlagRegistry,
    request: SessionFlagRequest,
) -> io::Result<()> {
    let reply = flags.respond(request);
    serde_json::to_writer(&mut *stream, &reply).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Set `key` for `session_id`. Connects to a running daemon only; never
/// starts one, so a hook cannot stall on daemon startup.
pub fn session_flag_set(
    state_dir: &Path,
    session_id: &str,
    key: &str,
    value: Value,
) -> io::Result<()> {
    match session_flag_rpc(
        state_dir,
        &SessionFlagRequest::Set {
            session_id: session_id.to_string(),
            key: key.to_string(),
            value,
        },
    )? {
        SessionFlagReply::Set => Ok(()),
        SessionFlagReply::Get { .. } => Err(unexpected_reply()),
    }
}

/// Read `key` for `session_id`; `Ok(None)` when it was never set. An error
/// means the daemon could not be asked, which a refusing control treats as
/// "not allowed".
pub fn session_flag_get(
    state_dir: &Path,
    session_id: &str,
    key: &str,
) -> io::Result<Option<Value>> {
    match session_flag_rpc(
        state_dir,
        &SessionFlagRequest::Get {
            session_id: session_id.to_string(),
            key: key.to_string(),
        },
    )? {
        SessionFlagReply::Get { value } => Ok(value),
        SessionFlagReply::Set => Err(unexpected_reply()),
    }
}

fn unexpected_reply() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "unexpected session flag reply")
}

/// Atomically apply a scheduling event; concurrent hooks never overwrite
/// another job's state with a read/modify/write from a stale snapshot.
pub fn session_loop_event(
    state_dir: &Path,
    session_id: &str,
    event: crate::loop_plan_mode::Event,
) -> io::Result<()> {
    match session_flag_rpc(
        state_dir,
        &SessionFlagRequest::LoopPlanMode {
            session_id: session_id.to_string(),
            event,
        },
    )? {
        SessionFlagReply::Set => Ok(()),
        SessionFlagReply::Get { .. } => Err(unexpected_reply()),
    }
}

fn session_flag_rpc(
    state_dir: &Path,
    request: &SessionFlagRequest,
) -> io::Result<SessionFlagReply> {
    let info = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir))?;
    let address = SocketAddr::from(([127, 0, 0, 1], info.port));
    let mut stream = TcpStream::connect_timeout(&address, SESSION_FLAG_RPC_TIMEOUT)?;
    stream.set_read_timeout(Some(SESSION_FLAG_RPC_TIMEOUT))?;
    stream.set_write_timeout(Some(SESSION_FLAG_RPC_TIMEOUT))?;
    serde_json::to_writer(&mut stream, request).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn get(registry: &SessionFlagRegistry, session: &str, key: &str) -> Option<Value> {
        match registry.respond(SessionFlagRequest::Get {
            session_id: session.to_string(),
            key: key.to_string(),
        }) {
            SessionFlagReply::Get { value } => value,
            SessionFlagReply::Set => panic!("a get answered as a set"),
        }
    }

    #[test]
    fn a_flag_belongs_to_one_session() {
        let registry = SessionFlagRegistry::default();
        registry.respond(SessionFlagRequest::Set {
            session_id: "a".into(),
            key: "worktrees".into(),
            value: json!(true),
        });
        assert_eq!(get(&registry, "a", "worktrees"), Some(json!(true)));
        assert_eq!(get(&registry, "b", "worktrees"), None);
        assert_eq!(get(&registry, "a", "other"), None);
    }

    #[test]
    fn concurrent_cron_creates_preserve_every_job() {
        let registry = SessionFlagRegistry::default();
        let workers: Vec<_> = (0..20)
            .map(|index| {
                let registry = registry.clone();
                std::thread::spawn(move || {
                    registry.respond(SessionFlagRequest::LoopPlanMode {
                        session_id: "loop".to_string(),
                        event: crate::loop_plan_mode::Event::Created(format!("job-{index}")),
                    })
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let state = get(&registry, "loop", crate::loop_plan_mode::FLAG_KEY).unwrap();
        assert_eq!(state["jobs"].as_array().unwrap().len(), 20);
        assert!(get(&registry, "other", crate::loop_plan_mode::FLAG_KEY).is_none());
    }

    #[test]
    fn requests_are_told_apart_from_checkout_requests() {
        let line = r#"{"session_flag_op":"get","session_id":"s","key":"worktrees"}"#;
        assert!(serde_json::from_str::<SessionFlagRequest>(line).is_ok());
        assert!(
            serde_json::from_str::<super::super::checkout_claims::CheckoutRequest>(line).is_err()
        );
        let checkout = r#"{"checkout_op":"who_all"}"#;
        assert!(serde_json::from_str::<SessionFlagRequest>(checkout).is_err());
    }

    /// A daemon serving only the session-flag lane, recorded in a fresh
    /// state dir the way the real daemon records itself (`daemon.json`).
    fn serve_session_flags() -> kernal_api::platform::fs::TemporaryDirectory {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        std::fs::write(
            super::super::paths::daemon_info_path(state.path()),
            json!({ "pid": std::process::id(), "port": port }).to_string(),
        )
        .unwrap();
        let registry = SessionFlagRegistry::default();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request = serde_json::from_str(&line).unwrap();
                handle_session_flag_connection(&mut stream, &registry, request).unwrap();
            }
        });
        state
    }

    fn prompt(state: &Path, session: &str, text: &str) {
        let payload = json!({ "session_id": session, "prompt": text }).to_string();
        assert_eq!(
            crate::worktree_consent::handle_prompt_in(state, &payload),
            0,
            "the prompt hook never blocks the prompt"
        );
    }

    #[test]
    fn a_session_whose_prompts_never_allowed_worktrees_is_refused() {
        let state = serve_session_flags();
        let dir = state.path();
        let refusal = |session| crate::worktree_consent::refusal_in(dir, Some(session));

        // No mention at all: refused.
        prompt(dir, "s1", "fix the login bug and open a PR");
        assert!(refusal("s1").unwrap().contains("has not allowed"));

        // A mention that says no, and one that is only about a branch.
        prompt(dir, "s1", "Just do one agent - no worktrees");
        prompt(
            dir,
            "s1",
            "Don't use the git work tree. Put it on a branch.",
        );
        assert!(refusal("s1").unwrap().contains("has not allowed"));

        // A yes allows that session only.
        prompt(dir, "s1", "ok, you can use worktrees for this one");
        assert_eq!(refusal("s1"), None);
        assert!(refusal("s2").unwrap().contains("has not allowed"));
    }

    #[test]
    fn a_later_refusal_revokes_the_sessions_permission() {
        let state = serve_session_flags();
        prompt(state.path(), "s1", "use worktrees");
        assert!(crate::worktree_consent::refusal_in(state.path(), Some("s1")).is_none());
        prompt(state.path(), "s1", "no worktrees");
        assert!(crate::worktree_consent::refusal_in(state.path(), Some("s1")).is_some());
    }

    #[test]
    fn a_session_is_refused_when_no_daemon_is_recorded() {
        let empty = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        prompt(empty.path(), "s1", "use worktrees");
        let refusal = crate::worktree_consent::refusal_in(empty.path(), Some("s1")).unwrap();
        assert!(refusal.contains("could not be asked"), "{refusal}");
    }
}

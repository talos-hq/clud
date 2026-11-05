//! Typed CLI-only adapter for daemon-managed Claude and Codex turns.
//!
//! Lifecycle code owns persistence and output draining. This module owns only
//! canonical plan construction and interpretation of individual JSONL lines.

use serde_json::Value;

use crate::args::Args;
use crate::backend::{Backend, ResolvedLaunchTarget};
use crate::command::{build_headless_turn_plan, HeadlessTurnRequest, LaunchPlan};

/// Build a captured initial or resumed provider turn. The request surface has
/// no raw argv or environment fields.
#[allow(dead_code)] // Consumed by the logical-session controller in #1038.
pub(super) fn build_turn_plan(
    args: &Args,
    target: ResolvedLaunchTarget,
    backend_path: &str,
    request: &HeadlessTurnRequest,
) -> Result<LaunchPlan, String> {
    build_headless_turn_plan(args, target, backend_path, request)
}

/// A normalized backend JSONL record for the future session controller.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // The controller lands in the following lifecycle slice.
pub(super) enum BackendEvent {
    ProviderSessionId(String),
    Opaque(Value),
    Malformed { line: String, error: String },
}

/// Parse one JSONL record without treating future provider events as errors.
#[allow(dead_code)] // The worker output observer lands in the following slice.
pub(super) fn parse_backend_event(backend: Backend, line: &str) -> BackendEvent {
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) => value,
        Err(error) => {
            return BackendEvent::Malformed {
                line: line.to_string(),
                error: error.to_string(),
            }
        }
    };
    let identity = match backend {
        Backend::Claude => value
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| *kind == "system")
            .and_then(|_| value.get("subtype").and_then(Value::as_str))
            .filter(|kind| *kind == "init")
            .and_then(|_| value.get("session_id").and_then(Value::as_str)),
        Backend::Codex => value
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| *kind == "thread.started")
            .and_then(|_| value.get("thread_id").and_then(Value::as_str)),
        Backend::DeepSeek => None,
        Backend::OpenCode => value.get("sessionID").and_then(Value::as_str),
    };
    identity
        .filter(|id| !id.is_empty())
        .map(|id| BackendEvent::ProviderSessionId(id.to_string()))
        .unwrap_or(BackendEvent::Opaque(value))
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::backend::{HarnessSelection, PreferenceSource, RoutingMode};
    use crate::command::HeadlessSession;

    fn args(raw: &[&str]) -> Args {
        Args::try_parse_from(raw).unwrap()
    }

    fn target(backend: Backend) -> ResolvedLaunchTarget {
        ResolvedLaunchTarget {
            routing_mode: RoutingMode::Direct,
            model_provider: backend.as_model_provider(),
            requested_harness: HarnessSelection::Default,
            effective_harness: backend,
            provider_source: PreferenceSource::BuiltInDefault,
            harness_source: PreferenceSource::BuiltInDefault,
        }
    }

    fn request(session: HeadlessSession) -> HeadlessTurnRequest {
        HeadlessTurnRequest {
            message: "hello".to_string(),
            cwd: std::env::current_dir().unwrap(),
            session,
        }
    }

    fn command_without_deletion_policy(plan: &LaunchPlan) -> Vec<String> {
        let mut command = plan.command.clone();
        if command[0] == "claude" {
            assert_eq!(command[1], "--append-system-prompt");
            assert!(command[2].contains(&crate::deletion_rules::generated().instructions));
            command.drain(1..3);
        } else {
            for prefix in [
                "hooks.PreToolUse=",
                "hooks.state=",
                "developer_instructions=",
            ] {
                let index = command
                    .iter()
                    .position(|arg| arg.starts_with(prefix))
                    .unwrap();
                assert_eq!(command[index - 1], "-c");
                assert!(index > command.iter().position(|arg| arg == "exec").unwrap());
                command.drain(index - 1..index + 1);
            }
        }
        command
    }

    #[test]
    fn claude_initial_and_resume_use_stream_json_identity_argv() {
        let initial = build_turn_plan(
            &args(&["clud", "--safe", "--model", "claude-test"]),
            target(Backend::Claude),
            "claude",
            &request(HeadlessSession::Initial {
                claude_session_id: Some("claude-uuid".to_string()),
            }),
        )
        .unwrap();
        assert_eq!(
            command_without_deletion_policy(&initial),
            [
                "claude",
                "--model",
                "claude-test",
                "--output-format",
                "stream-json",
                "--verbose",
                "--session-id",
                "claude-uuid",
                "-p",
                "hello"
            ]
        );
        assert_eq!(initial.launch_mode, crate::backend::LaunchMode::Subprocess);
        assert!(!initial.stream_json_progress);

        let resumed = build_turn_plan(
            &args(&["clud", "--safe"]),
            target(Backend::Claude),
            "claude",
            &request(HeadlessSession::Resume {
                provider_session_id: "claude-uuid".to_string(),
            }),
        )
        .unwrap();
        assert!(resumed
            .command
            .windows(3)
            .any(|window| window == ["--resume", "claude-uuid", "-p"]));
    }

    #[test]
    fn codex_initial_and_resume_use_exec_json() {
        let initial = build_turn_plan(
            &args(&["clud", "--codex", "--safe", "--model", "gpt-test"]),
            target(Backend::Codex),
            "codex",
            &request(HeadlessSession::Initial {
                claude_session_id: None,
            }),
        )
        .unwrap();
        assert_eq!(
            command_without_deletion_policy(&initial),
            [
                "codex",
                "-c",
                r#"approvals_reviewer="auto_review""#,
                "exec",
                "--json",
                "-m",
                "gpt-test",
                "hello"
            ]
        );
        let resumed = build_turn_plan(
            // This is the CLI-shaped resume input the HTTP/lifecycle slice
            // will translate into a typed request; the adapter must not fall
            // back to Codex's interactive `resume` argv.
            &args(&[
                "clud",
                "--codex",
                "--safe",
                "-r",
                "thread-123",
                "-p",
                "hello",
            ]),
            target(Backend::Codex),
            "codex",
            &request(HeadlessSession::Resume {
                provider_session_id: "thread-123".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(
            command_without_deletion_policy(&resumed),
            [
                "codex",
                "-c",
                r#"approvals_reviewer="auto_review""#,
                "exec",
                "resume",
                "--json",
                "thread-123",
                "hello"
            ]
        );
    }

    #[test]
    fn parser_extracts_ids_and_keeps_unknown_and_malformed_records() {
        assert_eq!(
            parse_backend_event(
                Backend::Claude,
                r#"{"type":"system","subtype":"init","session_id":"claude-uuid"}"#
            ),
            BackendEvent::ProviderSessionId("claude-uuid".to_string())
        );
        assert_eq!(
            parse_backend_event(
                Backend::Codex,
                r#"{"type":"thread.started","thread_id":"thread-123"}"#
            ),
            BackendEvent::ProviderSessionId("thread-123".to_string())
        );
        assert!(matches!(
            parse_backend_event(Backend::Codex, r#"{"type":"future.event","data":1}"#),
            BackendEvent::Opaque(_)
        ));
        assert!(matches!(
            parse_backend_event(Backend::Claude, "not json"),
            BackendEvent::Malformed { .. }
        ));
    }

    #[test]
    fn headless_request_rejects_relative_cwd_and_missing_identity() {
        let request = HeadlessTurnRequest {
            message: "hello".to_string(),
            cwd: "relative".into(),
            session: HeadlessSession::Initial {
                claude_session_id: None,
            },
        };
        assert!(build_turn_plan(
            &args(&["clud"]),
            target(Backend::Claude),
            "claude",
            &request
        )
        .is_err());
    }
}

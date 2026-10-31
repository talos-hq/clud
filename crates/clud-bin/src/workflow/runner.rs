//! Real agent runner: every `agent()` call becomes one headless
//! `clud -p <prompt> --output-format stream-json --verbose` child, started
//! through `running-process` and drained concurrently with the rest of its
//! batch. A `schema` is forwarded as `--json-schema`; the harness then
//! reports the validated value as `structured_output` on its `result` event.

use std::path::{Path, PathBuf};
use std::time::Duration;

use running_process::ReadStatus;
use serde_json::Value;

use super::engine::AgentRequest;
use crate::subprocess::ManagedSubprocess;

/// The argv for one agent call (exposed for tests and `--dry-run`).
pub fn agent_argv(clud_exe: &Path, req: &AgentRequest) -> Vec<String> {
    let mut argv = vec![
        clud_exe.to_string_lossy().into_owned(),
        "-p".into(),
        req.prompt.clone(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    if let Some(model) = &req.model {
        argv.push("--model".into());
        argv.push(model.clone());
    }
    if let Some(schema) = &req.schema {
        argv.push("--json-schema".into());
        argv.push(schema.to_string());
    }
    argv
}

/// Turn one child's captured stream-json into the `agent()` result: the
/// `structured_output` when a schema was requested, otherwise the final
/// `result` text. The last `result` event wins.
pub fn parse_agent_result(raw: &str, want_schema: bool, exit_code: i32) -> Result<Value, String> {
    let result = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .rfind(|v| v.get("type").and_then(Value::as_str) == Some("result"));
    let Some(result) = result else {
        return Err(format!("agent exited {exit_code} without a result event"));
    };
    if result.get("is_error").and_then(Value::as_bool) == Some(true) {
        let text = result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("agent reported an error");
        return Err(text.to_string());
    }
    if want_schema {
        return result
            .get("structured_output")
            .cloned()
            .filter(|v| !v.is_null())
            .ok_or_else(|| "agent returned no structured_output for its schema".to_string());
    }
    Ok(Value::String(
        result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    ))
}

struct Child {
    process: ManagedSubprocess,
    raw: String,
    done: bool,
}

/// Run a batch concurrently and return one result per request, in order.
/// `on_line` sees every raw stdout line tagged with its request id, so a
/// caller can render live progress.
pub fn run_batch(
    clud_exe: &Path,
    cwd: Option<PathBuf>,
    batch: &[AgentRequest],
    on_line: &mut dyn FnMut(u32, &str),
) -> Vec<Result<Value, String>> {
    let mut children: Vec<Result<Child, String>> = batch
        .iter()
        .map(|req| {
            ManagedSubprocess::start_inheriting_env(
                agent_argv(clud_exe, req),
                cwd.clone(),
                true,
                None,
            )
            .map(|process| Child {
                process,
                raw: String::new(),
                done: false,
            })
        })
        .collect();
    let slice = Duration::from_millis(20);
    while children
        .iter()
        .any(|c| matches!(c, Ok(child) if !child.done))
    {
        for (req, slot) in batch.iter().zip(children.iter_mut()) {
            let Ok(child) = slot else { continue };
            if child.done {
                continue;
            }
            match child.process.read_stdout(Some(slice)) {
                ReadStatus::Line(bytes) => {
                    let line = String::from_utf8_lossy(&bytes);
                    let line = line.trim_end_matches(['\r', '\n']);
                    on_line(req.id, line);
                    child.raw.push_str(line);
                    child.raw.push('\n');
                }
                ReadStatus::Timeout => {
                    let _ = child.process.poll();
                }
                ReadStatus::Eof => child.done = true,
            }
        }
    }
    batch
        .iter()
        .zip(children)
        .map(|(req, slot)| {
            let child = slot?;
            let code = child
                .process
                .wait(Some(Duration::from_secs(5)))
                .unwrap_or_else(|_| child.process.returncode().unwrap_or(-1));
            parse_agent_result(&child.raw, req.schema.is_some(), code)
        })
        .collect()
}

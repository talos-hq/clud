//! Registers clud's own MCP bridge (`python -m clud.mcp_server`) with a
//! Claude Code launch, so the agent can start Codex (or Claude) sub-agents
//! through the bridge's `run` / `session_*` tools with `backend="codex"`.
//!
//! The bridge ships in the wheel next to the `clud` binary, so the venv's
//! `python` beside the running executable is the interpreter that can import
//! it. A dev build under `target/` has no such sibling and registers nothing.

use std::path::{Path, PathBuf};

/// MCP server name the agent sees its tools under (`mcp__clud__*`).
const SERVER_NAME: &str = "clud";

/// Appended to the system prompt of every launch that gets the bridge, so the
/// agent polls clud's bulletin board for instructions from clud and Hermes.
pub(super) const BOARD_DIRECTIVE: &str = "You are connected to clud's agent \
bulletin board through the `clud` MCP server. clud, Hermes and other agents \
use it to message and steer you. Call `mcp__clud__board_read` when you start, \
between major steps, and before you finish; it returns only new messages. \
Treat a message as a request from the operator that launched you: follow it \
when it fits your task and permissions, and reply or report progress with \
`mcp__clud__board_post` (address `all` or the sender's id). `board_whoami` \
gives your own id.";

/// The `--mcp-config=<json>` token for `exe_dir`, or `None` when the bridge's
/// interpreter is not installed there.
///
/// One `=`-bound token on purpose: Claude declares `--mcp-config <configs...>`
/// as variadic, so the space-separated form would swallow a later `-p` prompt.
pub(super) fn claude_mcp_config_arg(exe_dir: &Path) -> Option<String> {
    let python = bridge_python(exe_dir)?;
    let config = serde_json::json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "stdio",
                "command": python.to_string_lossy(),
                "args": ["-m", "clud.mcp_server"],
            }
        }
    });
    Some(format!("--mcp-config={config}"))
}

/// Why the agent will not see `mcp__clud__*` tools, with the fix, or `None`
/// when the bridge interpreter is installed beside `exe_dir` (#2521).
pub(super) fn bridge_missing_notice(exe_dir: &Path) -> Option<String> {
    if bridge_python(exe_dir).is_some() {
        return None;
    }
    Some(format!(
        "[clud] note: the clud MCP server is not connected for this launch (no bridge \
interpreter beside {}), so `mcp__clud__*` tools such as session_start and board_read are \
absent. Fix: run clud from its installed wheel (`uv tool install clud`), not a bare \
dev binary.",
        exe_dir.display()
    ))
}

/// The directory of the running `clud`, resolved through the launcher symlink
/// a `uv tool install` leaves on `PATH`.
pub(super) fn current_exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

fn bridge_python(exe_dir: &Path) -> Option<PathBuf> {
    let name = if crate::platform::host::WINDOWS {
        "python.exe"
    } else {
        "python"
    };
    let python = exe_dir.join(name);
    python.is_file().then_some(python)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_bridge_explains_how_to_connect_it() {
        // #2521: an absent `mcp__clud__*` tool used to fail with no hint.
        let dir = tempfile::tempdir().unwrap();
        let notice = bridge_missing_notice(dir.path()).expect("notice when absent");
        assert!(
            notice.contains("clud MCP server is not connected"),
            "{notice}"
        );
        assert!(notice.contains("uv tool install clud"), "{notice}");
    }

    #[test]
    fn no_sibling_python_registers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(claude_mcp_config_arg(dir.path()), None);
    }

    #[test]
    fn sibling_python_registers_the_bridge_as_one_token() {
        let dir = tempfile::tempdir().unwrap();
        let name = if crate::platform::host::WINDOWS {
            "python.exe"
        } else {
            "python"
        };
        std::fs::write(dir.path().join(name), b"").unwrap();
        let arg = claude_mcp_config_arg(dir.path()).unwrap();
        let json = arg.strip_prefix("--mcp-config=").unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let server = &value["mcpServers"]["clud"];
        assert_eq!(server["args"], serde_json::json!(["-m", "clud.mcp_server"]));
        assert_eq!(
            server["command"].as_str().unwrap(),
            dir.path().join(name).to_string_lossy()
        );
    }
}

//! Computer use, built in: the pinned Talos HQ `cua-driver` registered as an
//! MCP server (`mcp__cua__*`) on clud's launches, provisioned on first use.
//!
//! - [`pin`] — the one pinned release, per host target, with digests.
//! - [`provision`] — find (override or managed copy) or install the driver.
//! - [`session_env`] — the driver's environment: permission mode, privacy,
//!   credential scrubbing, Linux display repairs.
//! - [`serve`] — `clud computer-use {status,install,doctor,grant,mcp}`.
//!
//! Contract: [docs/architecture/computer-use.md](../../../../docs/architecture/computer-use.md),
//! rationale: DD-233.

pub mod pin;
pub mod provision;
pub mod serve;
pub mod session_env;
mod uv_archive;

#[cfg(test)]
mod tests;

use std::path::Path;

pub use session_env::PermissionMode;

/// The MCP server name; the agent sees `mcp__cua__<tool>`. Not
/// `computer-use`, which Claude's desktop app uses for its own server.
pub const SERVER_NAME: &str = "cua";

/// Claude Code permission rule allowing every tool of the server.
pub const ALLOW_RULE: &str = "mcp__cua";

/// The `clud` argv that serves the driver over stdio.
pub const MCP_ARGS: [&str; 2] = ["computer-use", "mcp"];

/// Appended to the system prompt of a launch that registers the server.
pub const DIRECTIVE: &str = "Computer use is available: the `cua` MCP server \
(`mcp__cua__*` tools) drives this machine's desktop \
— windows, screenshots, clicks, typing, real browsers. Use the `clud-computer-use` \
skill before the first desktop action; it covers setup, platform limits and \
safety rules.";

/// `clud computer-use` actions.
#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Show the pinned driver, where it resolves, and what this desktop supports.
    Status {
        /// Emit stable JSON.
        #[arg(long = "json")]
        json: bool,
    },
    /// Provision and verify the pinned Computer Use driver (normally done on first use).
    Install,
    /// Run cua-driver's own diagnostics with clud's environment.
    Doctor,
    /// macOS: request Accessibility and Screen Recording for ComputerUse once.
    Grant,
    /// The stdio MCP server Claude Code and Codex start.
    #[command(hide = true)]
    Mcp,
}

pub fn run(action: &Action) -> i32 {
    match action {
        Action::Status { json } => serve::status(*json),
        Action::Install => serve::install(),
        Action::Doctor => serve::doctor(),
        Action::Grant => serve::grant(),
        Action::Mcp => serve::mcp(),
    }
}

/// Which launches register the server. Claude Code defers MCP tools behind
/// tool search, so the driver's ~220 KB of tool schemas cost nothing until
/// used; that is certain only against Anthropic's own API, so a first-party
/// Claude launch gets the server by default (`default_on`). A gateway route
/// may reject tool search and load every schema on every request, and Codex
/// loads them all, so those get it only on request (`--computer-use`).
pub fn registered_for(default_on: bool, forced: bool, disabled: bool) -> bool {
    !disabled && (default_on || forced)
}

fn server_entry(exe: &Path, mode: PermissionMode) -> serde_json::Value {
    serde_json::json!({
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": MCP_ARGS,
        "env": { session_env::MODE_ENV: mode.as_str() },
        // Keep all ~65 tools behind tool search, even any the driver marks
        // to load upfront.
        "alwaysLoad": false,
    })
}

/// The Claude `--mcp-config=<json>` token: one `=`-bound token, because the
/// flag is variadic and would swallow a later `-p` prompt.
pub fn claude_mcp_config_arg(exe: &Path, mode: PermissionMode) -> String {
    let config = serde_json::json!({ "mcpServers": { SERVER_NAME: server_entry(exe, mode) } });
    format!("--mcp-config={config}")
}

/// The Claude `--allowedTools=<rule>` token, so no cua call ever prompts,
/// including under `--safe`.
pub fn claude_allowed_tools_arg() -> String {
    format!("--allowedTools={ALLOW_RULE}")
}

/// Codex `-c` overrides registering the same server.
pub fn codex_config_overrides(exe: &Path, mode: PermissionMode) -> Vec<String> {
    let exe = toml::Value::String(exe.to_string_lossy().into_owned());
    let prefix = format!("mcp_servers.{SERVER_NAME}");
    vec![
        format!("{prefix}.command={exe}"),
        format!("{prefix}.args=[\"{}\",\"{}\"]", MCP_ARGS[0], MCP_ARGS[1]),
        format!(
            "{prefix}.env={{{}=\"{}\"}}",
            session_env::MODE_ENV,
            mode.as_str()
        ),
        // First use provisions the pinned driver before the server answers.
        format!("{prefix}.startup_timeout_sec=180"),
        // Codex otherwise asks before each MCP tool call.
        format!("{prefix}.default_tools_approval_mode=\"approve\""),
    ]
}

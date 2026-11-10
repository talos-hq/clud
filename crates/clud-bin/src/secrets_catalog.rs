//! `--allow-secrets`: registers lokr's MCP server with a Claude session, so the
//! agent can see which credentials exist. No route returns a value.

use std::ffi::OsString;
use std::path::Path;

/// Appended to the system prompt of an `--allow-secrets` launch.
pub const DIRECTIVE: &str = "This session can see which credentials exist on \
this machine through the `lokr` MCP server. Before concluding that a key is \
missing, call `mcp__lokr__secret_status` with its name (for example \
`openrouter`), or `mcp__lokr__list_secrets` to browse. Results give names, \
locations and how to use each one, never values. If a needed key is absent, \
call `mcp__lokr__request_secret`, which asks the user in a masked prompt and \
returns only stored, failed or cancelled. Never print a credential. Ask the \
user before reusing a key held by another program.";

/// The `--mcp-config=<json>` token for the running `clud`.
pub fn claude_mcp_config_arg() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(config_arg(&exe))
}

fn config_arg(exe: &Path) -> String {
    let config = serde_json::json!({ "mcpServers": { "lokr": {
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": ["secrets-mcp"],
    }}});
    format!("--mcp-config={config}")
}

fn options(exe: OsString) -> lokr::ServerOptions {
    lokr::ServerOptions {
        daemon_command: vec![exe.clone(), "secrets-daemon".into()],
        // lokr validates structured context/sinks and appends their CLI flags.
        ask_command: vec![exe, "secret".into()],
    }
}

/// Runs the MCP server on stdio.
pub fn serve() -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("clud secrets-mcp: cannot locate clud");
        return 1;
    };
    match lokr::serve_stdio(&options(exe.into_os_string())) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("clud secrets-mcp: {error}");
            1
        }
    }
}

/// Runs the background scanner.
pub fn daemon() -> i32 {
    match lokr::run_daemon() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("clud secrets-daemon: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_only_the_lokr_server_as_one_token() {
        let arg = config_arg(Path::new("/opt/clud"));
        let json = arg.strip_prefix("--mcp-config=").unwrap();
        let config: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(config["mcpServers"]["lokr"]["command"], "/opt/clud");
        assert_eq!(config["mcpServers"]["lokr"]["args"][0], "secrets-mcp");
    }

    #[test]
    fn prompt_accepts_lokr_structured_request_flags() {
        let built = options("clud".into());
        assert_eq!(built.daemon_command[1], "secrets-daemon");
        assert_eq!(
            built.ask_command,
            vec![OsString::from("clud"), "secret".into()]
        );
    }

    #[test]
    fn directive_names_every_route() {
        for route in ["secret_status", "list_secrets", "request_secret"] {
            assert!(DIRECTIVE.contains(&format!("mcp__lokr__{route}")));
        }
    }
}

//! Registers clud's own MCP bridge (`python -m clud.mcp_server`) with a
//! Claude Code launch, so the agent can start Codex (or Claude) sub-agents
//! through the bridge's `run` / `session_*` tools with `backend="codex"`.
//!
//! A wheel install ships the bridge next to the `clud` binary, so the venv's
//! `python` beside the running executable can import it. A native install
//! (`install.sh`) has no such sibling: it runs the sources embedded in this
//! binary through `uv`, and registers nothing only when `uv` is absent too.

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

/// The `mcp` requirement of a native launch; `pyproject.toml` owns the wheel's
/// copy (a test holds the two together).
const BRIDGE_MCP_REQUIREMENT: &str = "mcp>=2.1";

/// The bridge's Python package, embedded so a native binary with no wheel can
/// still run it. `src/clud/` stays the one source of these files.
const BRIDGE_SOURCES: [(&str, &str); 4] = [
    (
        "__init__.py",
        include_str!("../../../../src/clud/__init__.py"),
    ),
    (
        "mcp_server.py",
        include_str!("../../../../src/clud/mcp_server.py"),
    ),
    (
        "mcp_validate.py",
        include_str!("../../../../src/clud/mcp_validate.py"),
    ),
    (
        "serve_mcp.py",
        include_str!("../../../../src/clud/serve_mcp.py"),
    ),
];

/// The `--mcp-config=<json>` token for a `clud` in `exe_dir`, or `None` when
/// neither way of running the bridge is available.
///
/// One `=`-bound token on purpose: Claude declares `--mcp-config <configs...>`
/// as variadic, so the space-separated form would swallow a later `-p` prompt.
pub(super) fn claude_mcp_config_arg(exe_dir: &Path) -> Option<String> {
    let (path_env, state_root) = bridge_host()?;
    config_arg_for(exe_dir, &path_env, &state_root)
}

/// Why the agent will not see `mcp__clud__*` tools, with the fix, or `None`
/// when the bridge can run (#2521).
pub(super) fn bridge_missing_notice(exe_dir: &Path) -> Option<String> {
    let (path_env, state_root) = bridge_host().unwrap_or_default();
    notice_for(exe_dir, &path_env, &state_root)
}

fn bridge_host() -> Option<(String, PathBuf)> {
    // Unit tests pin exact argv and pass their own host to `config_arg_for`;
    // a `uv` on the machine running them must not join every launch plan.
    if cfg!(test) {
        return None;
    }
    let state_root = crate::home::user_home()?.join(".clud").join("state");
    Some((std::env::var("PATH").unwrap_or_default(), state_root))
}

fn config_arg_for(exe_dir: &Path, path_env: &str, state_root: &Path) -> Option<String> {
    let server = match bridge_python(exe_dir) {
        Some(python) => serde_json::json!({
            "type": "stdio",
            "command": python.to_string_lossy(),
            "args": ["-m", "clud.mcp_server"],
        }),
        None => native_server(exe_dir, path_env, state_root)?,
    };
    let config = serde_json::json!({ "mcpServers": { SERVER_NAME: server } });
    Some(format!("--mcp-config={config}"))
}

/// The server entry for a native install: `uv` supplies Python and the `mcp`
/// package, and the embedded sources are unpacked for `PYTHONPATH`. `-P` keeps
/// the launch directory off `sys.path`, so a repository's own `clud/` or
/// `anyio.py` cannot stand in for the bridge; it needs Python 3.11.
fn native_server(exe_dir: &Path, path_env: &str, state_root: &Path) -> Option<serde_json::Value> {
    let uv = crate::shim_uv::discover_uv(path_env, state_root)?;
    let package_root = unpack_bridge(state_root).ok()?;
    let clud = if crate::platform::host::WINDOWS {
        "clud.exe"
    } else {
        "clud"
    };
    Some(serde_json::json!({
        "type": "stdio",
        "command": uv.to_string_lossy(),
        "args": [
            "run", "--no-project", "--python", ">=3.11", "--with", BRIDGE_MCP_REQUIREMENT,
            "python", "-P", "-m", "clud.mcp_server",
        ],
        "env": {
            "PYTHONPATH": package_root.to_string_lossy(),
            "CLUD_BIN": exe_dir.join(clud).to_string_lossy(),
        },
    }))
}

/// Writes any embedded source that is missing or differs, under a per-version
/// directory, and returns the directory to put on `PYTHONPATH`.
fn unpack_bridge(state_root: &Path) -> std::io::Result<PathBuf> {
    let root = state_root
        .join("mcp-bridge")
        .join(env!("CARGO_PKG_VERSION"));
    let package = root.join("clud");
    std::fs::create_dir_all(&package)?;
    for (name, source) in BRIDGE_SOURCES {
        let file = package.join(name);
        if std::fs::read(&file).ok().as_deref() != Some(source.as_bytes()) {
            crate::fs_private::write_private_atomic(&file, source.as_bytes())?;
        }
    }
    Ok(root)
}

fn notice_for(exe_dir: &Path, path_env: &str, state_root: &Path) -> Option<String> {
    if config_arg_for(exe_dir, path_env, state_root).is_some() {
        return None;
    }
    Some(format!(
        "[clud] note: the clud MCP server is not connected for this launch (no bridge \
interpreter beside {}, and no usable `uv` to run the bundled bridge), so `mcp__clud__*` tools \
such as session_start and board_read are absent. Fix: install uv \
(https://docs.astral.sh/uv/getting-started/installation/) and relaunch.",
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
        let state = tempfile::tempdir().unwrap();
        let notice = notice_for(dir.path(), "", state.path()).expect("notice when absent");
        assert!(
            notice.contains("clud MCP server is not connected"),
            "{notice}"
        );
        assert!(notice.contains("install uv"), "{notice}");
        assert!(!notice.contains("dev binary"), "{notice}");
    }

    #[test]
    fn no_sibling_python_and_no_uv_registers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        assert_eq!(config_arg_for(dir.path(), "", state.path()), None);
    }

    /// A PATH holding a stand-in `uv`, as the `clud.bot` installer leaves a
    /// native binary with nothing but that on the machine.
    fn path_with_fake_uv(dir: &Path) -> String {
        let name = if crate::platform::host::WINDOWS {
            "uv.exe"
        } else {
            "uv"
        };
        let uv = dir.join(name);
        std::fs::write(&uv, b"").unwrap();
        crate::platform::shim_fs::set_mode(&uv, 0o755).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn server_of(arg: &str) -> serde_json::Value {
        let json = arg.strip_prefix("--mcp-config=").unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        value["mcpServers"]["clud"].clone()
    }

    #[test]
    fn a_native_install_runs_the_embedded_bridge_through_uv() {
        // A native `install.sh` binary has no sibling interpreter, and used to
        // lose every `mcp__clud__*` tool for that reason.
        let exe_dir = tempfile::tempdir().unwrap();
        let uv_dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let path = path_with_fake_uv(uv_dir.path());

        let arg = config_arg_for(exe_dir.path(), &path, state.path()).expect("native bridge");
        let server = server_of(&arg);

        assert!(server["command"].as_str().unwrap().contains("uv"));
        let args: Vec<&str> = server["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(&args[..2], ["run", "--no-project"], "{args:?}");
        assert!(args.contains(&"mcp>=2.1"), "{args:?}");
        assert_eq!(&args[args.len() - 2..], ["-m", "clud.mcp_server"]);

        let root = std::path::PathBuf::from(server["env"]["PYTHONPATH"].as_str().unwrap());
        for name in [
            "__init__.py",
            "mcp_server.py",
            "mcp_validate.py",
            "serve_mcp.py",
        ] {
            assert!(root.join("clud").join(name).is_file(), "{name} not written");
        }
        assert!(root.starts_with(state.path()), "{root:?}");
        assert!(server["env"]["CLUD_BIN"]
            .as_str()
            .unwrap()
            .starts_with(&*exe_dir.path().to_string_lossy()));
    }

    #[test]
    fn the_embedded_bridge_imports_every_clud_module_it_needs() {
        // A new `from clud import x` in the bridge must join BRIDGE_SOURCES.
        for (_, source) in BRIDGE_SOURCES {
            for line in source.lines() {
                let Some(rest) = line.strip_prefix("from clud") else {
                    continue;
                };
                let module = rest
                    .trim_start_matches('.')
                    .split_whitespace()
                    .next()
                    .filter(|m| *m != "import")
                    .map(str::to_string)
                    .or_else(|| rest.split_whitespace().nth(1).map(str::to_string))
                    .unwrap();
                let file = format!("{module}.py");
                assert!(
                    BRIDGE_SOURCES.iter().any(|(name, _)| *name == file),
                    "{file} is imported by the bridge but not embedded"
                );
            }
        }
    }

    #[test]
    fn the_uv_dependency_matches_the_wheel_dependency() {
        let pyproject = include_str!("../../../../pyproject.toml");
        assert!(
            pyproject.contains(&format!("\"{BRIDGE_MCP_REQUIREMENT};")),
            "{BRIDGE_MCP_REQUIREMENT} drifted from pyproject.toml"
        );
    }

    #[test]
    fn a_sibling_python_still_wins_over_uv() {
        let exe_dir = tempfile::tempdir().unwrap();
        let uv_dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let name = if crate::platform::host::WINDOWS {
            "python.exe"
        } else {
            "python"
        };
        std::fs::write(exe_dir.path().join(name), b"").unwrap();
        let path = path_with_fake_uv(uv_dir.path());
        let arg = config_arg_for(exe_dir.path(), &path, state.path()).unwrap();
        assert_eq!(
            server_of(&arg)["args"],
            serde_json::json!(["-m", "clud.mcp_server"])
        );
    }

    #[test]
    fn a_rerun_rewrites_a_corrupted_embedded_file() {
        let exe_dir = tempfile::tempdir().unwrap();
        let uv_dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let path = path_with_fake_uv(uv_dir.path());
        let first = config_arg_for(exe_dir.path(), &path, state.path()).unwrap();
        let root =
            std::path::PathBuf::from(server_of(&first)["env"]["PYTHONPATH"].as_str().unwrap());
        let file = root.join("clud").join("mcp_server.py");
        std::fs::write(&file, b"stale").unwrap();
        config_arg_for(exe_dir.path(), &path, state.path()).unwrap();
        assert_ne!(std::fs::read(&file).unwrap(), b"stale");
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
        let state = tempfile::tempdir().unwrap();
        let arg = config_arg_for(dir.path(), "", state.path()).unwrap();
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

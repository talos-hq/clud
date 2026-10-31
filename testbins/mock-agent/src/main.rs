//! Mock agent binary for integration testing.
//!
//! This binary is copied/symlinked as `claude` or `codex` in a temp directory
//! and placed on PATH. It records the args it received and exits.
//!
//! Behavior:
//! - Writes received args as JSON to stdout
//! - Reads stdin if available (for pipe mode testing)
//! - Exits with the code specified by --mock-exit-code (default 0)
//! - With a leading `--version` arg, prints `MOCK_CLAUDE_VERSION`
//!   (default `9.9.9 (mock-agent)`) so tests can exercise clud's Claude Code
//!   version gate
//! - With --mock-read-stdin-ms, reads stdin for N ms (even if terminal) and reports it
//! - With --mock-stdin-raw-to, writes captured stdin bytes (pre-JSON) to a file
//!   using Rust byte-literal escaping (e.g., `\x1b`) so binary input is preserved.
//! - With --mock-report-pty-size, polls and reports host/PTY dimensions via the
//!   `terminal_size` crate to a JSON file for the resize-propagation test.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CODEX_BRIDGE_PROBE_REQUEST: &str = include_str!("../assets/codex_bridge_probe_request.json");

mod codex_app_server;
mod platform;
mod serve;

#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn main() {
    trace("start");
    platform::signals::restore_default_interrupt();
    let args: Vec<String> = std::env::args().collect();
    // Windows shim tests copy this executable to the next PATH entry. It is
    // a recording handoff only: even a missed catastrophe guard cannot delete.
    if let Some(path) = std::env::var_os("MOCK_RM_STUB_LOG") {
        let payload = serde_json::to_vec(&args[1..]).expect("serialize recording-stub args");
        std::fs::write(path, payload).expect("write recording-stub args");
        return;
    }

    // `mock-agent serve …`: the scripted model backend for the real-harness
    // test tier (#1323). Everything below is the fake-`claude` behavior.
    if args.get(1).map(String::as_str) == Some("serve") {
        std::process::exit(serve::run(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("app-server") {
        if let Err(error) = codex_app_server::run() {
            eprintln!("mock Codex app-server failed: {error}");
            std::process::exit(1);
        }
        return;
    }

    // Extract --mock-exit-code if present (our own flag, not forwarded by clud)
    let mut exit_code = 0i32;
    let mut sleep_ms = 0u64;
    let mut read_stdin_ms = 0u64;
    let mut helper_role: Option<String> = None;
    let mut tree_log: Option<PathBuf> = None;
    let mut report_file: Option<PathBuf> = None;
    let mut started_file: Option<PathBuf> = None;
    let mut wait_for_file: Option<PathBuf> = None;
    let mut write_done_at: Option<PathBuf> = None;
    let mut write_done_body = String::from("mock-done");
    let mut write_blocked_at: Option<PathBuf> = None;
    let mut write_blocked_body = String::from("mock-blocked");
    let mut write_marker_on_iter: u32 = 0;
    let mut stdin_raw_to: Option<PathBuf> = None;
    let mut ready_file: Option<PathBuf> = None;
    let mut read_stdin_until: Option<Vec<u8>> = None;
    let mut pty_size_report_to: Option<PathBuf> = None;
    let mut pty_size_samples: u32 = 0;
    let mut pty_size_interval_ms: u64 = 100;
    let mut ansi_script: Option<PathBuf> = None;
    let mut ansi_after_wait: Option<PathBuf> = None;
    let mut tool_shell_probe_to: Option<PathBuf> = None;
    let mut bash_nounset_probe_to: Option<PathBuf> = None;
    let mut codex_bridge_probe_to: Option<PathBuf> = None;
    let mut codex_cache_identity_probe_to: Option<PathBuf> = None;
    let mut unified_route_probe_to: Option<PathBuf> = None;
    let mut unified_acceptance_probe_to: Option<PathBuf> = None;
    let mut acceptance_extra_turn: Option<String> = None;
    // Emit canned `--output-format stream-json` lines from a file (one line
    // each, separated by `--mock-stream-delay-ms`). Used by integration tests
    // that exercise clud's stream-json renderer without needing a real
    // claude subscription key.
    let mut stream_json_script: Option<PathBuf> = None;
    let mut stream_delay_ms: u64 = 0;
    let mut filtered_args: Vec<String> = Vec::new();
    let mut skip_next = false;
    for (i, arg) in args.iter().enumerate().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--mock-exit-code" {
            if let Some(code) = args.get(i + 1) {
                exit_code = code.parse().unwrap_or(0);
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-write-done" {
            if let Some(path) = args.get(i + 1) {
                write_done_at = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-write-done-body" {
            if let Some(body) = args.get(i + 1) {
                write_done_body = body.clone();
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-write-blocked" {
            if let Some(path) = args.get(i + 1) {
                write_blocked_at = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-write-blocked-body" {
            if let Some(body) = args.get(i + 1) {
                write_blocked_body = body.clone();
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-write-marker-on-iter" {
            if let Some(n) = args.get(i + 1) {
                write_marker_on_iter = n.parse().unwrap_or(0);
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-sleep-ms" {
            if let Some(ms) = args.get(i + 1) {
                sleep_ms = ms.parse().unwrap_or(0);
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-read-stdin-ms" {
            if let Some(ms) = args.get(i + 1) {
                read_stdin_ms = ms.parse().unwrap_or(0);
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-helper-role" {
            if let Some(role) = args.get(i + 1) {
                helper_role = Some(role.clone());
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-spawn-tree-log" {
            if let Some(path) = args.get(i + 1) {
                tree_log = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-report-file" {
            if let Some(path) = args.get(i + 1) {
                report_file = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-read-stdin-until" {
            if let Some(marker) = args.get(i + 1).filter(|marker| !marker.is_empty()) {
                read_stdin_until = Some(marker.as_bytes().to_vec());
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-ready-file" {
            if let Some(path) = args.get(i + 1) {
                ready_file = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-started-file" {
            if let Some(path) = args.get(i + 1) {
                started_file = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-wait-for-file" {
            if let Some(path) = args.get(i + 1) {
                wait_for_file = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-stdin-raw-to" {
            if let Some(path) = args.get(i + 1) {
                stdin_raw_to = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-report-pty-size" {
            if let Some(path) = args.get(i + 1) {
                pty_size_report_to = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-pty-size-samples" {
            if let Some(n) = args.get(i + 1) {
                pty_size_samples = n.parse().unwrap_or(0);
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-pty-size-interval-ms" {
            if let Some(n) = args.get(i + 1) {
                pty_size_interval_ms = n.parse().unwrap_or(100);
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-ansi-script" {
            if let Some(path) = args.get(i + 1) {
                ansi_script = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-ansi-after-wait" {
            if let Some(path) = args.get(i + 1) {
                ansi_after_wait = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-bash-nounset-probe" {
            if let Some(path) = args.get(i + 1) {
                bash_nounset_probe_to = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-tool-shell-probe" {
            if let Some(path) = args.get(i + 1) {
                tool_shell_probe_to = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-codex-bridge-probe" {
            if let Some(path) = args.get(i + 1) {
                codex_bridge_probe_to = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-codex-cache-identity-probe" {
            if let Some(path) = args.get(i + 1) {
                codex_cache_identity_probe_to = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-acceptance-extra-turn" {
            acceptance_extra_turn = args.get(i + 1).cloned();
            skip_next = true;
            continue;
        }
        if arg == "--mock-unified-acceptance-probe" {
            if let Some(path) = args.get(i + 1) {
                unified_acceptance_probe_to = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-unified-route-probe" {
            if let Some(path) = args.get(i + 1) {
                unified_route_probe_to = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-stream-json" {
            if let Some(path) = args.get(i + 1) {
                stream_json_script = Some(PathBuf::from(path));
            }
            skip_next = true;
            continue;
        }
        if arg == "--mock-stream-delay-ms" {
            if let Some(ms) = args.get(i + 1) {
                stream_delay_ms = ms.parse().unwrap_or(0);
            }
            skip_next = true;
            continue;
        }
        filtered_args.push(arg.clone());
    }

    // `clud` probes `claude --version` for the unified gateway-discovery
    // floor (issue #921). Stand in for a real Claude Code install: print the
    // version string from `MOCK_CLAUDE_VERSION` (default: a release far above
    // the floor, so existing tests stay silent) and exit before the JSON
    // report machinery runs.
    if args.get(1).map(String::as_str) == Some("--version") {
        if let Ok(path) = std::env::var("CLUD_KITTY_SMOKE_VERSION_MARKER") {
            let _ = std::fs::write(path, "mock-agent --version");
        }
        let version = std::env::var("MOCK_CLAUDE_VERSION")
            .unwrap_or_else(|_| "9.9.9 (mock-agent)".to_string());
        println!("{version}");
        return;
    }

    // Emit scripted ANSI bytes before everything else so they land in the
    // PTY / terminal capture first. Used by the attach-replay integration
    // test to paint a known frame whose presence we can verify in a
    // post-detach reattach's snapshot.
    if let Some(path) = ansi_script.as_ref() {
        if let Ok(bytes) = std::fs::read(path) {
            let _ = io::stdout().write_all(&bytes);
            let _ = io::stdout().flush();
        }
    }

    // Emit canned stream-json events line-by-line. When the integration
    // test wants to exercise clud's renderer, it points this at a file
    // containing one JSON event per line; we write them with a flush
    // between each so the parent's drain loop sees them incrementally,
    // exactly like real claude would emit them. Then exit immediately
    // so the test's wait completes without the usual JSON-report tail.
    // `clud workflow` integration (#2471): every `agent()` call is a
    // separate `clud -p <prompt>` child, so per-call `--mock-*` flags are
    // impossible. `MOCK_AGENT_WORKFLOW_RESPONSES` names a JSON object whose
    // keys are prompt substrings and whose values are `{result,
    // structured_output, is_error}`; the first key contained in the `-p`
    // prompt selects the scripted stream-json reply. Each call is also
    // appended to `<file>.calls` so tests can assert on concurrency inputs.
    if let Ok(path) = std::env::var("MOCK_AGENT_WORKFLOW_RESPONSES") {
        emit_workflow_response(&path, &args);
    }

    if let Some(path) = stream_json_script.as_ref() {
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let _ = io::stdout().write_all(line.as_bytes());
                let _ = io::stdout().write_all(b"\n");
                let _ = io::stdout().flush();
                if stream_delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(stream_delay_ms));
                }
            }
        }
        std::process::exit(exit_code);
    }

    if let Some(path) = pty_size_report_to.as_ref() {
        run_pty_size_probe(path, pty_size_samples.max(1), pty_size_interval_ms);
        std::process::exit(exit_code);
    }

    if let Some(role) = helper_role.as_deref() {
        run_helper(&args[0], role, tree_log.as_ref(), sleep_ms);
        return;
    }

    if let Some(path) = bash_nounset_probe_to.as_ref() {
        run_bash_nounset_probe(path);
        return;
    }

    if let Some(path) = tool_shell_probe_to.as_ref() {
        platform::tool_shell_probe::run_tool_shell_probe(&args[0], path);
        return;
    }

    if let Some(path) = started_file.as_ref() {
        let ready = serde_json::json!({
            "pid": std::process::id(),
            "env": {
                "CLUD_KITTY_TERM": std::env::var("CLUD_KITTY_TERM").ok(),
                "WEZTERM_PANE": std::env::var("WEZTERM_PANE").ok(),
                "WEZTERM_UNIX_SOCKET": std::env::var("WEZTERM_UNIX_SOCKET").ok(),
            },
        });
        if let Err(error) = write_atomic_ready_file(path, &ready) {
            eprintln!("mock-agent could not write readiness report: {error}");
            std::process::exit(87);
        }
    }

    // Track which iteration we're on by reading/bumping a counter file whose
    // path is shared by all three marker flags. We compute that path as the
    // parent of the first marker path, suffixed with ".iter-count".
    let counter_path = write_done_at
        .as_ref()
        .or(write_blocked_at.as_ref())
        .map(|p| p.with_file_name("iter-count"));
    let iteration = bump_iter_counter(counter_path.as_deref());

    if write_marker_on_iter > 0 && iteration >= write_marker_on_iter {
        if let Some(path) = write_done_at.as_ref() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, &write_done_body);
        }
        if let Some(path) = write_blocked_at.as_ref() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, &write_blocked_body);
        }
    }

    if let Some(path) = tree_log.as_ref() {
        append_tree_log(path, "root");
        spawn_helper(&args[0], "child", path, sleep_ms);
    }

    let stdin_is_terminal = io::stdin().is_terminal();

    // Read stdin: either timed read (--mock-read-stdin-ms) or pipe-mode read
    let stdin_bytes: Option<Vec<u8>> = if read_stdin_ms > 0 {
        trace("stdin read start");
        let bytes = read_stdin_timed(
            read_stdin_ms,
            ready_file.as_deref(),
            read_stdin_until.as_deref(),
        );
        trace(&format!(
            "stdin read done: {} bytes",
            bytes.as_ref().map_or(0, Vec::len)
        ));
        bytes
    } else if !stdin_is_terminal {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf).ok();
        if buf.is_empty() {
            None
        } else {
            Some(buf)
        }
    } else {
        None
    };

    if let (Some(path), Some(bytes)) = (stdin_raw_to.as_ref(), stdin_bytes.as_ref()) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, bytes);
    }

    let stdin_content: Option<String> = stdin_bytes
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).into_owned());

    // Capture env vars relevant for testing
    let in_clud = std::env::var("IN_CLUD").ok();
    let clud_exe = std::env::var("CLUD_EXE").ok();
    let originator = std::env::var("RUNNING_PROCESS_ORIGINATOR").ok();
    let clud_exe_probe = std::env::var_os("MOCK_AGENT_CLUD_EXE_PROBE")
        .map(|_| {
            Command::new("sh")
                .args(["-c", "\"$CLUD_EXE\" --version"])
                .output()
        })
        .map(|result| match result {
            Ok(output) => serde_json::json!({
                "exit_code": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            }),
            Err(error) => serde_json::json!({"error": error.to_string()}),
        });
    let anthropic_base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
    let anthropic_auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let anthropic_base_url_present = anthropic_base_url.is_some();
    let anthropic_auth_token_present = anthropic_auth_token.is_some();
    let anthropic_api_key_present = std::env::var_os("ANTHROPIC_API_KEY").is_some();
    let api_timeout_ms = std::env::var("API_TIMEOUT_MS").ok();
    let disable_nonessential_traffic =
        std::env::var("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").ok();
    let enable_gateway_model_discovery =
        std::env::var("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY").ok();
    let max_context_tokens = std::env::var("CLAUDE_CODE_MAX_CONTEXT_TOKENS").ok();
    let wezterm_unix_socket = std::env::var("WEZTERM_UNIX_SOCKET").ok();
    let bridge_probe = codex_bridge_probe_to.as_deref().map(run_codex_bridge_probe);
    let cache_identity_probe = codex_cache_identity_probe_to
        .as_deref()
        .map(run_codex_cache_identity_probe);
    if let Some(path) = unified_acceptance_probe_to.as_deref() {
        run_unified_acceptance_probe(path, acceptance_extra_turn.as_deref());
    }
    let unified_route_probe = unified_route_probe_to
        .as_deref()
        .map(run_unified_route_probe);
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().to_string());

    if let Some(path) = wait_for_file.as_ref() {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !path.is_file() {
            if Instant::now() >= deadline {
                eprintln!("mock-agent timed out waiting for {}", path.display());
                std::process::exit(88);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if let Some(path) = ansi_after_wait.as_ref() {
        let bytes = std::fs::read(path).unwrap_or_else(|error| {
            eprintln!("mock-agent could not read post-release ANSI script: {error}");
            std::process::exit(89);
        });
        io::stdout().write_all(&bytes).unwrap_or_else(|error| {
            eprintln!("mock-agent could not emit post-release ANSI script: {error}");
            std::process::exit(89);
        });
        io::stdout().flush().unwrap_or_else(|error| {
            eprintln!("mock-agent could not flush post-release ANSI script: {error}");
            std::process::exit(89);
        });
    }
    if sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }

    // Output JSON report of what we received
    let report = serde_json::json!({
        "program": args[0],
        "args": filtered_args,
        "cwd": cwd,
        "stdin": stdin_content,
        "stdin_is_terminal": stdin_is_terminal,
        "exit_code": exit_code,
        "sleep_ms": sleep_ms,
        "env": {
            "IN_CLUD": in_clud,
            "UV_CACHE_DIR": std::env::var("UV_CACHE_DIR").ok(),
            "CLUD_EXE": clud_exe,
            "RUNNING_PROCESS_ORIGINATOR": originator,
            "ANTHROPIC_BASE_URL_PRESENT": anthropic_base_url_present,
            "ANTHROPIC_AUTH_TOKEN_PRESENT": anthropic_auth_token_present,
            "ANTHROPIC_API_KEY_PRESENT": anthropic_api_key_present,
            "API_TIMEOUT_MS": api_timeout_ms,
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": disable_nonessential_traffic,
            "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": enable_gateway_model_discovery,
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": max_context_tokens,
            "WEZTERM_UNIX_SOCKET": wezterm_unix_socket,
        },
        "codex_bridge_probe": bridge_probe,
        "codex_cache_identity_probe": cache_identity_probe,
        "unified_route_probe": unified_route_probe,
        "clud_exe_probe": clud_exe_probe,
    });

    let report_str = serde_json::to_string(&report).unwrap();
    if [
        anthropic_base_url.as_deref(),
        anthropic_auth_token.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|secret| !secret.is_empty() && report_str.contains(secret))
    {
        eprintln!("mock-agent refused to serialize a bridge credential");
        std::process::exit(86);
    }
    trace("report write start");
    println!("{}", report_str);
    trace("report write done");

    // Also write to file if requested (useful when stdout is captured by PTY)
    if let Some(path) = report_file {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, &report_str);
    }

    trace(&format!("exit {exit_code}"));
    std::process::exit(exit_code);
}

/// Append one timestamped stage line to `$MOCK_AGENT_TRACE_DIR/mock-<pid>.log`
/// (#1310). CI dumps these files when a PTY test times out, so a hang shows
/// whether the child stalled reading stdin, writing its report, or exiting.
/// A no-op unless the variable is set.
fn trace(stage: &str) {
    let Some(dir) = std::env::var_os("MOCK_AGENT_TRACE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let path = dir.join(format!("mock-{}.log", std::process::id()));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{millis} {stage}");
    }
}

fn write_atomic_ready_file(path: &Path, ready: &serde_json::Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("ready-{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(ready).map_err(io::Error::other)?;
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

fn run_codex_bridge_probe(report_path: &Path) -> serde_json::Value {
    let base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
    let token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let mut report = serde_json::json!({
        "attempted": false,
        "loopback": false,
        "port": null,
        "status": null,
        "bridged_reply_received": false,
        "error": null,
    });

    let result = (|| -> Result<(), String> {
        let base_url = base_url.as_deref().ok_or("missing bridge URL")?;
        let token = token.as_deref().ok_or("missing bridge token")?;
        let address: SocketAddr = base_url
            .strip_prefix("http://")
            .ok_or("bridge URL is not HTTP")?
            .parse()
            .map_err(|_| "bridge URL is not a socket address")?;
        report["attempted"] = true.into();
        report["loopback"] = address.ip().is_loopback().into();
        report["port"] = address.port().into();

        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .map_err(|error| format!("connect failed: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| format!("read timeout failed: {error}"))?;
        let body = CODEX_BRIDGE_PROBE_REQUEST.trim();
        let request = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("write failed: {error}"))?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .map_err(|error| format!("read failed: {error}"))?;
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or("missing HTTP status")?;
        report["status"] = status.into();
        // #627 step 5: the bridge now translates a real upstream reply, so the
        // probe looks for the text the fake Responses server produced.
        report["bridged_reply_received"] = response.contains("bridged reply").into();
        Ok(())
    })();
    if let Err(error) = result {
        report["error"] = error.into();
    }
    if let Some(parent) = report_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(report_path, serde_json::to_vec(&report).unwrap_or_default());
    report
}

/// Production-shaped multi-turn bridge probe for cache-identity coverage.
///
/// The report intentionally contains only request status and reply counts. The
/// integration fake is the wire ledger for headers, bodies, and cache keys;
/// neither the gateway token nor the fixed Claude session/agent literals may
/// escape this process.
fn run_codex_cache_identity_probe(report_path: &Path) -> serde_json::Value {
    let base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
    let token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let mut report = serde_json::json!({
        "attempted": false,
        "loopback": false,
        "statuses": [],
        "bridged_reply_count": 0,
        "error": null,
    });

    let result = (|| -> Result<(), String> {
        let base_url = base_url.as_deref().ok_or("missing bridge URL")?;
        let token = token.as_deref().ok_or("missing bridge token")?;
        let address: SocketAddr = base_url
            .strip_prefix("http://")
            .ok_or("bridge URL is not HTTP")?
            .parse()
            .map_err(|_| "bridge URL is not a socket address")?;
        report["attempted"] = true.into();
        report["loopback"] = address.ip().is_loopback().into();
        let turns = [
            None,
            None,
            None,
            None,
            None,
            Some("cache-agent-a"),
            Some("cache-agent-a"),
            Some("cache-agent-b"),
        ];
        // A large shared prefix makes cache loss observable as a budget
        // regression, not merely a change in an otherwise opaque key.
        let fixed_prefix = "cache-prefix ".repeat(1_260);
        let mut statuses = Vec::new();
        let mut replies = 0_u64;

        for (index, agent) in turns.into_iter().enumerate() {
            let body = serde_json::json!({
                "model": "claude-x",
                "messages": [{"role": "user", "content": format!("{fixed_prefix}cache-probe-turn-{index}")}],
                "stream": false,
            })
            .to_string();
            let agent_headers = agent.map_or_else(String::new, |agent| {
                format!(
                    "x-claude-code-agent-id: {agent}\r\nx-claude-code-parent-agent-id: cache-main\r\n"
                )
            });
            let request = format!(
                "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-Claude-Code-Session-Id: cache-session\r\n{agent_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
                .map_err(|error| format!("connect failed: {error}"))?;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .map_err(|error| format!("read timeout failed: {error}"))?;
            stream
                .write_all(request.as_bytes())
                .map_err(|error| format!("write failed: {error}"))?;
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .map_err(|error| format!("read failed: {error}"))?;
            let status = response
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u16>().ok())
                .ok_or("missing HTTP status")?;
            statuses.push(status);
            replies += u64::from(response.contains("bridged reply"));
        }
        report["statuses"] = serde_json::json!(statuses);
        report["bridged_reply_count"] = replies.into();
        Ok(())
    })();
    if let Err(error) = result {
        report["error"] = error.into();
    }
    if let Some(parent) = report_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(report_path, serde_json::to_vec(&report).unwrap_or_default());
    report
}

/// Drive representative native-Claude, Codex, DeepSeek, and OpenRouter turns
/// through one real foreground unified launch. The Python integration fake
/// owns wire-level assertions; this report intentionally contains statuses and
/// response presence only, never the gateway bearer, URL, or test literals.
fn run_unified_route_probe(report_path: &Path) -> serde_json::Value {
    let base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
    let token = std::env::var("CLUD_GATEWAY_TOKEN").ok();
    let mut report = serde_json::json!({
        "attempted": false,
        "loopback": false,
        "statuses": [],
        "response_count": 0,
        "error": null,
    });
    let result = (|| -> Result<(), String> {
        let base_url = base_url.as_deref().ok_or("missing bridge URL")?;
        let token = token.as_deref().ok_or("missing gateway token")?;
        let address: SocketAddr = base_url
            .strip_prefix("http://")
            .ok_or("bridge URL is not HTTP")?
            .parse()
            .map_err(|_| "bridge URL is not a socket address")?;
        report["attempted"] = true.into();
        report["loopback"] = address.ip().is_loopback().into();
        let turns = [
            ("claude-opus-4-1", None),
            ("claude-opus-4-1", None),
            ("clud-claude-codex-terra", None),
            ("clud-claude-codex-terra", None),
            ("clud-claude-deepseek-flash", None),
            ("clud-claude-deepseek-flash", Some("unified-agent-deepseek")),
            ("clud-claude-openrouter-sonnet", None),
            ("clud-claude-openrouter-sonnet", None),
            ("clud-claude-codex-terra", Some("unified-agent-codex")),
            ("clud-claude-codex-terra", None),
        ];
        let mut statuses = Vec::new();
        let mut responses = 0_u64;
        for (index, (model, agent)) in turns.into_iter().enumerate() {
            let body = serde_json::json!({
                "model": model,
                "system": [{"type": "text", "text": "unified-cache-marker", "cache_control": {"type": "ephemeral"}}],
                "messages": [{"role": "user", "content": format!("unified-route-turn-{index}")}],
                "stream": false,
            })
            .to_string();
            let agent_headers = agent.map_or_else(String::new, |agent| {
                format!(
                    "x-claude-code-agent-id: {agent}\r\nx-claude-code-parent-agent-id: unified-main\r\n"
                )
            });
            let request = format!(
                "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Clud-Gateway-Token: {token}\r\nContent-Type: application/json\r\nX-Claude-Code-Session-Id: unified-session\r\n{agent_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
                .map_err(|error| format!("connect failed: {error}"))?;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .map_err(|error| format!("read timeout failed: {error}"))?;
            stream
                .write_all(request.as_bytes())
                .map_err(|error| format!("write failed: {error}"))?;
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .map_err(|error| format!("read failed: {error}"))?;
            let status = response
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u16>().ok())
                .ok_or("missing HTTP status")?;
            statuses.push(status);
            responses += u64::from(response.contains("fake") || response.contains("bridged reply"));
        }
        report["statuses"] = serde_json::json!(statuses);
        report["response_count"] = responses.into();
        Ok(())
    })();
    if let Err(error) = result {
        report["error"] = error.into();
    }
    if let Some(parent) = report_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(report_path, serde_json::to_vec(&report).unwrap_or_default());
    report
}

/// One HTTP/1.1 exchange with the gateway; returns (status, body).
fn gateway_exchange(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Clud-Gateway-Token: {token}\r\nContent-Type: application/json\r\nX-Claude-Code-Session-Id: acceptance-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|error| format!("connect failed: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("read timeout failed: {error}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write failed: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("read failed: {error}"))?;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or("missing HTTP status")?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

/// #901 acceptance probe: model discovery, then one conversation that crosses
/// Claude -> Codex -> DeepSeek -> Claude with a different effort each turn,
/// then an unknown reserved model id that must be rejected locally.
/// `extra_turn` appends one more model (at high effort) before the unknown id,
/// so a suite can route a provider the fixed four turns do not cover.
fn run_unified_acceptance_probe(report_path: &Path, extra_turn: Option<&str>) -> serde_json::Value {
    let mut report = serde_json::json!({
        "models_status": null,
        "models": [],
        "turn_statuses": [],
        "unknown_model_status": null,
        "error": null,
    });
    let result = (|| -> Result<(), String> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL").map_err(|_| "missing bridge URL")?;
        let token = std::env::var("CLUD_GATEWAY_TOKEN").map_err(|_| "missing gateway token")?;
        let address: SocketAddr = base_url
            .strip_prefix("http://")
            .ok_or("bridge URL is not HTTP")?
            .parse()
            .map_err(|_| "bridge URL is not a socket address")?;
        let (status, body) =
            gateway_exchange(address, "GET", "/v1/models?limit=1000", &token, None)?;
        report["models_status"] = status.into();
        let listing: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        report["models"] = listing["data"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| serde_json::json!({"id": row["id"], "display_name": row["display_name"]}))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into();
        let mut turns = vec![
            ("claude-opus-4-1", "low"),
            ("clud-claude-codex-terra", "high"),
            ("clud-claude-deepseek-flash", "max"),
            ("claude-opus-4-1", "medium"),
        ];
        if let Some(model) = extra_turn {
            turns.push((model, "high"));
        }
        let mut statuses = Vec::new();
        for (index, (model, effort)) in turns.into_iter().enumerate() {
            let body = serde_json::json!({
                "model": model,
                "max_tokens": 64,
                "output_config": {"effort": effort},
                "messages": [{"role": "user", "content": format!("acceptance-turn-{index}")}],
                "stream": false,
            })
            .to_string();
            statuses
                .push(gateway_exchange(address, "POST", "/v1/messages", &token, Some(&body))?.0);
        }
        report["turn_statuses"] = serde_json::json!(statuses);
        let unknown = serde_json::json!({
            "model": "clud-claude-nonexistent-route",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "unknown"}],
            "stream": false,
        })
        .to_string();
        report["unknown_model_status"] =
            gateway_exchange(address, "POST", "/v1/messages", &token, Some(&unknown))?
                .0
                .into();
        Ok(())
    })();
    if let Err(error) = result {
        report["error"] = error.into();
    }
    if let Some(parent) = report_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(report_path, serde_json::to_vec(&report).unwrap_or_default());
    report
}

/// Read from stdin for up to `timeout_ms` milliseconds, collecting whatever arrives.
/// Works regardless of whether stdin is a terminal or pipe.
/// Read stdin for up to `timeout_ms` after the input mode is final, or until
/// the collected bytes contain `until` (`--mock-read-stdin-until`), so a test
/// whose input takes a variable time to arrive needs no worst-case window.
fn read_stdin_timed(
    timeout_ms: u64,
    ready_file: Option<&Path>,
    until: Option<&[u8]>,
) -> Option<Vec<u8>> {
    // Real TUI children (e.g., codex Ink) put their PTY slave into raw mode
    // before reading. The mock-agent must do the same when its stdin is a PTY
    // slave, otherwise the kernel's canonical line discipline holds non-
    // newline-terminated bytes (like the F3 voice-mode transcript) forever
    // and they never reach the test's stdin capture.
    platform::stdin_mode::set_stdin_raw_if_tty();
    // #1310: tell the test its input mode is final. On Windows, ConPTY turns
    // input into key records as the bytes arrive, under the console mode of
    // that moment, so bytes sent before this point lose their escape
    // sequences. A test sends only after this file exists.
    if let Some(path) = ready_file {
        let ready = serde_json::json!({ "pid": std::process::id(), "stdin_raw_ready": true });
        if let Err(error) = write_atomic_ready_file(path, &ready) {
            eprintln!("mock-agent could not write stdin readiness report: {error}");
            std::process::exit(87);
        }
    }

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let stdin = io::stdin();
        loop {
            match stdin.lock().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(data) => {
                collected.extend(data);
                if until.is_some_and(|marker| {
                    collected
                        .windows(marker.len())
                        .any(|window| window == marker)
                }) {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if collected.is_empty() {
        None
    } else {
        Some(collected)
    }
}

/// Poll `terminal_size::terminal_size()` `samples` times, sleeping
/// `interval_ms` between each poll, and write a JSON array of `(cols, rows)`
/// pairs (nullable when no terminal is detected) to `path`. Also emits a
/// marker line to stdout after each sample so the test harness can drive
/// a mid-run resize between samples.
fn run_pty_size_probe(path: &Path, samples: u32, interval_ms: u64) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut out = Vec::new();
    for i in 0..samples {
        let size = terminal_size::terminal_size();
        let entry = match size {
            Some((w, h)) => serde_json::json!({ "cols": w.0, "rows": h.0 }),
            None => serde_json::json!({ "cols": null, "rows": null }),
        };
        out.push(entry.clone());
        let _ = std::fs::write(path, serde_json::to_string(&out).unwrap());
        let line = format!("PTY_SIZE_SAMPLE {} {}\n", i + 1, entry);
        let _ = io::stdout().write_all(line.as_bytes());
        let _ = io::stdout().flush();
        if i + 1 < samples {
            std::thread::sleep(Duration::from_millis(interval_ms));
        }
    }
}

fn bump_iter_counter(path: Option<&Path>) -> u32 {
    let path = match path {
        Some(p) => p,
        None => return 1,
    };
    let cur: u32 = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let next = cur + 1;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, next.to_string());
    next
}

fn run_helper(exe: &str, role: &str, tree_log: Option<&PathBuf>, sleep_ms: u64) {
    if let Some(path) = tree_log {
        append_tree_log(path, role);
        if role == "child" {
            spawn_helper(exe, "grandchild", path, sleep_ms);
        }
    }
    if sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
}

fn spawn_helper(exe: &str, role: &str, tree_log: &PathBuf, sleep_ms: u64) {
    let mut command = Command::new(exe);
    command
        .arg("--mock-helper-role")
        .arg(role)
        .arg("--mock-spawn-tree-log")
        .arg(tree_log)
        .arg("--mock-sleep-ms")
        .arg(sleep_ms.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = command.spawn();
}

fn append_tree_log(path: &PathBuf, role: &str) {
    let parent = path.parent().expect("tree log parent");
    let _ = std::fs::create_dir_all(parent);
    let line = serde_json::json!({
        "role": role,
        "pid": std::process::id(),
        "ppid": std::process::id(),
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open tree log");
    use std::io::Write;
    writeln!(file, "{}", line).expect("write tree log");
}

use std::io::IsTerminal;

/// Issue #1066: stand in for a Bash tool call that references an unset
/// variable.
///
/// The agent is the process clud launched, so the shell spawned here inherits
/// exactly the environment clud built — which is the thing under test. Running
/// the same probe from the test process instead would prove only that bash
/// honours `BASH_ENV`, not that clud actually set it on the backend.
///
/// The command is `rm -rf "$SP"/` from #1064 reduced to an echo. Under nounset
/// the shell dies before the command runs; without it, `$SP` expands to
/// nothing and the caller acts on the wrong path.
fn run_bash_nounset_probe(report_path: &Path) {
    let output = Command::new("bash")
        .args(["-c", r#"echo "[${SP}]""#])
        .stdin(Stdio::null())
        .output();
    let report = match output {
        Ok(output) => serde_json::json!({
            "bash_nounset_probe": {
                "spawned": true,
                "exit_code": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout).trim().to_string(),
                "stderr": String::from_utf8_lossy(&output.stderr).trim().to_string(),
                "bash_env": std::env::var("BASH_ENV").ok(),
            }
        }),
        Err(err) => serde_json::json!({
            "bash_nounset_probe": {
                "spawned": false,
                "error": err.to_string(),
            }
        }),
    };
    let _ = std::fs::write(report_path, serde_json::to_string_pretty(&report).unwrap());
}

/// Reply to a headless `-p` call from the `MOCK_AGENT_WORKFLOW_RESPONSES`
/// table and exit. Does nothing when no `-p`/`--print` prompt is present.
fn emit_workflow_response(table_path: &str, args: &[String]) {
    let Some(prompt) = args
        .iter()
        .position(|a| a == "-p" || a == "--print")
        .and_then(|i| args.get(i + 1))
    else {
        return;
    };
    let table: serde_json::Value = std::fs::read_to_string(table_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    let reply = table
        .as_object()
        .and_then(|m| m.iter().find(|(k, _)| prompt.contains(k.as_str())))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| serde_json::json!({"result": format!("echo: {prompt}")}));
    if let Ok(mut calls) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{table_path}.calls"))
    {
        let schema = args
            .iter()
            .position(|a| a == "--json-schema")
            .and_then(|i| args.get(i + 1))
            .cloned();
        let _ = writeln!(calls, "{}", serde_json::json!({"prompt": prompt, "schema": schema}));
    }
    let mut result = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "num_turns": 1,
        "duration_ms": 5,
        "total_cost_usd": 0.01,
        "is_error": reply.get("is_error").cloned().unwrap_or(serde_json::json!(false)),
        "result": reply.get("result").cloned().unwrap_or(serde_json::json!("")),
    });
    if let Some(structured) = reply.get("structured_output") {
        result["structured_output"] = structured.clone();
    }
    let out = io::stdout();
    let mut out = out.lock();
    let _ = writeln!(out, "{}", serde_json::json!({"type": "system", "subtype": "init", "model": "mock"}));
    let _ = writeln!(out, "{result}");
    let _ = out.flush();
    std::process::exit(0);
}

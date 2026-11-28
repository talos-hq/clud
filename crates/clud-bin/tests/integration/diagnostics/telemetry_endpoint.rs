//! End-to-end integration test for the telemetry sink (issue #469).
//!
//! Spawns the daemon's HTTP listener via [`spawn_dashboard`], invokes
//! `clud log --cmd <known> --fail-on-no-server` against it with a
//! `CLUD_TEST_MARKER` env var, and asserts:
//!
//! 1. `clud log` exited 0 (proves the POST round-tripped).
//! 2. `GET /telemetry` returns one summary entry keyed by the test
//!    process's PID (issue #471 — moved off `/state.json#telemetry`).
//! 3. `GET /telemetry/by-pid/<pid>` returns the full entry including
//!    the captured `CLUD_*` env vars.
//!
//! Validates the entire chain: CLI → HTTP POST → in-memory sink →
//! dashboard read.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use clud::daemon::{
    spawn_dashboard_telemetry_only, TelemetryPidDetail, TelemetryPidSummary, TelemetryStore,
};
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

use crate::exe;

/// Path to the `clud` binary under test.
///
/// Cargo sets `CARGO_BIN_EXE_<bin>` at compile time, which is correct when the
/// harness runs on the machine that built it. CI compiles harnesses on Linux
/// and runs them on native macOS/Windows runners, so `CLUD_TEST_BIN_DIR` takes
/// precedence when set — see `common/exe.rs`.
fn clud_exe() -> PathBuf {
    exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn telemetry_round_trip_via_clud_log_subprocess() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let telemetry = TelemetryStore::new();
    let port = spawn_dashboard_telemetry_only(
        dir.path().to_path_buf(),
        9999,
        100,
        telemetry.clone(),
        "test-capability".to_string(),
    )
    .expect("dashboard spawned");

    // Invoke `clud log` against the spawned port. --fail-on-no-server
    // guarantees a non-zero exit if the POST doesn't round-trip, which
    // is exactly what we want for an end-to-end proof.
    let known_cmd = "telemetry-test-marker-xyz";
    let url = format!("http://127.0.0.1:{port}");

    // Build the env: inherit current process's, drop any pre-existing
    // CLUD_DAEMON_HTTP_SERVER, then layer in the two we care about so
    // the test can assert the marker round-tripped.
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.retain(|(k, _)| {
        k != "CLUD_DAEMON_HTTP_SERVER"
            && k != "CLUD_TEST_MARKER"
            && k != "CLUD_DAEMON_HTTP_TOKEN"
            && k != "RUNNING_PROCESS_ORIGINATOR"
    });
    env.push(("CLUD_DAEMON_HTTP_SERVER".to_string(), url.clone()));
    env.push(("CLUD_TEST_MARKER".to_string(), "42".to_string()));
    env.push((
        "CLUD_DAEMON_HTTP_TOKEN".to_string(),
        "test-capability".to_string(),
    ));

    let config = ProcessConfig {
        command: CommandSpec::Argv(vec![
            clud_exe().to_string_lossy().into_owned(),
            "log".to_string(),
            "--cmd".to_string(),
            known_cmd.to_string(),
            "--fail-on-no-server".to_string(),
        ]),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new(config);
    process.start().expect("spawn clud log");

    let mut stdout = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let exit_code = loop {
        match process.read_combined(Some(Duration::from_millis(50))) {
            ReadStatus::Line(event) => {
                stdout.push_str(&String::from_utf8_lossy(&event.line));
                stdout.push('\n');
            }
            ReadStatus::Timeout | ReadStatus::Eof => {}
        }
        match process.poll().expect("poll") {
            Some(code) => break code,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = process.kill();
                    panic!("clud log did not exit within 10s; stdout so far:\n{stdout}");
                }
            }
        }
    };
    assert_eq!(
        exit_code, 0,
        "clud log --fail-on-no-server should round-trip the POST; got exit={exit_code}\nstdout:\n{stdout}",
    );

    // Issue #471: telemetry summary now lives at its own URL — fetch
    // from `/telemetry` directly instead of pulling it out of the
    // consolidated state document.
    let summary_body = fetch_path(port, "GET", "/telemetry", None).expect("GET /telemetry");
    let summaries: Vec<TelemetryPidSummary> =
        serde_json::from_str(&summary_body).expect("parse summary");
    assert_eq!(
        summaries.len(),
        1,
        "expected exactly one PID summary, got {}: {summaries:?}",
        summaries.len(),
    );
    let summary = &summaries[0];
    assert_eq!(
        summary.entry_count, 1,
        "expected 1 entry, got {}",
        summary.entry_count
    );
    let test_pid = std::process::id();
    assert_eq!(
        summary.parent_pid, test_pid,
        "parent_pid should be the test process's PID ({test_pid}), got {}",
        summary.parent_pid
    );

    // Assert /telemetry/by-pid/<pid> returns the full entry with our env marker.
    let detail_path = format!("/telemetry/by-pid/{}", summary.parent_pid);
    let detail_body = fetch_path(port, "GET", &detail_path, None).expect("GET detail");
    let detail: TelemetryPidDetail = serde_json::from_str(&detail_body).expect("parse detail");
    assert_eq!(detail.entries.len(), 1);
    let entry = &detail.entries[0];
    assert_eq!(entry.cmd, known_cmd);
    assert_eq!(
        entry.env.get("CLUD_TEST_MARKER").map(String::as_str),
        Some("42"),
        "CLUD_TEST_MARKER missing from captured env: {:?}",
        entry.env
    );
    assert!(
        !entry.env.contains_key("CLUD_DAEMON_HTTP_TOKEN"),
        "dashboard capability leaked into telemetry: {:?}",
        entry.env
    );
    // All captured env keys must start with CLUD_ (the logger filters).
    assert!(
        entry.env.keys().all(|k| k.starts_with("CLUD_")),
        "non-CLUD_ env keys leaked through: {:?}",
        entry.env.keys().collect::<Vec<_>>()
    );
}

#[test]
fn clud_log_no_env_with_fail_flag_exits_nonzero() {
    // Without CLUD_DAEMON_HTTP_SERVER, --fail-on-no-server must exit nonzero.
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.retain(|(k, _)| k != "CLUD_DAEMON_HTTP_SERVER");

    let config = ProcessConfig {
        command: CommandSpec::Argv(vec![
            clud_exe().to_string_lossy().into_owned(),
            "log".to_string(),
            "--cmd".to_string(),
            "noserver".to_string(),
            "--fail-on-no-server".to_string(),
        ]),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new(config);
    process.start().expect("spawn clud log");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let exit_code = loop {
        match process.read_combined(Some(Duration::from_millis(50))) {
            ReadStatus::Line(_) | ReadStatus::Timeout | ReadStatus::Eof => {}
        }
        match process.poll().expect("poll") {
            Some(code) => break code,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = process.kill();
                    panic!("clud log did not exit within 5s");
                }
            }
        }
    };
    assert_ne!(exit_code, 0, "expected non-zero exit without env var");
}

#[test]
fn clud_log_unreachable_server_with_fail_flag_exits_nonzero() {
    // Point at a port nothing is listening on. --fail-on-no-server must
    // surface the connection failure.
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.retain(|(k, _)| k != "CLUD_DAEMON_HTTP_SERVER");
    // Port 1 is reserved (tcpmux) and reliably refuses connections on
    // every CI runner the matrix targets.
    env.push((
        "CLUD_DAEMON_HTTP_SERVER".to_string(),
        "http://127.0.0.1:1".to_string(),
    ));

    let config = ProcessConfig {
        command: CommandSpec::Argv(vec![
            clud_exe().to_string_lossy().into_owned(),
            "log".to_string(),
            "--cmd".to_string(),
            "unreachable".to_string(),
            "--fail-on-no-server".to_string(),
        ]),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new(config);
    process.start().expect("spawn clud log");

    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    let exit_code = loop {
        match process.read_combined(Some(Duration::from_millis(50))) {
            ReadStatus::Line(_) | ReadStatus::Timeout | ReadStatus::Eof => {}
        }
        match process.poll().expect("poll") {
            Some(code) => break code,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = process.kill();
                    panic!("clud log did not exit within 6s");
                }
            }
        }
    };
    assert_ne!(exit_code, 0, "expected non-zero exit on unreachable server");
}

/// Issue #471: `GET /telemetry` returns the per-PID summary list
/// independently of `/state.json`. Asserts the empty case + that two
/// distinct PIDs from two POSTs both appear, sorted last-seen-desc.
#[test]
fn telemetry_summary_endpoint_returns_independent_list() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let telemetry = TelemetryStore::new();
    let port = spawn_dashboard_telemetry_only(
        dir.path().to_path_buf(),
        9999,
        100,
        telemetry.clone(),
        "test-capability".to_string(),
    )
    .expect("dashboard spawned");

    // Empty case: no POSTs yet → empty array.
    let empty_body = fetch_path(port, "GET", "/telemetry", None).expect("GET /telemetry empty");
    let empty: Vec<TelemetryPidSummary> = serde_json::from_str(&empty_body).expect("parse empty");
    assert!(empty.is_empty(), "expected empty array, got {empty:?}");

    // POST two entries from two distinct PIDs.
    for (pid, ts) in [(111u32, 1_700_000_001_000u64), (222, 1_700_000_002_000)] {
        let body =
            format!(r#"{{"parent_pid":{pid},"time_ms":{ts},"cmd":"e","cwd":"/tmp","env":{{}}}}"#);
        let resp =
            fetch_path(port, "POST", "/telemetry/log", Some(body)).expect("POST /telemetry/log");
        assert!(resp.contains("{}"), "expected ack {{}}, got: {resp}");
    }

    let body = fetch_path(port, "GET", "/telemetry", None).expect("GET /telemetry populated");
    let summaries: Vec<TelemetryPidSummary> = serde_json::from_str(&body).expect("parse populated");
    assert_eq!(summaries.len(), 2);
    // Sort order is last_at_ms desc — newer (received later) comes first.
    // Both POSTs happened in the same test second, so use parent_pid as a
    // tie-breaker assertion: the SET equals {111, 222}.
    let pids: std::collections::BTreeSet<u32> = summaries.iter().map(|s| s.parent_pid).collect();
    assert_eq!(pids, [111, 222].into_iter().collect());
    assert!(
        summaries.iter().all(|s| s.entry_count == 1),
        "each PID should have exactly one entry: {summaries:?}"
    );
    // Sanity: /state.json must NOT contain a `telemetry` field anymore.
    let state_body = fetch_path(port, "GET", "/state.json", None).expect("GET /state.json");
    let state: serde_json::Value = serde_json::from_str(&state_body).expect("parse state");
    assert!(
        state.get("telemetry").is_none(),
        "/state.json should no longer carry telemetry; got {state}"
    );
}

/// Tiny HTTP/1.0 client for the test. Mirrors the helper in
/// `daemon::http::tests` so we don't need a real HTTP client dep.
fn fetch_path(port: u16, method: &str, path: &str, body: Option<String>) -> io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut req = format!(
        "{method} {path} HTTP/1.0\r\nHost: localhost:{port}\r\nCookie: clud_dashboard_token=test-capability\r\nConnection: close\r\n",
    );
    if let Some(b) = &body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            b.len(),
            b
        ));
    } else {
        req.push_str("\r\n");
    }
    stream.write_all(req.as_bytes())?;
    stream.flush()?;
    let mut buf = Vec::with_capacity(4096);
    stream.read_to_end(&mut buf)?;
    // Find the body start.
    let body_start = (0..buf.len().saturating_sub(3))
        .find(|&i| &buf[i..i + 4] == b"\r\n\r\n")
        .map(|i| i + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    String::from_utf8(buf[body_start..].to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

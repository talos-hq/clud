//! Cross-platform behavior tests for `running_process::pty::NativePtyProcess`
//! as used by `clud --codex` (see zackees/clud#28, #31).
//!
//! Each test asserts the platform-specific contract that
//! `running-process-core` 3.1.0 actually exposes today. When a theory from
//! #31 predicts the *wrong* behavior, the test asserts that wrong behavior
//! so the fix can be landed as a test flip rather than a quiet regression.
//!
//! Theories covered:
//!   T1 — `respond_to_queries_impl` DSR stub. Windows writes a hardcoded
//!        `\x1b[1;1R` per query into the PTY input and POSIX is a no-op, but
//!        on a real ConPTY the stub never reaches the child: ConPTY takes a
//!        cursor-position report on its input as the answer to its own DSR
//!        (#1310, first observed once these tests stopped skipping on CI).
//!        So on every platform the child sees nothing. clud's fix: stop
//!        calling it (session.rs / daemon.rs).
//!   T2 — `resize_impl` is a no-op on Windows; forwards on POSIX.
//!        clud's fix: `session::resize_pty` reaches master.resize() directly.
//!   T3 — Spawn accepts `cols=32767` (the old clud fallback) without panicking.
//!        clud's fix: `resolve_terminal_size` now caps at 200 cols.
//!
//! ## Host-environment requirement
//!
//! On Windows, `CreatePseudoConsole` behaves oddly when the spawning process's
//! stdout is redirected (not a real console) — see
//! microsoft/terminal discussions around STARTF_USESTDHANDLES. In that case
//! the child's output never reaches the master reader and these tests time
//! out with 4 bytes of `\x1b[6n` and nothing else.
//!
//! To keep the suite green in such environments (piped `cargo test`, nested
//! shells, some CI runners), every test runs a one-shot `pty_canary()` first.
//! If the canary fails, the test logs a diagnostic and returns early rather
//! than panicking. On a real Windows Terminal / cmd / pwsh session, on Linux,
//! and on macOS, the canary passes and the real assertions run.
//!
//! Raw-PTY-pump integration tests live in `tests/integration/pty/pty_pump.rs`; shared
//! harness helpers are in `tests/integration/common/mod.rs`.

use std::time::Duration;

use running_process::pty::NativePtyProcess;
use serde_json::Value;

use crate::common::{
    cargo_built_executable_path, drain_reader, mock_agent_path, wait_answering_cursor_queries,
};

#[test]
fn cargo_build_output_reports_mock_agent_executable() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let exe = tmp.path().join(if clud::platform::host::WINDOWS {
        "mock-agent.exe"
    } else {
        "mock-agent"
    });
    std::fs::write(&exe, b"binary").expect("write mock binary");

    let output = format!(
        "{{\"reason\":\"compiler-artifact\",\"target\":{{\"name\":\"mock-agent\",\"kind\":[\"bin\"]}},\"executable\":{}}}\n",
        serde_json::to_string(&exe.to_string_lossy().to_string()).expect("json string")
    );

    assert_eq!(cargo_built_executable_path(&output), Some(exe));
}

// ─────────────────────────────────────────────────────────────────────────
// T1 — respond_to_queries_impl DSR behavior
// ─────────────────────────────────────────────────────────────────────────

/// Feed one `\x1b[6n` DSR query into the `respond_to_queries_impl` handler
/// and assert what the PTY child actually received on stdin.
///
/// - Windows: handler writes one hardcoded `\x1b[1;1R` into the PTY input
///   (issue #31, theory T1), and ConPTY consumes it as a cursor-position
///   report, so the child receives nothing (#1310).
/// - POSIX: handler is a no-op; the child receives zero bytes.
#[test]
fn respond_to_queries_matches_platform_stub() {
    require_pty_or_skip!("respond_to_queries_matches_platform_stub");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "600".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");

    // Let the child enter its stdin read loop.
    std::thread::sleep(Duration::from_millis(200));

    process
        .respond_to_queries_impl(b"prefix\x1b[6nsuffix")
        .expect("respond_to_queries");

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(500));
    let _ = process.close_impl();

    let got = std::fs::read(&raw_stdin).unwrap_or_default();

    assert!(
        got.is_empty(),
        "no DSR reply may reach the child (POSIX: no-op; Windows: ConPTY consumes the stub); \
         child received {:?}",
        got
    );
}

/// A chunk containing N DSR queries still delivers nothing to the child on
/// any platform: Windows' N stubs are consumed by ConPTY (#1310).
#[test]
fn respond_to_queries_is_linear_in_query_count() {
    require_pty_or_skip!("respond_to_queries_is_linear_in_query_count");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "600".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    std::thread::sleep(Duration::from_millis(200));

    process
        .respond_to_queries_impl(b"\x1b[6nA\x1b[6nB\x1b[6n")
        .expect("respond_to_queries");

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(500));
    let _ = process.close_impl();

    let got = std::fs::read(&raw_stdin).unwrap_or_default();

    assert!(
        got.is_empty(),
        "no DSR reply may reach the child regardless of query count; got {:?}",
        got
    );
}

// ─────────────────────────────────────────────────────────────────────────
// T2 — resize_impl behavior
// ─────────────────────────────────────────────────────────────────────────

/// Spawn mock-agent in a PTY with a known size and assert the child sees
/// those dimensions via the `terminal_size` crate. Baseline: the axes must
/// match on POSIX before the resize test below is meaningful.
#[test]
fn initial_pty_size_is_forwarded_to_child() {
    require_pty_or_skip!("initial_pty_size_is_forwarded_to_child");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let size_report = tmp.path().join("size.json");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-report-pty-size".to_string(),
        size_report.to_string_lossy().to_string(),
        "--mock-pty-size-samples".to_string(),
        "1".to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 30, 100, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");

    // #1310: answer ConPTY's startup cursor query while waiting, or the
    // Windows child never runs and the report never appears.
    let wait = wait_answering_cursor_queries(&process, Duration::from_secs(10), || {
        std::fs::metadata(&size_report)
            .map(|m| m.len() > 2)
            .unwrap_or(false)
    });

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    assert!(
        wait.met,
        "child never wrote its size report; output: {:?}",
        String::from_utf8_lossy(&wait.output)
    );
    let body = std::fs::read_to_string(&size_report).expect("read size report");
    let samples: Value = serde_json::from_str(&body).expect("parse size report");
    let samples = samples.as_array().expect("array");
    assert!(!samples.is_empty(), "no samples recorded");

    let first = &samples[0];
    assert_eq!(
        first["cols"].as_u64(),
        Some(100),
        "PTY cols mismatch: {:?}",
        first
    );
    assert_eq!(
        first["rows"].as_u64(),
        Some(30),
        "PTY rows mismatch: {:?}",
        first
    );
}

/// Document what `running_process::pty::NativePtyProcess::resize_impl`
/// does today on each platform:
///   - POSIX: `master.resize()` propagates; the child sees the new size.
///   - Windows: intentional no-op (see running-process-core mod.rs:730-737).
///
/// clud no longer relies on this API on Windows — `session::resize_pty`
/// reaches the underlying `portable_pty::MasterPty::resize()` directly
/// (issue #31 T2 fix). This test locks the *library* contract so a future
/// library fix that enables Windows resize makes the workaround obsolete
/// and this assertion flips.
#[test]
fn resize_impl_propagates_on_posix_and_noops_on_windows() {
    require_pty_or_skip!("resize_impl_propagates_on_posix_and_noops_on_windows");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let size_report = tmp.path().join("size.json");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-report-pty-size".to_string(),
        size_report.to_string_lossy().to_string(),
        "--mock-pty-size-samples".to_string(),
        "3".to_string(),
        "--mock-pty-size-interval-ms".to_string(),
        "250".to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 20, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");

    // #1310: answer ConPTY's startup cursor query while waiting.
    let wait = wait_answering_cursor_queries(&process, Duration::from_secs(10), || {
        std::fs::metadata(&size_report)
            .map(|m| m.len() > 2)
            .unwrap_or(false)
    });
    if !wait.met {
        let _ = process.close_impl();
        panic!(
            "child never wrote its first size sample; output: {:?}",
            String::from_utf8_lossy(&wait.output)
        );
    }

    std::thread::sleep(Duration::from_millis(80));
    process.resize_impl(40, 120).expect("resize");

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    let body = std::fs::read_to_string(&size_report).unwrap_or_default();
    let samples: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let samples = match samples.as_array() {
        Some(arr) if !arr.is_empty() => arr.clone(),
        _ => panic!("size report has no samples: {body:?}"),
    };

    let first = &samples[0];
    let last = samples.last().expect("last sample");

    if clud::platform::host::UNIX {
        assert_eq!(
            last["cols"].as_u64(),
            Some(120),
            "POSIX resize_impl did not propagate cols: {:?}",
            samples
        );
        assert_eq!(
            last["rows"].as_u64(),
            Some(40),
            "POSIX resize_impl did not propagate rows: {:?}",
            samples
        );
    } else {
        // Windows: resize_impl is a no-op. The child's observed size MUST
        // NOT have changed to (120, 40). A `None` observation (headless
        // ConPTY) also satisfies "did not change".
        let changed = last["cols"].as_u64() == Some(120) && last["rows"].as_u64() == Some(40);
        assert!(
            !changed,
            "Windows resize_impl unexpectedly propagated (fix landed? flip this test): first={:?} last={:?}",
            first, last
        );
    }
}

/// `clud::session::resize_pty` must change the master's reported size on
/// every platform, including Windows (where the library's own `resize_impl`
/// is a no-op). Issue #31, theory T2. Lives here rather than in the `--lib`
/// tests so `require_pty_or_skip!` turns a PTY failure into a hard failure
/// under `CLUD_REQUIRE_PTY=1` instead of a silent skip (#1348).
#[test]
fn resize_pty_updates_master_size_on_all_platforms() {
    require_pty_or_skip!("resize_pty_updates_master_size_on_all_platforms");

    let argv: Vec<String> = if clud::platform::host::WINDOWS {
        // `ping -n 3 127.0.0.1` keeps the child alive ~2s without needing
        // a console for stdout, which is enough for a resize roundtrip.
        vec![
            "cmd.exe".into(),
            "/c".into(),
            "ping -n 3 127.0.0.1 > NUL".into(),
        ]
    } else {
        vec!["/bin/sh".into(), "-c".into(), "sleep 2".into()]
    };

    let process = NativePtyProcess::new(argv, None, None, 20, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");

    // Sanity: the master reports the initial size we requested.
    {
        let guard = process.handles.lock().expect("handles");
        let handles = guard.as_ref().expect("handles present");
        let before = handles.master.get_size().expect("get_size");
        assert_eq!(
            (before.rows, before.cols),
            (20, 80),
            "initial master size wrong: {:?}",
            before
        );
    }

    // Resize via the helper and verify the master advances.
    clud::session::resize_pty(&process, 40, 120).expect("resize_pty");

    {
        let guard = process.handles.lock().expect("handles");
        let handles = guard.as_ref().expect("handles present");
        let after = handles.master.get_size().expect("get_size");
        assert_eq!(
            (after.rows, after.cols),
            (40, 120),
            "resize_pty did not propagate to master: {:?}",
            after
        );
    }

    let _ = process.close_impl();
}

// ─────────────────────────────────────────────────────────────────────────
// T3 — extreme `cols` values
// ─────────────────────────────────────────────────────────────────────────

/// clud's `get_terminal_size()` fallback returns `cols = 32767` when stdout
/// isn't a terminal (main.rs:137-145). Verify portable-pty accepts this
/// without panicking at spawn — even if the child's layout math goes
/// sideways on the value. See issue #31, theory T3.
#[test]
fn extreme_cols_does_not_crash_at_spawn() {
    require_pty_or_skip!("extreme_cols_does_not_crash_at_spawn");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let size_report = tmp.path().join("size.json");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-report-pty-size".to_string(),
        size_report.to_string_lossy().to_string(),
        "--mock-pty-size-samples".to_string(),
        "1".to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 32767, None).expect("new pty");
    process.set_echo(false);
    process
        .start_impl()
        .expect("portable-pty rejected cols=32767 at spawn");

    // #1310: answer ConPTY's startup cursor query, or the Windows child never
    // runs and this waits out the full timeout.
    let _ = wait_answering_cursor_queries(&process, Duration::from_secs(5), || {
        std::fs::metadata(&size_report)
            .map(|m| m.len() > 2)
            .unwrap_or(false)
    });
    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    // If the child reported a size at all, it must be positive. We are not
    // asserting the exact value — portable-pty may clamp or pass through.
    // The load-bearing claim is "start_impl did not panic/error".
    if let Ok(body) = std::fs::read_to_string(&size_report) {
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&body) {
            if let Some(first) = arr.first() {
                if let Some(cols) = first["cols"].as_u64() {
                    assert!(cols > 0, "cols must be positive when reported: {}", cols);
                }
            }
        }
    }
}

//! Integration test for `clud symbols`.
//!
//! Drops two canned crash-report JSON files into a tempdir, points
//! `CLUD_DAEMON_STATE_DIR` at it, then exercises the subcommands as a
//! subprocess of the installed `clud` binary so the full clap routing
//! is covered.

use std::fs;
use std::time::Duration;

use kernal_api::platform::fs::TemporaryDirectory;
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

use crate::exe;

fn write_report(dir: &std::path::Path, name: &str, backtrace: &str) {
    let path = dir.join(name);
    let body = serde_json::json!({
        "version": "0.0.0",
        "role": "test",
        "kind": "panic",
        "pid": 12345,
        "cwd": null,
        "args": [],
        "timestamp_unix_ms": 100u64,
        "panic_message": "synthetic",
        "backtrace": backtrace,
    });
    fs::write(&path, body.to_string()).expect("write report");
}

/// Spawn `clud <args>` via running_process::NativeProcess, drain its
/// merged stdout+stderr into a `String`, and return `(exit_code, output)`.
fn run_clud(args: &[&str], state_dir: &std::path::Path) -> (i32, String) {
    let clud = exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"));
    let mut argv = vec![clud.to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|s| s.to_string()));

    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.retain(|(k, _)| k != "CLUD_DAEMON_STATE_DIR" && k != "CLUD_ALLOW_DAEMON_SPAWN");
    env.push((
        "CLUD_DAEMON_STATE_DIR".to_string(),
        state_dir.to_string_lossy().into_owned(),
    ));

    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().expect("spawn clud");

    let mut buf = Vec::<u8>::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => {
                buf.extend_from_slice(&event.line);
                buf.push(b'\n');
            }
            ReadStatus::Timeout => {
                if process.returncode().is_some() {
                    break;
                }
            }
            ReadStatus::Eof => break,
        }
    }
    let exit = process
        .wait(Some(Duration::from_secs(15)))
        .expect("wait clud");
    // `wait` ran the bounded capture drain; keep lines that trailed the exit (#2159).
    for event in process.drain_combined() {
        buf.extend_from_slice(&event.line);
        buf.push(b'\n');
    }
    (exit, String::from_utf8_lossy(&buf).into_owned())
}

#[test]
fn symbols_verify_all_passes_when_every_report_has_file_line_frames() {
    let tmp = TemporaryDirectory::new().unwrap();
    let state_dir = tmp.path().join("state");
    let crashes_dir = state_dir.join("crashes");
    fs::create_dir_all(&crashes_dir).unwrap();
    write_report(
        &crashes_dir,
        "100-test-1.json",
        "   0: clud::main::h1234\n             at /home/u/clud/src/main.rs:42:5\n",
    );
    write_report(
        &crashes_dir,
        "200-test-2.json",
        "   0: clud::main::h5678\n             at /home/u/clud/src/lib.rs:99:1\n",
    );

    let (exit, output) = run_clud(&["symbols", "verify", "--all"], &state_dir);
    assert_eq!(exit, 0, "expected exit 0; output: {output}");
    assert!(
        output.contains("clud symbols: OK"),
        "expected OK footer; got: {output}"
    );
}

#[test]
fn symbols_verify_all_fails_when_any_report_is_unsymbolicated() {
    let tmp = TemporaryDirectory::new().unwrap();
    let state_dir = tmp.path().join("state");
    let crashes_dir = state_dir.join("crashes");
    fs::create_dir_all(&crashes_dir).unwrap();
    write_report(
        &crashes_dir,
        "100-test-1.json",
        "   0: clud::main::h1234\n             at /home/u/clud/src/main.rs:42:5\n",
    );
    write_report(
        &crashes_dir,
        "200-test-2.json",
        "   0: 0x7fffabcd1234\n   1: 0x7fffabcd5678\n",
    );

    let (exit, output) = run_clud(&["symbols", "verify", "--all"], &state_dir);
    assert_ne!(exit, 0, "expected non-zero exit; output: {output}");
    assert!(
        output.contains("clud symbols: FAIL"),
        "expected FAIL footer; got: {output}"
    );
}

/// `verify`, not `install`, is what inspects.
///
/// This test used to drive `install`, which was correct while `install` was an
/// alias for `verify`. #1016 gave `install` its own job — fetch the sidecar
/// the newest report needs — so the inspect-only-the-newest property now
/// belongs to `verify`, and asserting it through `install` would be asserting
/// the alias rather than the behaviour.
#[test]
fn symbols_verify_inspects_only_most_recent_report() {
    let tmp = TemporaryDirectory::new().unwrap();
    let state_dir = tmp.path().join("state");
    let crashes_dir = state_dir.join("crashes");
    fs::create_dir_all(&crashes_dir).unwrap();
    // Older report is unsymbolicated; newest is symbolicated. `install`
    // should only look at the newest and return OK.
    write_report(
        &crashes_dir,
        "100-test-old.json",
        "   0: 0x7fffabcd1234\n   1: 0x7fffabcd5678\n",
    );
    write_report(
        &crashes_dir,
        "999-test-new.json",
        "   0: clud::main::h1234\n             at /home/u/clud/src/main.rs:42:5\n",
    );

    let (exit, output) = run_clud(&["symbols", "verify"], &state_dir);
    assert_eq!(exit, 0, "expected exit 0; output: {output}");
    assert!(
        output.contains("999-test-new.json"),
        "should mention newest report; got: {output}"
    );
    assert!(
        !output.contains("100-test-old.json"),
        "should NOT mention older report; got: {output}"
    );
}

#[test]
fn symbols_bare_prints_summary() {
    let tmp = TemporaryDirectory::new().unwrap();
    let state_dir = tmp.path().join("state");
    let crashes_dir = state_dir.join("crashes");
    fs::create_dir_all(&crashes_dir).unwrap();
    write_report(
        &crashes_dir,
        "100-test-1.json",
        "   0: clud::main::h1234\n             at /home/u/clud/src/main.rs:42:5\n",
    );
    write_report(
        &crashes_dir,
        "200-test-2.json",
        "   0: 0x7fffabcd1234\n   1: 0x7fffabcd5678\n",
    );

    let (exit, output) = run_clud(&["symbols"], &state_dir);
    assert_eq!(exit, 0, "bare summary should exit 0");
    assert!(output.contains("total reports: 2"), "got: {output}");
    assert!(
        output.contains("reports with file:line frames: 1"),
        "got: {output}"
    );
    assert!(
        output.contains("reports without file:line frames: 1"),
        "got: {output}"
    );
}

/// `install` on a report that predates #1016 says so rather than guessing.
///
/// The version and target are what name the release asset; without them there
/// is no sidecar to ask for. Guessing at this binary's own version would fetch
/// symbols for the wrong build, which mis-symbolicates with confident, wrong
/// line numbers — the failure #1016 argues is worse than none. It is a skip
/// with a reason, not an error: an old report is a normal thing to have.
#[test]
fn symbols_install_reports_a_pre_1016_report_rather_than_guessing() {
    let tmp = TemporaryDirectory::new().unwrap();
    let state_dir = tmp.path().join("state");
    let crashes_dir = state_dir.join("crashes");
    fs::create_dir_all(&crashes_dir).unwrap();
    // `write_report` writes no version/target, which is exactly the shape of
    // a report written before those fields existed.
    write_report(
        &crashes_dir,
        "999-test-new.json",
        "   0: clud::main::h1234\n             at /home/u/clud/src/main.rs:42:5\n",
    );

    let (exit, output) = run_clud(&["symbols", "install"], &state_dir);

    assert_eq!(exit, 0, "expected exit 0; output: {output}");
    assert!(
        output.contains("version/target"),
        "the reason must name what is missing; got: {output}"
    );
}

/// No reports at all is a normal state, and `install` must not treat it as a
/// failure or reach for the network to find out.
#[test]
fn symbols_install_with_no_reports_is_not_an_error() {
    let tmp = TemporaryDirectory::new().unwrap();
    let state_dir = tmp.path().join("state");
    fs::create_dir_all(state_dir.join("crashes")).unwrap();

    let (exit, output) = run_clud(&["symbols", "install"], &state_dir);

    assert_eq!(exit, 0, "expected exit 0; output: {output}");
}

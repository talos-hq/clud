//! A hook payload on plain `clud` stdin must never launch a harness. The mock
//! `claude` prints [`MOCK_RAN`] when spawned and `CLUD_SESSION_DB` names a file
//! that only a session registration creates.

use crate::{common, exe};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

const PAYLOAD: &str = r#"{"session_id":"s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo hi"}}"#;

/// The last line of every mock-agent run.
const MOCK_RAN: &str = "[done]";

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    session_db: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let home = root.join("home");
    let bin = root.join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let claude = bin.join(if clud::platform::host::WINDOWS {
        "claude.exe"
    } else {
        "claude"
    });
    fs::copy(common::mock_agent_path(), &claude).unwrap();
    Fixture {
        _root: dir,
        session_db: root.join("sessions.redb"),
        root,
        home,
        bin,
    }
}

fn clud_path() -> String {
    exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
        .to_string_lossy()
        .into_owned()
}

fn run(fx: &Fixture, argv: Vec<String>, stdin: &str) -> (i32, String) {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| {
            !["HOME", "USERPROFILE", "PATH"]
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
        })
        .collect();
    let home = fx.home.to_string_lossy().into_owned();
    env.push(("HOME".to_string(), home.clone()));
    env.push(("USERPROFILE".to_string(), home));
    env.push(("PATH".to_string(), fx.bin.to_string_lossy().into_owned()));
    env.push(("CLUD_SERVER_SETTINGS".to_string(), "0".to_string()));
    env.push((
        "CLUD_SESSION_DB".to_string(),
        fx.session_db.to_string_lossy().into_owned(),
    ));
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: Some(fx.root.clone()),
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Piped,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().expect("spawn clud");
    process.write_stdin(stdin.as_bytes()).expect("write stdin");
    let mut output = String::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => {
                output.push_str(&String::from_utf8_lossy(&event.line));
                output.push('\n');
            }
            ReadStatus::Timeout if process.returncode().is_some() => break,
            ReadStatus::Timeout => {}
            ReadStatus::Eof => break,
        }
    }
    let exit = process
        .wait(Some(Duration::from_secs(60)))
        .expect("wait for clud");
    for event in process.drain_combined() {
        output.push_str(&String::from_utf8_lossy(&event.line));
        output.push('\n');
    }
    (exit, output)
}

fn no_daemon() -> Vec<String> {
    vec![clud_path(), "--no-daemon".to_string()]
}

fn assert_not_launched(fx: &Fixture, exit: i32, output: &str) {
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("clud doctor"), "{output}");
    assert!(!output.contains(MOCK_RAN), "a harness launched:\n{output}");
    assert!(!fx.session_db.exists(), "a session registered:\n{output}");
}

#[test]
fn a_hook_payload_on_bare_clud_launches_nothing() {
    let fx = fixture();
    let (exit, output) = run(&fx, vec![clud_path()], PAYLOAD);
    assert_not_launched(&fx, exit, &output);
}

#[test]
fn a_hook_payload_with_launch_flags_launches_nothing() {
    let fx = fixture();
    let (exit, output) = run(&fx, no_daemon(), PAYLOAD);
    assert_not_launched(&fx, exit, &output);
}

#[test]
fn a_compiled_event_line_that_lost_its_alias_launches_nothing() {
    let fx = fixture();
    let argv = vec![clud_path(), "--event".to_string(), "Stop".to_string()];
    let (exit, output) = run(&fx, argv, r#"{"hook_event_name":"Stop"}"#);
    assert_not_launched(&fx, exit, &output);
}

#[test]
fn a_launcher_script_that_loses_argv0_launches_nothing() {
    if clud::platform::host::WINDOWS {
        return;
    }
    let fx = fixture();
    let stub = fx.bin.join("clud-cmd-scan");
    fs::write(&stub, format!("#!/bin/sh\nexec {} \"$@\"\n", clud_path())).unwrap();
    let argv = vec!["/bin/sh".to_string(), stub.to_string_lossy().into_owned()];
    let (exit, output) = run(&fx, argv, PAYLOAD);
    assert_not_launched(&fx, exit, &output);
}

#[test]
fn a_denied_command_still_gets_its_verdict() {
    let fx = fixture();
    let payload = r#"{"session_id":"s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"while true; do sleep 1; done"}}"#;
    let (exit, output) = run(&fx, vec![clud_path()], payload);
    assert!(!fx.session_db.exists(), "{output}");
    assert!(
        output.contains("clud doctor") && (exit == 2 || output.contains("deny")),
        "exit {exit}, no denial:\n{output}"
    );
}

#[test]
fn a_prompt_mentioning_the_key_still_launches() {
    let fx = fixture();
    let (_, output) = run(&fx, no_daemon(), "what is hook_event_name?");
    assert!(output.contains(MOCK_RAN), "no launch:\n{output}");
    assert!(output.contains("what is hook_event_name?"), "{output}");
    assert!(
        fx.session_db.exists(),
        "the DB probe proves nothing:\n{output}"
    );
    assert!(!output.contains("clud doctor"), "{output}");
}

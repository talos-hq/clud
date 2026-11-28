//! Compiled process boundary regression for #1094. The mock Claude marker is
//! the observable child-spawn fact: before the admission preflight this route
//! launched the mock and failed only on its first bridge request.

use crate::{common, exe};
use std::fs;
use std::time::Duration;

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

fn run(home: &std::path::Path, path: &std::path::Path, args: &[String]) -> (i32, String) {
    let mut argv = vec![exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
        .to_string_lossy()
        .into_owned()];
    argv.extend(args.iter().cloned());
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| {
            !key.eq_ignore_ascii_case("HOME")
                && !key.eq_ignore_ascii_case("USERPROFILE")
                && !key.eq_ignore_ascii_case("OPENAI_API_KEY")
                && !key.eq_ignore_ascii_case("PATH")
        })
        .collect();
    let home = home.to_string_lossy().into_owned();
    env.push(("HOME".to_string(), home.clone()));
    env.push(("USERPROFILE".to_string(), home));
    env.push(("PATH".to_string(), path.to_string_lossy().into_owned()));
    // Never fetch server settings during a test (#1192).
    env.push(("CLUD_SERVER_SETTINGS".to_string(), "0".to_string()));

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
        .wait(Some(Duration::from_secs(30)))
        .expect("wait for clud");
    // `wait` ran the bounded capture drain; keep lines that trailed the exit (#2159).
    for event in process.drain_combined() {
        output.push_str(&String::from_utf8_lossy(&event.line));
        output.push('\n');
    }
    (exit, output)
}

fn isolated_fixture() -> (
    kernal_api::platform::fs::TemporaryDirectory,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    // Deliberately valid-looking, but clud must not import this separate login.
    fs::write(
        home.join(".codex").join("auth.json"),
        r#"{"tokens":{"access_token":"native-codex-only"}}"#,
    )
    .unwrap();
    let claude = bin.join(if clud::platform::host::WINDOWS {
        "claude.exe"
    } else {
        "claude"
    });
    fs::copy(common::mock_agent_path(), &claude).unwrap();
    (root, home, bin)
}

fn bridge_args(marker: &std::path::Path, mode: &str) -> Vec<String> {
    vec![
        "--no-daemon".to_string(),
        "--provider".to_string(),
        "codex".to_string(),
        "--harness".to_string(),
        "claude".to_string(),
        mode.to_string(),
        "-p".to_string(),
        "hello".to_string(),
        "--mock-write-done".to_string(),
        marker.to_string_lossy().into_owned(),
    ]
}

fn assert_missing_diagnostic(exit: i32, output: &str) {
    assert_ne!(exit, 0, "{output}");
    assert!(
        output.contains("clud auth login codex --acknowledge-experimental"),
        "{output}"
    );
    assert!(output.contains("set OPENAI_API_KEY"), "{output}");
    assert!(
        output.contains("clud --codex --harness default"),
        "{output}"
    );
    assert!(!output.contains("native-codex-only"));
}

#[test]
fn missing_bridge_credentials_refuse_before_mock_claude_spawns_in_each_foreground_mode() {
    for mode in ["--subprocess", "--pty"] {
        let (root, home, bin) = isolated_fixture();
        let marker = root.path().join(format!("mock-claude-spawned-{mode}"));
        let (exit, output) = run(&home, &bin, &bridge_args(&marker, mode));

        assert_missing_diagnostic(exit, &output);
        assert!(
            !marker.exists(),
            "the mock Claude child was spawned in {mode}: {output}"
        );
    }
}

#[test]
fn detached_submission_rejects_before_creating_or_spawning_a_session() {
    let (root, home, bin) = isolated_fixture();
    let marker = root.path().join("detached-mock-claude-spawned");
    let mut args = bridge_args(&marker, "--subprocess");
    args.retain(|arg| arg != "--no-daemon");
    args.insert(0, "--detach".to_string());
    let (exit, output) = run(&home, &bin, &args);

    assert_missing_diagnostic(exit, &output);
    assert!(
        !marker.exists(),
        "the detached mock Claude child was spawned: {output}"
    );
    assert!(
        !home.join(".clud").join("daemon").exists(),
        "credential admission must run before daemon/session creation: {output}"
    );
}

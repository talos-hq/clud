use crate::exe;

use std::time::Duration;

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode, StreamKind,
};

fn clud() -> std::path::PathBuf {
    exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
}

fn run_isolated(home: &std::path::Path, args: &[&str]) -> (i32, Vec<u8>) {
    let mut argv = vec![clud().to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| {
            !key.eq_ignore_ascii_case("HOME") && !key.eq_ignore_ascii_case("USERPROFILE")
        })
        .collect();
    let home = home.to_string_lossy().into_owned();
    env.push(("HOME".to_string(), home.clone()));
    env.push(("USERPROFILE".to_string(), home));
    // Never fetch server settings during a test (#1192).
    env.push(("CLUD_SERVER_SETTINGS".to_string(), "0".to_string()));

    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().expect("spawn clud");
    let mut output = Vec::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => {
                if event.stream == StreamKind::Stdout {
                    output.extend_from_slice(&event.line);
                    output.push(b'\n');
                }
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
        if event.stream == StreamKind::Stdout {
            output.extend_from_slice(&event.line);
            output.push(b'\n');
        }
    }
    (exit, output)
}

#[test]
fn rejected_selection_does_not_persist_no_fix_hooks() {
    let home = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let (exit, _) = run_isolated(
        home.path(),
        &[
            "--no-fix-hooks",
            "--provider",
            "deepseek",
            "--model",
            "codex-terra",
        ],
    );
    assert_eq!(exit, 2);
    assert!(!home.path().join(".clud").join("settings.json").exists());
}

#[test]
fn rejected_kimi_on_the_codex_harness_writes_no_settings() {
    // Kimi speaks only the Anthropic protocol, so the Codex harness is
    // refused during target resolution -- before any vault access and before
    // anything is persisted.
    for harness_args in [&["--harness", "codex"][..], &["--codex"][..]] {
        let home = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut args = vec!["--no-fix-hooks", "--kimi"];
        args.extend_from_slice(harness_args);
        args.extend_from_slice(&["-p", "hello"]);
        let (exit, output) = run_isolated(home.path(), &args);
        assert_eq!(exit, 2, "{}", String::from_utf8_lossy(&output));
        assert!(!home.path().join(".clud").join("settings.json").exists());
    }
}

#[test]
fn default_claude_keeps_a_custom_wire_model_reachable() {
    let home = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let (exit, output) = run_isolated(home.path(), &["--dry-run", "--model", "my-gateway-model"]);
    assert_eq!(exit, 0, "{}", String::from_utf8_lossy(&output));
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["model_provider"], "claude");
    assert_eq!(value["model_selection"]["model"], "my-gateway-model");
    assert_eq!(value["model_selection"]["wire_model"], "my-gateway-model");
    let command = value["command"].as_array().unwrap();
    assert!(command.windows(2).any(|pair| {
        pair[0] == serde_json::Value::String("--model".to_string())
            && pair[1] == serde_json::Value::String("my-gateway-model".to_string())
    }));
}

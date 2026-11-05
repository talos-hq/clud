//! Subscription-first provider policy at the process boundary. The mock
//! Claude marker is the observable child-spawn fact; a refused configuration
//! must exit 2 before any child, launch record or settings file exists.

use crate::{common, exe};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

const POLICY_VARS: &[&str] = &[
    "CLUD_SUBSCRIPTION_PROVIDER",
    "CLUD_PROVIDER",
    "CLUD_ALLOW_METERED",
    "CLUD_METERED_PROVIDER",
];

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    marker: PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let claude = bin.join(if clud::platform::host::WINDOWS {
        "claude.exe"
    } else {
        "claude"
    });
    fs::copy(common::mock_agent_path(), &claude).unwrap();
    let marker = root.path().join("spawned.marker");
    Fixture {
        _root: root,
        home,
        bin,
        marker,
    }
}

fn run(fx: &Fixture, policy: &[(&str, &str)], args: &[&str]) -> (i32, String) {
    let mut argv = vec![exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
        .to_string_lossy()
        .into_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| {
            !key.eq_ignore_ascii_case("HOME")
                && !key.eq_ignore_ascii_case("USERPROFILE")
                && !key.eq_ignore_ascii_case("PATH")
                && !key.eq_ignore_ascii_case("OPENAI_API_KEY")
                && !POLICY_VARS.iter().any(|var| key.eq_ignore_ascii_case(var))
        })
        .collect();
    let home = fx.home.to_string_lossy().into_owned();
    env.push(("HOME".to_string(), home.clone()));
    env.push(("USERPROFILE".to_string(), home));
    env.push(("PATH".to_string(), fx.bin.to_string_lossy().into_owned()));
    env.push(("CLUD_SERVER_SETTINGS".to_string(), "0".to_string()));
    env.extend(
        policy
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
    );
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
        .wait(Some(Duration::from_secs(60)))
        .expect("wait for clud");
    for event in process.drain_combined() {
        output.push_str(&String::from_utf8_lossy(&event.line));
        output.push('\n');
    }
    (exit, output)
}

fn launch_args(fx: &Fixture) -> Vec<String> {
    vec![
        "--no-daemon".into(),
        "-p".into(),
        "hello".into(),
        "--mock-write-done".into(),
        fx.marker.to_string_lossy().into_owned(),
    ]
}

fn find_named(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if entry.file_name() == name {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_named(&path, name) {
                return Some(found);
            }
        }
    }
    None
}

fn assert_refused_without_side_effects(fx: &Fixture, exit: i32, output: &str, needle: &str) {
    assert_eq!(exit, 2, "{output}");
    assert!(
        !output.contains("\"program\":"),
        "a child spawned:\n{output}"
    );
    assert!(output.contains(needle), "expected {needle:?} in:\n{output}");
    assert!(!fx.marker.exists(), "a child spawned:\n{output}");
    assert!(
        find_named(&fx.home, "launches").is_none(),
        "a launch record was written"
    );
    assert!(!fx.home.join(".clud").join("settings.json").exists());
    assert!(!output.contains("WARNING: METERED"), "{output}");
}

#[test]
fn contradictory_policy_env_is_refused_before_any_side_effect() {
    let table: &[(&[(&str, &str)], &str)] = &[
        (&[("CLUD_PROVIDER", "openrouter")], "is metered"),
        (
            &[("CLUD_PROVIDER", "openrouter"), ("CLUD_ALLOW_METERED", "1")],
            "without CLUD_METERED_PROVIDER",
        ),
        (
            &[
                ("CLUD_PROVIDER", "openrouter"),
                ("CLUD_METERED_PROVIDER", "openrouter"),
            ],
            "without CLUD_ALLOW_METERED=1",
        ),
        (
            &[
                ("CLUD_PROVIDER", "claude"),
                ("CLUD_ALLOW_METERED", "1"),
                ("CLUD_METERED_PROVIDER", "openrouter"),
            ],
            "contradiction",
        ),
        (
            &[
                ("CLUD_PROVIDER", "openrouter"),
                ("CLUD_ALLOW_METERED", "1"),
                ("CLUD_METERED_PROVIDER", "deepseek"),
            ],
            "disagrees with CLUD_PROVIDER=openrouter",
        ),
        (
            &[("CLUD_SUBSCRIPTION_PROVIDER", "openrouter")],
            "subscription default can only be",
        ),
    ];
    for (policy, needle) in table {
        let fx = fixture();
        let args = launch_args(&fx);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (exit, output) = run(&fx, policy, &args);
        assert_refused_without_side_effects(&fx, exit, &output, needle);
    }
}

#[test]
fn a_cli_provider_contradicting_clud_provider_is_refused() {
    let fx = fixture();
    let mut args = vec!["--claude".to_string()];
    args.extend(launch_args(&fx));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (exit, output) = run(&fx, &[("CLUD_PROVIDER", "codex")], &args);
    assert_refused_without_side_effects(&fx, exit, &output, "contradiction");
}

#[test]
fn default_launch_uses_the_subscription_without_a_warning() {
    let fx = fixture();
    let args = launch_args(&fx);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (exit, output) = run(&fx, &[], &args);
    assert_eq!(exit, 0, "{output}");
    // The mock agent prints its own invocation record when it runs.
    assert!(
        output.contains("\"program\":") && output.contains("\"-p\",\"hello\""),
        "the subscription launch must run:\n{output}"
    );
    assert!(!output.contains("WARNING: METERED"), "{output}");
}

#[test]
fn subscription_provider_env_selects_codex_without_a_warning() {
    let fx = fixture();
    let (exit, output) = run(
        &fx,
        &[("CLUD_SUBSCRIPTION_PROVIDER", "codex")],
        &["--dry-run", "-p", "hello"],
    );
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("\"model_provider\": \"codex\""), "{output}");
    assert!(!output.contains("WARNING: METERED"), "{output}");
}

#[test]
fn both_metered_overrides_allow_openrouter_with_the_loud_warning() {
    let fx = fixture();
    let (exit, output) = run(
        &fx,
        &[
            ("CLUD_PROVIDER", "openrouter"),
            ("CLUD_ALLOW_METERED", "1"),
            ("CLUD_METERED_PROVIDER", "openrouter"),
        ],
        &["--dry-run", "-p", "hello"],
    );
    assert_eq!(exit, 0, "{output}");
    assert!(
        output.contains("\"model_provider\": \"openrouter\""),
        "{output}"
    );
    assert!(
        output.contains(&clud::metered_policy::metered_warning(
            clud::backend::ModelProvider::OpenRouter
        )),
        "{output}"
    );
}

#[test]
fn missing_codex_subscription_never_falls_back_to_openai_api_key() {
    let fx = fixture();
    let mut args = vec!["--harness".to_string(), "claude".to_string()];
    args.extend(launch_args(&fx));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (exit, output) = run(
        &fx,
        &[
            ("CLUD_PROVIDER", "codex"),
            ("OPENAI_API_KEY", "sk-test-metered-key"),
        ],
        &args,
    );
    assert_ne!(exit, 0, "{output}");
    assert!(output.contains("clud auth login codex"), "{output}");
    assert!(output.contains("CLUD_ALLOW_METERED=1"), "{output}");
    assert!(!fx.marker.exists(), "a child spawned:\n{output}");
    assert!(!output.contains("sk-test-metered-key"), "{output}");
}

/// Naming a metered provider on the command line is consent: it launches with
/// the loud warning and no environment overrides.
#[test]
fn an_explicit_metered_provider_flag_launches_with_the_warning() {
    for (route, provider) in [
        (
            &["--openrouter"][..],
            clud::backend::ModelProvider::OpenRouter,
        ),
        (
            &["--provider", "openrouter"][..],
            clud::backend::ModelProvider::OpenRouter,
        ),
        (&["--deepseek"][..], clud::backend::ModelProvider::DeepSeek),
    ] {
        let fx = fixture();
        let mut args: Vec<&str> = route.to_vec();
        args.extend(["--dry-run", "-p", "hello"]);
        let (exit, output) = run(&fx, &[], &args);
        assert_eq!(exit, 0, "{route:?}: {output}");
        assert!(
            output.contains(&clud::metered_policy::explicit_metered_warning(provider)),
            "{route:?}: {output}"
        );
    }
}

/// Implicit routes onto metered billing still need the environment consent.
#[test]
fn implicit_metered_routes_need_consent() {
    let routes: &[&[&str]] = &[&["--failover-allow-metered"]];
    for route in routes {
        let fx = fixture();
        let mut args: Vec<String> = route.iter().map(|arg| (*arg).to_string()).collect();
        args.extend(launch_args(&fx));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (exit, output) = run(&fx, &[], &args);
        assert_refused_without_side_effects(&fx, exit, &output, "CLUD_ALLOW_METERED=1");
    }
}

#[test]
fn a_saved_metered_provider_needs_consent() {
    let fx = fixture();
    clud::clud_settings::save_global_launch_preferences_at(
        &fx.home,
        Some(clud::backend::ModelProvider::OpenRouter),
        None,
        clud::launch_setup::LaunchSetupScope::Global,
    )
    .expect("save provider");
    let args = launch_args(&fx);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (exit, output) = run(&fx, &[], &args);
    assert_eq!(exit, 2, "{output}");
    assert!(output.contains("is metered"), "{output}");
    assert!(
        !output.contains("\"program\":"),
        "a child spawned:\n{output}"
    );
}

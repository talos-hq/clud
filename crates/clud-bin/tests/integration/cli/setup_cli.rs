//! `clud setup` at the process boundary (#2219): the dry run touches
//! nothing, `--yes` installs the aliases once and is idempotent, a
//! non-terminal run without `--yes` fails fast instead of waiting for input,
//! and an OpenRouter key goes through the existing credential store without
//! ever being echoed.

use crate::exe;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

/// A well-formed, obviously fake OpenRouter key, assembled at runtime so
/// secret scanners do not mistake the test fixture for a leaked credential.
fn fake_key() -> String {
    format!("{}{}", "sk-or-v1-", "0123456789abcdef".repeat(4))
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    vault: PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let vault = root.path().join("vault");
    fs::create_dir_all(&home).unwrap();
    Fixture {
        _root: root,
        home,
        vault,
    }
}

fn run(fx: &Fixture, extra_env: &[(&str, &str)], args: &[&str]) -> (i32, String) {
    let mut argv = vec![exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
        .to_string_lossy()
        .into_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| {
            let upper = key.to_ascii_uppercase();
            upper != "HOME"
                && upper != "USERPROFILE"
                && upper != "OPENROUTER_API_KEY"
                && !upper.starts_with("CLUD_")
        })
        .collect();
    let home = fx.home.to_string_lossy().into_owned();
    env.push(("HOME".into(), home.clone()));
    env.push(("USERPROFILE".into(), home));
    env.push(("CLUD_SERVER_SETTINGS".into(), "0".into()));
    env.push(("CLUD_INTEGRATION_TESTS".into(), "1".into()));
    env.push((
        "CLUD_TEST_SECRET_STORE_DIR".into(),
        fx.vault.to_string_lossy().into_owned(),
    ));
    env.extend(
        extra_env
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

/// Every file under `root` with its bytes (or link target), for a
/// byte-identical before/after comparison.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                files.insert(path.clone(), b"<dir>".to_vec());
                stack.push(path);
            } else if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                files.insert(path, target.to_string_lossy().into_owned().into_bytes());
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

fn shim_names(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(home.join(".clud/state/shims"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn setup_help_lists_dry_run_and_yes() {
    let fx = fixture();
    let (exit, output) = run(&fx, &[], &["setup", "--help"]);
    assert_eq!(exit, 0, "{output}");
    for flag in ["--dry-run", "--yes", "--provider", "--write-profile"] {
        assert!(output.contains(flag), "missing {flag}:\n{output}");
    }
}

#[test]
fn setup_dry_run_lists_steps_and_changes_nothing() {
    let fx = fixture();
    let before = snapshot(fx.home.parent().unwrap());
    let (exit, output) = run(&fx, &[], &["setup", "--dry-run", "--provider", "claude"]);
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("would install"), "{output}");
    assert!(output.contains("provider"), "{output}");
    assert!(output.contains("claude"), "{output}");
    assert!(output.contains("shell profiles"), "{output}");
    assert_eq!(before, snapshot(fx.home.parent().unwrap()), "{output}");
}

#[test]
fn setup_yes_installs_aliases_once_and_second_run_reports_already_installed() {
    let fx = fixture();
    let (exit, first) = run(&fx, &[], &["setup", "--yes", "--provider", "claude"]);
    assert_eq!(exit, 0, "{first}");
    let installed = shim_names(&fx.home);
    assert!(
        installed.iter().any(|name| name.starts_with("python")),
        "{installed:?}\n{first}"
    );
    let (exit, second) = run(&fx, &[], &["setup", "--yes", "--provider", "claude"]);
    assert_eq!(exit, 0, "{second}");
    assert!(second.contains("already installed"), "{second}");
    assert!(second.contains("already"), "{second}");
    assert_eq!(installed, shim_names(&fx.home), "aliases changed on rerun");
}

#[test]
fn setup_without_yes_on_non_terminal_fails_fast_and_names_the_flag() {
    let fx = fixture();
    let started = Instant::now();
    let (exit, output) = run(&fx, &[], &["setup"]);
    assert_eq!(exit, 2, "{output}");
    assert!(output.contains("--yes"), "{output}");
    assert!(started.elapsed() < Duration::from_secs(30), "setup blocked");
    assert!(
        shim_names(&fx.home).is_empty(),
        "refused setup installed aliases"
    );
}

#[test]
fn setup_openrouter_stores_key_in_credential_store_without_echoing_it() {
    let fx = fixture();
    let key = fake_key();
    let (exit, output) = run(
        &fx,
        &[("OPENROUTER_API_KEY", key.as_str())],
        &["setup", "--yes", "--provider", "openrouter"],
    );
    assert_eq!(exit, 0, "{output}");
    assert!(!output.contains(&key), "key echoed:\n{output}");
    let vault = snapshot(&fx.vault);
    assert!(
        vault.values().any(|bytes| bytes != b"<dir>"),
        "key not written to the credential store"
    );
    assert!(output.contains("API key stored"), "{output}");
    let settings = snapshot(&fx.home.join(".clud"))
        .into_iter()
        .any(|(path, bytes)| {
            path.extension().is_some_and(|ext| ext == "json")
                && String::from_utf8_lossy(&bytes).contains("openrouter")
        });
    assert!(settings, "provider not persisted");
    let (exit, again) = run(&fx, &[], &["setup", "--yes", "--provider", "openrouter"]);
    assert_eq!(exit, 0, "{again}");
    // Read back through the store, with no key in the environment.
    assert!(again.contains("already stored"), "{again}");
    assert!(!again.contains(&key), "key echoed:\n{again}");
}

#[test]
fn setup_openrouter_yes_without_a_key_fails_with_guidance() {
    let fx = fixture();
    let (exit, output) = run(&fx, &[], &["setup", "--yes", "--provider", "openrouter"]);
    assert_eq!(exit, 2, "{output}");
    assert!(output.contains("OPENROUTER_API_KEY"), "{output}");
}

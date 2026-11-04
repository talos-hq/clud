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
                && !upper.starts_with("HERMES_")
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

// ---------------------------------------------------------------------------
// Hermes OpenRouter import: only `clud setup` may read Hermes config or env.
// ---------------------------------------------------------------------------

const HERMES_CONFIG: &str = "model:\n  default: deepseek/deepseek-v4.1-flash-1017\n  provider: openrouter\n  base_url: ''\nmodel_overrides:\n  openrouter:\n    deepseek/deepseek-v4.1-flash-1017:\n      context_window: 1048576\n    z-ai/glm-5.3-flash:\n      context_window: 200000\n";

/// Not YAML at all: any parser that touches it fails loudly.
const INVALID_YAML: &str = "model: [unclosed\n  provider: openrouter\n\t- : : :\n";

fn write_hermes(fx: &Fixture, body: &str) -> PathBuf {
    let dir = fx.home.join(".hermes");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    fs::write(&path, body).unwrap();
    path
}

fn metered_env(key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("OPENROUTER_API_KEY", key.to_string()),
        ("CLUD_ALLOW_METERED", "1".into()),
        ("CLUD_METERED_PROVIDER", "openrouter".into()),
    ]
}

fn borrow<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    env.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

fn mentions_hermes(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("hermes") || lower.contains("yaml") || lower.contains("config.yaml")
}

/// The security boundary: with an invalid-YAML Hermes config and a Hermes
/// key in the environment, normal launches neither parse, warn about, nor
/// import it. Only `clud setup` reacts to the same fixture.
#[test]
fn hermes_config_is_invisible_to_every_command_but_setup() {
    let fx = fixture();
    let hermes_dir = fx.home.join(".hermes");
    write_hermes(&fx, INVALID_YAML);
    let key = fake_key();
    fs::write(
        hermes_dir.join(".env"),
        format!("OPENROUTER_API_KEY={key}\n"),
    )
    .unwrap();
    let hermes_home = hermes_dir.to_string_lossy().into_owned();
    let env = [
        ("OPENROUTER_API_KEY", key.as_str()),
        ("HERMES_HOME", hermes_home.as_str()),
    ];
    let assert_untouched = |args: &[&str], output: &str| {
        assert!(
            !mentions_hermes(output),
            "clud {args:?} reacted to the Hermes config:\n{output}"
        );
        assert!(!output.contains(&key), "key echoed:\n{output}");
        assert!(
            snapshot(&fx.vault).values().all(|bytes| bytes == b"<dir>"),
            "clud {args:?} imported a credential"
        );
    };
    for args in [
        &["-p", "hello", "--dry-run"][..],
        &["-p", "hello", "--dry-run", "run"][..],
        &["--openrouter", "-p", "hello", "--dry-run"][..],
    ] {
        let (exit, output) = run(&fx, &env, args);
        assert_eq!(exit, 0, "clud {args:?} failed:\n{output}");
        assert_untouched(args, &output);
    }
    // `serve --openrouter` with the key only in Hermes' .env: reading
    // Hermes would find it; the boundary means the key is missing.
    let serve = ["serve", "--openrouter", "--port", "0"];
    let (exit, output) = run(
        &fx,
        &[
            ("HERMES_HOME", hermes_home.as_str()),
            (
                "CLUD_SERVE_CLIENT_TOKEN",
                "abcdefabcdefabcdefabcdefabcdefab",
            ),
        ],
        &serve,
    );
    assert_eq!(exit, 2, "{output}");
    assert!(
        output.contains("OpenRouter credentials are missing"),
        "{output}"
    );
    assert_untouched(&serve, &output);
    // Same fixture: setup is the one command that reads it.
    let (exit, output) = run(&fx, &env, &["setup", "--dry-run", "--provider", "claude"]);
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("hermes: could not parse"), "{output}");
}

#[test]
fn setup_dry_run_shows_hermes_openrouter_settings_and_imports_nothing() {
    let fx = fixture();
    write_hermes(&fx, HERMES_CONFIG);
    let key = fake_key();
    let env = metered_env(&key);
    let before = snapshot(fx.home.parent().unwrap());
    let (exit, output) = run(&fx, &borrow(&env), &["setup", "--dry-run"]);
    assert_eq!(exit, 0, "{output}");
    assert!(
        output.contains("base_url: https://openrouter.ai/api"),
        "{output}"
    );
    assert!(
        output.contains("deepseek/deepseek-v4.1-flash-1017"),
        "{output}"
    );
    assert!(output.contains("z-ai/glm-5.3-flash"), "{output}");
    assert!(output.contains("found in OPENROUTER_API_KEY"), "{output}");
    assert!(output.contains("would import"), "{output}");
    assert!(!output.contains(&key), "key echoed:\n{output}");
    assert_eq!(before, snapshot(fx.home.parent().unwrap()), "{output}");
}

#[test]
fn setup_yes_without_import_flag_never_imports_hermes_credentials() {
    let fx = fixture();
    write_hermes(&fx, HERMES_CONFIG);
    let key = fake_key();
    let hermes_home = fx.home.join(".hermes").to_string_lossy().into_owned();
    let env = [
        ("CLUD_ALLOW_METERED", "1"),
        ("CLUD_METERED_PROVIDER", "openrouter"),
        ("HERMES_HOME", hermes_home.as_str()),
    ];
    // Key lives only in Hermes' .env, so only an import could store it.
    fs::write(
        fx.home.join(".hermes/.env"),
        format!("OPENROUTER_API_KEY={key}\n"),
    )
    .unwrap();
    let (exit, output) = run(&fx, &env, &["setup", "--yes", "--provider", "claude"]);
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("pass --import-hermes"), "{output}");
    assert!(
        snapshot(&fx.vault).values().all(|bytes| bytes == b"<dir>"),
        "--yes imported a Hermes credential:\n{output}"
    );
    assert!(
        output.contains("provider: default set to claude"),
        "{output}"
    );
}

#[test]
fn setup_import_hermes_stores_key_and_prints_metered_warning() {
    let fx = fixture();
    write_hermes(&fx, HERMES_CONFIG);
    let key = fake_key();
    let env = metered_env(&key);
    let (exit, output) = run(&fx, &borrow(&env), &["setup", "--yes", "--import-hermes"]);
    assert_eq!(exit, 0, "{output}");
    assert!(
        output.contains("METERED PROVIDER IN USE: openrouter"),
        "{output}"
    );
    assert!(output.contains("API key stored"), "{output}");
    assert!(!output.contains(&key), "key echoed:\n{output}");
    assert!(
        snapshot(&fx.vault).values().any(|bytes| bytes != b"<dir>"),
        "key not written to the credential store"
    );
    let settings: String = snapshot(&fx.home.join(".clud"))
        .into_iter()
        .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "json"))
        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
        .collect();
    assert!(
        settings.contains("deepseek/deepseek-v4.1-flash-1017"),
        "model not imported:\n{settings}"
    );
}

#[test]
fn setup_import_hermes_without_metered_consent_is_refused() {
    let fx = fixture();
    write_hermes(&fx, HERMES_CONFIG);
    let key = fake_key();
    let before = snapshot(fx.home.parent().unwrap());
    let (exit, output) = run(
        &fx,
        &[("OPENROUTER_API_KEY", key.as_str())],
        &["setup", "--yes", "--import-hermes"],
    );
    assert_eq!(exit, 2, "{output}");
    assert!(
        output.contains("METERED PROVIDER IN USE: openrouter"),
        "{output}"
    );
    assert!(output.contains("CLUD_ALLOW_METERED=1"), "{output}");
    assert_eq!(before, snapshot(fx.home.parent().unwrap()), "{output}");
}

#[test]
fn setup_import_hermes_with_settings_but_no_key_names_what_is_missing() {
    let fx = fixture();
    write_hermes(&fx, HERMES_CONFIG);
    let before = snapshot(fx.home.parent().unwrap());
    let (exit, output) = run(
        &fx,
        &[
            ("CLUD_ALLOW_METERED", "1"),
            ("CLUD_METERED_PROVIDER", "openrouter"),
        ],
        &["setup", "--yes", "--import-hermes"],
    );
    assert_eq!(exit, 2, "{output}");
    assert!(output.contains("no OpenRouter API key"), "{output}");
    assert!(output.contains("OPENROUTER_API_KEY"), "{output}");
    assert_eq!(
        before,
        snapshot(fx.home.parent().unwrap()),
        "partial write:\n{output}"
    );
}

#[test]
fn setup_without_hermes_config_prints_nothing_about_hermes() {
    let fx = fixture();
    let (exit, output) = run(&fx, &[], &["setup", "--dry-run", "--provider", "claude"]);
    assert_eq!(exit, 0, "{output}");
    assert!(!mentions_hermes(&output), "{output}");
}

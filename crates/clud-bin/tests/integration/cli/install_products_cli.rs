//! `clud install <product>` at the process boundary (#2609): `clud setup
//! <product>` is the same command byte for byte, an unknown product is refused
//! with the known ones, `--dry-run` writes nothing, and a migration puts the
//! provider's settings into the product's `config.yaml` without ever printing
//! the key.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

use crate::exe;

/// A well-formed, obviously fake OpenRouter key, assembled at runtime so secret
/// scanners do not mistake the fixture for a leaked credential.
fn fake_key() -> String {
    format!("{}{}", "sk-or-v1-", "0123456789abcdef".repeat(4))
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("home");
    Fixture { _root: root, home }
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
        fx.home.join("vault").to_string_lossy().into_owned(),
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

fn config_path(fx: &Fixture) -> PathBuf {
    fx.home.join(".hermes").join("config.yaml")
}

fn seed_config(fx: &Fixture) {
    let path = config_path(fx);
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(
        &path,
        "# my hermes config\nmodel:\n  default: \"x/y\"\n# trailing comment\n",
    )
    .expect("seed");
}

#[test]
fn install_help_names_both_products_and_the_configure_flags() {
    let fx = fixture();
    let (exit, output) = run(&fx, &[], &["install", "--help"]);
    assert_eq!(exit, 0, "{output}");
    for needle in [
        "hermes",
        "talos",
        "--dry-run",
        "--configure-only",
        "--migrate-credentials",
    ] {
        assert!(output.contains(needle), "missing {needle}:\n{output}");
    }
}

#[test]
fn setup_with_a_product_is_the_same_command_as_install() {
    let fx = fixture();
    let (install_exit, install) = run(&fx, &[], &["install", "hermes", "--dry-run"]);
    let (setup_exit, setup) = run(&fx, &[], &["setup", "hermes", "--dry-run"]);
    assert_eq!(install_exit, 0, "{install}");
    assert_eq!((install_exit, install), (setup_exit, setup.clone()));
    assert!(
        setup.contains("talos-hq/hermes-slim@main/slim/install.sh"),
        "{setup}"
    );
}

#[test]
fn an_unknown_product_is_refused_with_the_known_ones() {
    let fx = fixture();
    let (exit, output) = run(&fx, &[], &["install", "codex"]);
    assert_eq!(exit, 2, "{output}");
    assert!(output.contains("hermes, talos"), "{output}");
    assert!(!fx.home.join(".clud").exists(), "{output}");
}

#[test]
fn a_dry_run_writes_nothing() {
    let fx = fixture();
    let (exit, output) = run(&fx, &[], &["install", "talos", "--dry-run"]);
    assert_eq!(exit, 0, "{output}");
    assert!(
        output.contains("talos-hq/talos@main/scripts/install.sh"),
        "{output}"
    );
    assert!(!fx.home.join(".clud").exists(), "{output}");
}

#[test]
fn configure_only_with_no_stored_credentials_leaves_the_config_alone() {
    let fx = fixture();
    seed_config(&fx);
    let before = fs::read_to_string(config_path(&fx)).expect("read");
    let (exit, output) = run(&fx, &[], &["install", "hermes", "--configure-only"]);
    assert_eq!(exit, 0, "{output}");
    assert!(
        output.contains("providers: nothing stored in clud; nothing to migrate"),
        "{output}"
    );
    assert_eq!(
        fs::read_to_string(config_path(&fx)).expect("read"),
        before,
        "{output}"
    );
}

#[test]
fn migration_writes_the_provider_and_never_prints_the_key() {
    let fx = fixture();
    seed_config(&fx);
    let key = fake_key();
    let (exit, output) = run(
        &fx,
        &[("OPENROUTER_API_KEY", key.as_str())],
        &[
            "install",
            "hermes",
            "--configure-only",
            "--migrate-credentials",
        ],
    );
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("wrote openrouter"), "{output}");
    assert!(!output.contains(&key), "the key reached stdout:\n{output}");
    let text = fs::read_to_string(config_path(&fx)).expect("read");
    assert!(text.contains("# my hermes config"), "{text}");
    assert!(text.contains("# trailing comment"), "{text}");
    assert!(
        text.contains("base_url: \"https://openrouter.ai/api/v1\""),
        "{text}"
    );
    assert!(text.contains(&format!("api_key: \"{key}\"")), "{text}");
}

#[test]
fn yes_alone_does_not_migrate_and_says_how_to() {
    let fx = fixture();
    seed_config(&fx);
    let before = fs::read_to_string(config_path(&fx)).expect("read");
    let key = fake_key();
    let (exit, output) = run(
        &fx,
        &[("OPENROUTER_API_KEY", key.as_str())],
        &["install", "hermes", "--configure-only", "--yes"],
    );
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("--migrate-credentials"), "{output}");
    assert_eq!(
        fs::read_to_string(config_path(&fx)).expect("read"),
        before,
        "{output}"
    );
}

#[test]
fn an_unparseable_config_is_left_byte_identical() {
    let fx = fixture();
    let path = config_path(&fx);
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(&path, "providers: [unterminated\n").expect("seed");
    let key = fake_key();
    let (exit, output) = run(
        &fx,
        &[("OPENROUTER_API_KEY", key.as_str())],
        &[
            "install",
            "hermes",
            "--configure-only",
            "--migrate-credentials",
        ],
    );
    assert_eq!(exit, 1, "{output}");
    assert!(output.contains("left untouched"), "{output}");
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        "providers: [unterminated\n"
    );
}

#[test]
fn dir_and_target_are_refused_together() {
    let fx = fixture();
    let (exit, output) = run(
        &fx,
        &[],
        &[
            "install",
            "hermes",
            "--dry-run",
            "--dir",
            "/tmp/a",
            "--target",
            "/tmp/b",
        ],
    );
    assert_eq!(exit, 2, "{output}");
}

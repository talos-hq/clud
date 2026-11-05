use super::*;
use std::path::Path;

const REAL_SHAPE: &str = "model:\n  default: deepseek/deepseek-v4.1-flash-1017\n  provider: openrouter\n  base_url: ''\nmodel_overrides:\n  openrouter:\n    deepseek/deepseek-v4.1-flash:\n      context_window: 1048576\n    deepseek/deepseek-v4.1-flash-1017:\n      context_window: 1048576\n";

fn fake_key() -> String {
    format!("{}{}", "sk-or-v1-", "0123456789abcdef".repeat(4))
}

fn home_with(config: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".hermes")).unwrap();
    std::fs::write(home.path().join(".hermes/config.yaml"), config).unwrap();
    home
}

fn found(detection: Detection) -> Found {
    match detection {
        Detection::Found(found) => found,
        other => panic!("expected settings, got {other:?}"),
    }
}

#[test]
fn empty_base_url_means_the_openrouter_default() {
    let home = home_with(REAL_SHAPE);
    let found = found(detect(home.path(), None, None));
    assert_eq!(found.base_url, OPENROUTER_DEFAULT_BASE_URL);
    assert_eq!(
        found.models,
        [
            "deepseek/deepseek-v4.1-flash-1017",
            "deepseek/deepseek-v4.1-flash"
        ]
    );
    assert_eq!(found.key, None, "the key is not in the YAML");
}

#[test]
fn top_level_provider_and_providers_section_are_read() {
    let home = home_with(
        "provider: openrouter\nbase_url: https://example.test/v1\nproviders:\n  openrouter:\n    model: z-ai/glm-5.3-flash\n",
    );
    let found = found(detect(home.path(), None, None));
    assert_eq!(found.base_url, "https://example.test/v1");
    assert_eq!(found.models, ["z-ai/glm-5.3-flash"]);
}

#[test]
fn non_openrouter_config_is_not_offered() {
    let home = home_with("model:\n  default: claude\n  provider: anthropic\n");
    assert_eq!(detect(home.path(), None, None), Detection::None);
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(detect(empty.path(), None, None), Detection::None);
}

#[test]
fn invalid_yaml_is_reported_not_fatal() {
    let home = home_with("model: [unclosed\n\t- : :\n");
    assert!(matches!(
        detect(home.path(), None, None),
        Detection::Unparsable(..)
    ));
}

#[test]
fn key_comes_from_environment_then_hermes_dotenv_and_hermes_home_wins() {
    let home = home_with("model: x\n");
    let custom = tempfile::tempdir().unwrap();
    std::fs::write(custom.path().join("config.yaml"), REAL_SHAPE).unwrap();
    std::fs::write(
        custom.path().join(".env"),
        format!("export OPENROUTER_API_KEY=\"{}\"\n", fake_key()),
    )
    .unwrap();
    let from_dotenv = found(detect(home.path(), Some(custom.path().into()), None));
    let (source, key) = from_dotenv.key.unwrap();
    assert!(source.contains(".env"), "{source}");
    assert_eq!(key, fake_key());
    let from_env = found(detect(
        home.path(),
        Some(custom.path().into()),
        Some("sk-env".into()),
    ));
    assert_eq!(from_env.key.unwrap().1, "sk-env");
}

#[test]
fn profile_dirs_are_scanned() {
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join(".hermes/profiles/work");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("config.yaml"), REAL_SHAPE).unwrap();
    assert!(found(detect(home.path(), None, None))
        .path
        .starts_with(&profile));
}

struct Run {
    outcome: Outcome,
    out: String,
    stored: usize,
}

fn run_offer(found: Option<Found>, mode: Mode, answer: &str, metered_ok: bool) -> Run {
    let detection = found.map_or(Detection::None, Detection::Found);
    let mut out = Vec::new();
    let mut stored = 0;
    let mut store = |_: &Found, _: &mut dyn Write| {
        stored += 1;
        0
    };
    let outcome = offer(
        &detection,
        mode,
        &mut answer.as_bytes(),
        &mut out,
        metered_ok,
        &mut store,
    );
    Run {
        outcome,
        out: String::from_utf8(out).unwrap(),
        stored,
    }
}

fn sample(with_key: bool) -> Found {
    Found {
        path: Path::new("/h/.hermes/config.yaml").into(),
        base_url: OPENROUTER_DEFAULT_BASE_URL.into(),
        models: vec!["deepseek/deepseek-v4.1-flash-1017".into()],
        key: with_key.then(|| ("OPENROUTER_API_KEY (environment)".into(), fake_key())),
    }
}

#[test]
fn interactive_prompt_defaults_to_no() {
    for answer in ["\n", "", "n\n", "maybe\n"] {
        let run = run_offer(
            Some(sample(true)),
            Mode::Interactive { import: false },
            answer,
            true,
        );
        assert_eq!(run.outcome, Outcome::Skipped, "{answer:?}");
        assert_eq!(run.stored, 0);
        assert!(run.out.contains(PROMPT.trim_end()), "{}", run.out);
        assert!(!run.out.contains(&fake_key()));
    }
    let yes = run_offer(
        Some(sample(true)),
        Mode::Interactive { import: false },
        "y\n",
        true,
    );
    assert_eq!(yes.outcome, Outcome::Imported);
    assert_eq!(yes.stored, 1);
    assert!(yes.out.contains("METERED PROVIDER IN USE: openrouter"));
}

#[test]
fn yes_never_imports_without_the_flag_and_dry_run_never_stores() {
    for mode in [Mode::NonInteractive { import: false }, Mode::DryRun] {
        let run = run_offer(Some(sample(true)), mode, "y\n", true);
        assert_eq!(run.outcome, Outcome::Skipped, "{mode:?}");
        assert_eq!(run.stored, 0, "{mode:?}");
        assert!(!run.out.contains(PROMPT.trim_end()), "{mode:?} prompted");
    }
}

#[test]
fn import_requires_a_key_and_metered_consent() {
    let no_key = run_offer(
        Some(sample(false)),
        Mode::NonInteractive { import: true },
        "",
        true,
    );
    assert_eq!(no_key.outcome, Outcome::Failed(2));
    assert_eq!(no_key.stored, 0);
    let unmetered = run_offer(
        Some(sample(true)),
        Mode::NonInteractive { import: true },
        "",
        false,
    );
    assert_eq!(unmetered.outcome, Outcome::Failed(2));
    assert_eq!(unmetered.stored, 0);
    assert!(unmetered
        .out
        .contains("METERED PROVIDER IN USE: openrouter"));
}

#[test]
fn no_config_is_silent_unless_import_was_requested() {
    let quiet = run_offer(None, Mode::Interactive { import: false }, "y\n", true);
    assert_eq!(quiet.outcome, Outcome::Skipped);
    assert!(quiet.out.is_empty(), "{}", quiet.out);
    let asked = run_offer(None, Mode::NonInteractive { import: true }, "", true);
    assert_eq!(asked.outcome, Outcome::Failed(2));
}

/// This crate's `src/`: the compile-time manifest dir when it exists (the
/// build host), else the checkout above the working directory. Native CI
/// runners execute a bundle built on Linux from a checkout at another path,
/// as `clud_repo_dev::this_checkout_is_detected` documents.
fn crate_src_dir(manifest_dir: &Path, cwd: &Path) -> Option<std::path::PathBuf> {
    let compiled = manifest_dir.join("src");
    if compiled.is_dir() {
        return Some(compiled);
    }
    cwd.ancestors()
        .map(|dir| dir.join("crates").join("clud-bin").join("src"))
        .find(|dir| dir.is_dir())
}

#[test]
fn crate_src_dir_falls_back_to_the_checkout_above_cwd() {
    let checkout = tempfile::tempdir().unwrap();
    let src = checkout.path().join("crates").join("clud-bin").join("src");
    std::fs::create_dir_all(&src).unwrap();
    let nested = checkout.path().join("bundle").join("target");
    std::fs::create_dir_all(&nested).unwrap();
    let missing = checkout.path().join("not-the-build-host");
    assert_eq!(crate_src_dir(&missing, &nested), Some(src.clone()));
    let host = checkout.path().join("crates").join("clud-bin");
    assert_eq!(crate_src_dir(&host, &missing), Some(src));
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(crate_src_dir(&missing, empty.path()), None);
}

/// The structural half of the boundary: no source file outside the setup paths
/// names a Hermes config path or the Hermes environment.
///
/// `install/` belongs to the same boundary (#2609): installing a product has to
/// name that product's install directory, config home and config file, and the
/// migration writes its config. A launch, `run`, `serve`, the daemon and
/// `doctor` stay outside it.
#[test]
fn only_setup_hermes_module_names_hermes_paths_or_env() {
    let cwd = std::env::current_dir().unwrap();
    let src = crate_src_dir(Path::new(env!("CARGO_MANIFEST_DIR")), &cwd)
        .expect("no clud-bin src/ at the build path or above the working directory");
    let allowed = [
        src.join("setup_command/hermes.rs"),
        src.join("setup_command/hermes_tests.rs"),
        src.join("install/command.rs"),
        src.join("install/configure.rs"),
        src.join("install/products.rs"),
    ];
    let needles = [".hermes", "HERMES_HOME", "HERMES_"];
    let mut stack = vec![src.clone()];
    let mut offenders = Vec::new();
    let mut scanned = 0usize;
    while let Some(dir) = stack.pop() {
        // A directory the walker cannot open is a scan gap, not an offender: filesystem layouts
        // differ per platform and inside CI sandboxes, and panicking here turns a boundary check
        // into an environment test. Record it loudly, keep walking, and prove below that the scan
        // was not vacuous.
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                eprintln!("warning: cannot scan {}: {err}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Follow nothing: a symlink whose target is absent makes read_dir fail, and its
            // contents are not this crate's source anyway.
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") || allowed.contains(&path) {
                continue;
            }
            scanned += 1;
            // The public setting spelling is not a filesystem path or an environment read.
            let text = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .replace("context.hermes.enabled", "context-setting");
            for needle in needles {
                if text.contains(needle) {
                    offenders.push(format!("{} names {needle}", path.display()));
                }
            }
        }
    }
    assert!(
        scanned > 0,
        "scanned no Rust sources under {} — the boundary check must not pass vacuously",
        src.display()
    );
    assert!(
        offenders.is_empty(),
        "only the setup and install paths may read Hermes config or env:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn optional_personality_reference_does_not_read_or_inject_the_file() {
    let home = tempfile::tempdir().unwrap();
    let instance = home.path().join(".hermes");
    std::fs::create_dir(&instance).unwrap();
    std::fs::write(
        instance.join("SOUL.md"),
        "PERSONA_CONTENT_MUST_NOT_BE_INJECTED",
    )
    .unwrap();
    std::fs::write(instance.join("config.yaml"), "not: [valid yaml").unwrap();
    let reference = context_reference(home.path(), None).unwrap();
    assert!(reference.contains("SOUL.md"));
    assert!(reference.contains("Reading it is optional"));
    assert!(!reference.contains("PERSONA_CONTENT_MUST_NOT_BE_INJECTED"));
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("SOUL.md"), "other personality").unwrap();
    let reference = context_reference(home.path(), Some(other.path().into())).unwrap();
    let path = other.path().join("SOUL.md").canonicalize().unwrap();
    let quoted = serde_json::to_string(&path.to_string_lossy()).unwrap();
    assert!(reference.contains(&quoted));
}

#[test]
fn missing_optional_personality_file_is_an_actionable_error() {
    let home = tempfile::tempdir().unwrap();
    let error = context_reference(home.path(), None).unwrap_err();
    assert!(error.contains("context.hermes.enabled=false"));
    assert!(error.contains("SOUL.md"));
}

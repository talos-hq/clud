//! `clud setup` (#2219): one command that takes a machine to a working clud.
//!
//! 1. installs the interpreter aliases with the same code every launch uses
//!    ([`crate::shim_install::extract_shims_at`]);
//! 2. runs the shared provider setup ([`crate::self_install::setup`], also the
//!    interactive installer's post-install step): provider, credential through
//!    the existing store, OpenRouter model, saved defaults;
//! 3. edits shell profiles / PATH only with `--write-profile`, by running the
//!    native installer for this version, which owns activation.
//!
//! 0. only here: offers to import OpenRouter settings Hermes already has
//!    ([`hermes`], private to this module; no other command reads Hermes).
//!
//! Every step reports what it found already in place, so a rerun is safe.
//! `--dry-run` reports the same steps and writes nothing. Without a terminal,
//! `--yes` is required; the command never waits for input it cannot get.
//! See docs/architecture/setup-command.md.

use std::io::{IsTerminal, Write};
use std::path::Path;

use crate::backend::ModelProvider;
use crate::self_install::setup as provider_setup;
use crate::shim_install;

mod frontend;
mod hermes;

/// Resolve an opt-in personality reference without importing credentials or reading its contents.
pub(crate) fn hermes_context_reference(home: &Path) -> Result<String, String> {
    hermes::context_reference_from_process(home)
}

/// Exit status for a refused or impossible request (usage-level failure).
pub const EXIT_USAGE: i32 = 2;

/// `clud setup --check`: setup has not been run, or left no usable credential.
pub const EXIT_NEEDS_SETUP: i32 = 3;

/// Whether a machine still needs `clud setup`: no saved provider, or OpenRouter
/// (the only provider whose key clud stores) with no key stored or exported.
fn needs_setup(provider: Option<ModelProvider>, openrouter_key: bool) -> bool {
    match provider {
        None => true,
        Some(ModelProvider::OpenRouter) => !openrouter_key,
        Some(_) => false,
    }
}

/// `clud setup --check`: report whether setup is needed; change nothing.
pub fn check() -> i32 {
    let Some(home) = crate::home::user_home() else {
        eprintln!("clud setup: cannot resolve the home directory");
        return 1;
    };
    let provider = crate::clud_settings::load_launch_preferences_read_only_at(&home)
        .ok()
        .and_then(|snapshot| snapshot.global.model_provider);
    if needs_setup(provider, openrouter_key_available()) {
        println!("clud setup: not set up yet; run `clud setup`");
        EXIT_NEEDS_SETUP
    } else if let Some(missing) = provider.filter(|provider| {
        !frontend::frontend_installed(
            *provider,
            &mut crate::backend_bootstrap::ProductionBackendBootstrapHost,
        )
    }) {
        println!(
            "clud setup: {} launches need {}, which is not installed; run `clud setup`",
            missing.as_str(),
            frontend::frontend_for(missing).executable_name()
        );
        EXIT_NEEDS_SETUP
    } else {
        println!("clud setup: set up");
        0
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    pub yes: bool,
    pub provider: Option<ModelProvider>,
    pub write_profile: bool,
    /// Import Hermes' OpenRouter settings (required under `--yes`).
    pub import_hermes: bool,
}

/// Run `clud setup` against the real home, executable and terminal.
pub fn run(options: &Options) -> i32 {
    let Some(home) = crate::home::user_home() else {
        eprintln!("clud setup: cannot resolve the home directory");
        return 1;
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let source = shim_install::packaged_shim().ok();
    let mut stdout = std::io::stdout().lock();
    let detection = hermes::detect_from_process(&home);
    let code = run_at(
        &home,
        source.as_deref(),
        options,
        interactive,
        &Hermes { detection },
        &mut crate::backend_bootstrap::ProductionBackendBootstrapHost,
        &mut stdout,
    );
    let _ = stdout.flush();
    code
}

/// What setup learned about Hermes before any step ran.
struct Hermes {
    detection: hermes::Detection,
}

fn run_at<H: crate::backend_bootstrap::BackendBootstrapHost>(
    home: &Path,
    source: Option<&Path>,
    options: &Options,
    interactive: bool,
    hermes_state: &Hermes,
    host: &mut H,
    out: &mut dyn Write,
) -> i32 {
    if let Some(provider) = options.provider {
        if !provider_setup::OFFERED.contains(&provider) {
            eprintln!(
                "clud setup: --provider must be one of claude, codex, openrouter (got {})",
                provider.as_str()
            );
            return EXIT_USAGE;
        }
    }
    if !options.dry_run && !options.yes && !interactive {
        eprintln!(
            "clud setup: no interactive terminal; rerun with --yes \
             (optionally --provider claude|codex|openrouter), or --dry-run to preview"
        );
        return EXIT_USAGE;
    }
    let prompt = interactive && !options.yes && !options.dry_run;
    if options.dry_run {
        let _ = writeln!(out, "clud setup --dry-run: nothing will be changed");
    }
    // Before any write, so a refused import leaves nothing behind.
    let imported = match offer_hermes(home, options, prompt, hermes_state, out) {
        Ok(imported) => imported,
        Err(code) => return code,
    };
    let code = install_aliases(home, source, options.dry_run, out);
    if code != 0 {
        return code;
    }
    let provider = if imported {
        let current = crate::clud_settings::load_launch_preferences_read_only_at(home)
            .ok()
            .and_then(|snapshot| snapshot.global.model_provider);
        match save_default(home, ModelProvider::OpenRouter, current, false, out) {
            0 => ModelProvider::OpenRouter,
            code => return code,
        }
    } else {
        match configure_provider(home, options, prompt, out) {
            Ok(provider) => provider,
            Err(code) => return code,
        }
    };
    let mode = match (options.dry_run, prompt) {
        (true, _) => frontend::Mode::DryRun,
        (false, true) => frontend::Mode::Prompt,
        (false, false) => frontend::Mode::Yes,
    };
    let mut stdin = std::io::stdin().lock();
    let code = frontend::ensure_frontend(provider, mode, &mut stdin, host, out);
    if code != 0 {
        return code;
    }
    profile_step(options, out)
}

/// Step 0: offer the Hermes import. `Ok(true)` when it was imported.
fn offer_hermes(
    home: &Path,
    options: &Options,
    prompt: bool,
    state: &Hermes,
    out: &mut dyn Write,
) -> Result<bool, i32> {
    let import = options.import_hermes;
    let mode = if options.dry_run {
        hermes::Mode::DryRun
    } else if prompt {
        hermes::Mode::Interactive { import }
    } else {
        hermes::Mode::NonInteractive { import }
    };
    let mut store =
        |found: &hermes::Found, out: &mut dyn Write| hermes::store_import(home, found, out);
    match hermes::offer(
        &state.detection,
        mode,
        &mut hermes::stdin_lock(),
        out,
        &mut store,
    ) {
        hermes::Outcome::Skipped => Ok(false),
        hermes::Outcome::Imported => Ok(true),
        hermes::Outcome::Failed(code) => Err(code),
    }
}

/// Step 1: interpreter aliases in this build's directory (DD-203).
fn install_aliases(home: &Path, source: Option<&Path>, dry_run: bool, out: &mut dyn Write) -> i32 {
    let names = shim_install::alias_names();
    let Some(source) = source else {
        let _ = writeln!(
            out,
            "aliases: skipped, this process is not the clud executable"
        );
        return 0;
    };
    let dir = match crate::version_dir::locate(home, source) {
        Ok(build) => build.interpreter(),
        Err(error) => {
            eprintln!("clud setup: could not read {}: {error}", source.display());
            return 1;
        }
    };
    let missing: Vec<&str> = names
        .iter()
        .filter(|name| !crate::alias_link::is_fresh(source, &dir.join(name)))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        let _ = writeln!(
            out,
            "aliases: all {} already installed in {}",
            names.len(),
            dir.display()
        );
        return 0;
    }
    if dry_run {
        let _ = writeln!(
            out,
            "aliases: would install {} of {} into {}: {}",
            missing.len(),
            names.len(),
            dir.display(),
            missing.join(", ")
        );
        return 0;
    }
    match shim_install::extract_shims_at(home, source) {
        Ok(count) => {
            let _ = writeln!(
                out,
                "aliases: installed {count} of {} into {}",
                names.len(),
                dir.display()
            );
            0
        }
        Err(error) => {
            eprintln!("clud setup: could not install aliases: {error}");
            1
        }
    }
}

/// Step 2: provider, its credential, and the saved default.
/// The interactive provider step alone, for the native installer after a
/// verified install: the same code `clud setup` runs as its step 2.
pub fn run_provider_step_interactive() -> i32 {
    let Some(home) = crate::home::user_home() else {
        eprintln!("clud setup: cannot resolve the home directory");
        return 1;
    };
    let mut stdout = std::io::stdout().lock();
    let code = match configure_provider(&home, &Options::default(), true, &mut stdout) {
        Ok(provider) => {
            let mut stdin = std::io::stdin().lock();
            let mut host = crate::backend_bootstrap::ProductionBackendBootstrapHost;
            frontend::ensure_frontend(
                provider,
                frontend::Mode::Prompt,
                &mut stdin,
                &mut host,
                &mut stdout,
            )
        }
        Err(code) => code,
    };
    let _ = stdout.flush();
    code
}

fn configure_provider(
    home: &Path,
    options: &Options,
    prompt: bool,
    out: &mut dyn Write,
) -> Result<ModelProvider, i32> {
    let current = crate::clud_settings::load_launch_preferences_read_only_at(home)
        .ok()
        .and_then(|snapshot| snapshot.global.model_provider);
    let provider = match (options.provider, prompt) {
        (Some(provider), _) => provider,
        (None, true) => provider_setup::choose_provider()?,
        (None, false) => scripted_provider(current, openrouter_key_available()),
    };
    if provider == ModelProvider::OpenRouter {
        let code = configure_openrouter(home, options.dry_run, prompt, out);
        if code != 0 {
            return Err(code);
        }
    } else {
        let _ = writeln!(
            out,
            "credentials: {} signs in through its own subscription login; nothing stored by clud",
            provider.as_str()
        );
    }
    match save_default(home, provider, current, options.dry_run, out) {
        0 => Ok(provider),
        code => Err(code),
    }
}

/// The provider a scripted setup (`--yes`, `--dry-run`) picks without
/// `--provider`: the saved one when setup offers it, else OpenRouter when its
/// key is available (setup prefers OpenRouter, #2752), else Claude.
fn scripted_provider(current: Option<ModelProvider>, openrouter_key: bool) -> ModelProvider {
    match current.filter(|provider| provider_setup::OFFERED.contains(provider)) {
        Some(saved) => saved,
        None if openrouter_key => ModelProvider::OpenRouter,
        None => ModelProvider::Claude,
    }
}

/// An OpenRouter key in the credential store or `OPENROUTER_API_KEY`.
fn openrouter_key_available() -> bool {
    let descriptor = crate::provider_registry::descriptor_for(ModelProvider::OpenRouter)
        .expect("OpenRouter descriptor");
    crate::provider_auth::has_well_formed_persisted_key(descriptor).unwrap_or(false)
        || std::env::var(descriptor.api_key_env).is_ok_and(|key| !key.trim().is_empty())
}

fn save_default(
    home: &Path,
    provider: ModelProvider,
    current: Option<ModelProvider>,
    dry_run: bool,
    out: &mut dyn Write,
) -> i32 {
    let name = provider.as_str();
    if current == Some(provider) {
        let _ = writeln!(out, "provider: already {name}");
        return 0;
    }
    if dry_run {
        let _ = writeln!(out, "provider: would set the default provider to {name}");
        return 0;
    }
    match provider_setup::save_provider(home, provider) {
        Ok(()) => {
            let _ = writeln!(out, "provider: default set to {name}");
            0
        }
        Err(error) => {
            eprintln!("clud setup: could not save provider: {error}");
            1
        }
    }
}

fn configure_openrouter(home: &Path, dry_run: bool, prompt: bool, out: &mut dyn Write) -> i32 {
    let descriptor = crate::provider_registry::descriptor_for(ModelProvider::OpenRouter)
        .expect("OpenRouter descriptor");
    let stored = crate::provider_auth::has_well_formed_persisted_key(descriptor).unwrap_or(false);
    let env_key = std::env::var(descriptor.api_key_env)
        .ok()
        .filter(|key| !key.trim().is_empty());
    let code = if stored {
        let _ = writeln!(out, "credentials: OpenRouter key already stored");
        0
    } else if dry_run {
        let source = if env_key.is_some() {
            descriptor.api_key_env
        } else {
            "the masked prompt"
        };
        let _ = writeln!(
            out,
            "credentials: would store the OpenRouter key from {source} in the credential store"
        );
        0
    } else if let Some(key) = env_key {
        let _ = out.flush();
        crate::provider_auth::login_with_key(descriptor, key)
    } else if prompt {
        provider_setup::login_openrouter().err().unwrap_or(0)
    } else {
        eprintln!(
            "clud setup: no OpenRouter key stored; set {} or run `clud auth login openrouter`",
            descriptor.api_key_env
        );
        EXIT_USAGE
    };
    if code != 0 {
        return code;
    }
    if !prompt {
        return default_openrouter_model(home, dry_run, out);
    }
    match provider_setup::choose_openrouter_model() {
        Ok(model) => {
            let _ = writeln!(out, "model: OpenRouter default set to {model}");
            0
        }
        Err(code) => code,
    }
}

/// Without the model menu (`--yes`, `--dry-run`), save the menu's default
/// OpenRouter model unless one is already saved, so a scripted setup launches
/// the recommended model rather than the catalog fallback (#2749).
fn default_openrouter_model(home: &Path, dry_run: bool, out: &mut dyn Write) -> i32 {
    let saved = crate::clud_settings::load_launch_preferences_read_only_at(home)
        .ok()
        .and_then(|snapshot| snapshot.profile(ModelProvider::OpenRouter)?.model.clone());
    let model = provider_setup::DEEPSEEK;
    if let Some(saved) = saved {
        let _ = writeln!(out, "model: OpenRouter default already {saved}");
        return 0;
    }
    if dry_run {
        let _ = writeln!(out, "model: would set the OpenRouter default to {model}");
        return 0;
    }
    let patch = crate::clud_settings::GlobalSettingsPatch {
        provider_profiles: vec![crate::clud_settings::ProviderProfilePatch {
            provider: Some(ModelProvider::OpenRouter),
            model: Some(model.to_owned()),
            ..crate::clud_settings::ProviderProfilePatch::default()
        }],
        ..crate::clud_settings::GlobalSettingsPatch::default()
    };
    match crate::clud_settings::save_settings_patch_at(home, patch) {
        Ok(()) => {
            let _ = writeln!(out, "model: OpenRouter default set to {model}");
            0
        }
        Err(error) => {
            eprintln!("clud setup: could not save OpenRouter model: {error}");
            1
        }
    }
}

/// Step 3: shell profiles and PATH, only on request.
fn profile_step(options: &Options, out: &mut dyn Write) -> i32 {
    if !options.write_profile {
        let _ = writeln!(
            out,
            "shell profiles and PATH: not touched (pass --write-profile to activate clud in new shells)"
        );
        return 0;
    }
    if options.dry_run {
        let _ = writeln!(
            out,
            "shell profiles and PATH: would run the installer for this version with shell activation"
        );
        return 0;
    }
    let _ = out.flush();
    crate::self_install::entry::install_current_with_profile(options.yes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dry_run: bool, yes: bool, provider: Option<ModelProvider>) -> Options {
        Options {
            dry_run,
            yes,
            provider,
            write_profile: false,
            import_hermes: false,
        }
    }

    fn fake_clud(dir: &Path) -> std::path::PathBuf {
        let exe = dir.join(if crate::platform::host::WINDOWS {
            "clud.exe"
        } else {
            "clud"
        });
        std::fs::write(&exe, b"not really clud").unwrap();
        exe
    }

    fn run_capture(home: &Path, source: &Path, options: &Options, tty: bool) -> (i32, String) {
        let mut out = Vec::new();
        let none = Hermes {
            detection: hermes::Detection::None,
        };
        let mut host = frontend::tests_support::Present;
        let code = run_at(home, Some(source), options, tty, &none, &mut host, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    /// #2752: a scripted setup prefers OpenRouter when its key is available,
    /// and never replaces a saved provider.
    #[test]
    fn scripted_setup_prefers_openrouter_when_a_key_is_available() {
        assert_eq!(scripted_provider(None, true), ModelProvider::OpenRouter);
        assert_eq!(scripted_provider(None, false), ModelProvider::Claude);
        for saved in provider_setup::OFFERED {
            assert_eq!(scripted_provider(Some(saved), true), saved);
            assert_eq!(scripted_provider(Some(saved), false), saved);
        }
        // A saved provider setup does not offer falls back like no provider.
        assert_eq!(
            scripted_provider(Some(ModelProvider::Kimi), true),
            ModelProvider::OpenRouter
        );
    }

    #[test]
    fn check_needs_setup_without_a_provider_or_an_openrouter_key() {
        assert!(needs_setup(None, false));
        assert!(needs_setup(None, true));
        assert!(needs_setup(Some(ModelProvider::OpenRouter), false));
        assert!(!needs_setup(Some(ModelProvider::OpenRouter), true));
        for provider in provider_setup::OFFERED {
            if provider != ModelProvider::OpenRouter {
                assert!(!needs_setup(Some(provider), false), "{provider:?}");
            }
        }
    }

    #[test]
    fn non_terminal_without_yes_is_refused_before_any_write() {
        let root = tempfile::tempdir().unwrap();
        let source = fake_clud(root.path());
        let home = root.path().join("home");
        let (code, _) = run_capture(&home, &source, &opts(false, false, None), false);
        assert_eq!(code, EXIT_USAGE);
        assert!(!home.exists());
    }

    #[test]
    fn dry_run_writes_nothing_and_lists_every_step() {
        let root = tempfile::tempdir().unwrap();
        let source = fake_clud(root.path());
        let home = root.path().join("home");
        let (code, out) = run_capture(&home, &source, &opts(true, false, None), false);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("would install"), "{out}");
        assert!(
            out.contains("would set the default provider to claude"),
            "{out}"
        );
        assert!(out.contains("not touched"), "{out}");
        assert!(!home.exists(), "dry run created {}", home.display());
    }

    #[test]
    fn yes_is_idempotent_for_every_subscription_provider() {
        for provider in provider_setup::OFFERED
            .into_iter()
            .filter(|provider| *provider != ModelProvider::OpenRouter)
        {
            let root = tempfile::tempdir().unwrap();
            let source = fake_clud(root.path());
            let home = root.path().join("home");
            let options = opts(false, true, Some(provider));
            let (code, first) = run_capture(&home, &source, &options, false);
            assert_eq!(code, 0, "{first}");
            assert!(first.contains("aliases: installed"), "{first}");
            let (code, second) = run_capture(&home, &source, &options, false);
            assert_eq!(code, 0, "{second}");
            assert!(second.contains("already installed"), "{second}");
            assert!(
                second.contains(&format!("provider: already {}", provider.as_str())),
                "{second}"
            );
        }
    }

    fn saved_openrouter_model(home: &Path) -> Option<String> {
        crate::clud_settings::load_launch_preferences_read_only_at(home)
            .ok()?
            .profile(ModelProvider::OpenRouter)?
            .model
            .clone()
    }

    /// #2749: a non-interactive OpenRouter setup saves the same default the
    /// menu would select (DeepSeek V4.1 Flash, 1017 checkpoint).
    #[test]
    fn non_interactive_openrouter_setup_saves_the_default_model() {
        let root = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        assert_eq!(default_openrouter_model(root.path(), false, &mut out), 0);
        assert_eq!(
            saved_openrouter_model(root.path()).as_deref(),
            Some("deepseek/deepseek-v4.1-flash-1017")
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("model: OpenRouter default set to deepseek/deepseek-v4.1-flash-1017"),
            "{text}"
        );
    }

    #[test]
    fn non_interactive_openrouter_setup_keeps_a_saved_model() {
        let root = tempfile::tempdir().unwrap();
        crate::clud_settings::save_settings_patch_at(
            root.path(),
            crate::clud_settings::GlobalSettingsPatch {
                provider_profiles: vec![crate::clud_settings::ProviderProfilePatch {
                    provider: Some(ModelProvider::OpenRouter),
                    model: Some("z-ai/glm-5.3-flash".into()),
                    ..crate::clud_settings::ProviderProfilePatch::default()
                }],
                ..crate::clud_settings::GlobalSettingsPatch::default()
            },
        )
        .unwrap();
        let mut out = Vec::new();
        assert_eq!(default_openrouter_model(root.path(), false, &mut out), 0);
        assert_eq!(
            saved_openrouter_model(root.path()).as_deref(),
            Some("z-ai/glm-5.3-flash")
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("model: OpenRouter default already z-ai/glm-5.3-flash"),
            "{text}"
        );
    }

    #[test]
    fn dry_run_reports_the_default_model_and_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        assert_eq!(default_openrouter_model(root.path(), true, &mut out), 0);
        assert_eq!(saved_openrouter_model(root.path()), None);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "model: would set the OpenRouter default to deepseek/deepseek-v4.1-flash-1017"
            ),
            "{text}"
        );
    }

    #[test]
    fn providers_setup_does_not_offer_are_rejected() {
        for &provider in crate::self_install::setup::NOT_OFFERED {
            let root = tempfile::tempdir().unwrap();
            let source = fake_clud(root.path());
            let (code, _) = run_capture(
                &root.path().join("home"),
                &source,
                &opts(true, true, Some(provider)),
                false,
            );
            assert_eq!(code, EXIT_USAGE, "{provider:?}");
        }
    }
}

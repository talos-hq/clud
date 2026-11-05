//! Hermes integration boundary: setup imports and opt-in personality references.
//!
//! Security boundary: this is the only code in clud that reads Hermes'
//! config (`$HERMES_HOME/config.yaml`, `~/.hermes/config.yaml`,
//! `~/.hermes/profiles/*/config.yaml`), its `.env`, or `HERMES_HOME`. The
//! module is private to [`crate::setup_command`]. Launches can request only a
//! reference to SOUL.md, without reading it or discovering credentials. A source-scan test fails if a
//! Hermes path or variable appears anywhere else in `src/`.
//!
//! Setup only *offers* the import: it prints what it found and asks
//! `Import these OpenRouter settings into clud? [y/N]` (default no). `--yes`
//! never imports without `--import-hermes`, and an import still needs the
//! metered consent (`CLUD_ALLOW_METERED=1`, `CLUD_METERED_PROVIDER=openrouter`)
//! because OpenRouter bills per token. The key is never printed.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::backend::ModelProvider;
use crate::metered_policy;

pub(super) fn context_reference_from_process(home: &Path) -> Result<String, String> {
    context_reference(
        home,
        std::env::var_os("HERMES_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    )
}

fn context_reference(home: &Path, instance: Option<PathBuf>) -> Result<String, String> {
    let path = instance
        .unwrap_or_else(|| home.join(".hermes"))
        .join("SOUL.md");
    let path = path.canonicalize().map_err(|error| format!(
        "cannot resolve optional Hermes personality file {}: {error}; disable with `clud config set context.hermes.enabled=false`",
        path.display()))?;
    if !path.is_file() {
        return Err(format!(
            "optional Hermes personality reference is not a file: {}",
            path.display()
        ));
    }
    let quoted = serde_json::to_string(&path.to_string_lossy()).map_err(|e| e.to_string())?;
    Ok(format!("Optional personality context: Hermes SOUL.md is at {quoted}. You may read this file when personality or communication guidance would be useful. Reading it is optional."))
}

/// OpenRouter's endpoint, used when Hermes leaves `base_url` empty.
pub const OPENROUTER_DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const KEY_ENV: &str = "OPENROUTER_API_KEY";
pub const PROMPT: &str = "Import these OpenRouter settings into clud? [y/N] ";

/// What setup found in one Hermes config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub base_url: String,
    pub models: Vec<String>,
    /// Where the key was found (never the key itself) and the key.
    pub key: Option<(String, String)>,
}

/// Detection result: nothing, an unreadable config, or settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    None,
    Unparsable(PathBuf, String),
    Found(Found),
}

/// Candidate configs, in priority order: `$HERMES_HOME`, the default home,
/// then profile dirs.
fn candidates(home: &Path, hermes_home: Option<PathBuf>) -> Vec<PathBuf> {
    let default = home.join(".hermes");
    let mut dirs: Vec<PathBuf> = hermes_home.into_iter().collect();
    dirs.push(default.clone());
    let mut profiles: Vec<PathBuf> = std::fs::read_dir(default.join("profiles"))
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    profiles.sort();
    dirs.extend(profiles);
    let mut seen = Vec::new();
    dirs.into_iter()
        .map(|dir| dir.join("config.yaml"))
        .filter(|path| {
            let fresh = path.is_file() && !seen.contains(path);
            if fresh {
                seen.push(path.clone());
            }
            fresh
        })
        .collect()
}

fn str_at<'a>(value: &'a serde_yaml::Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |node, key| node.get(*key))
        .and_then(serde_yaml::Value::as_str)
}

/// OpenRouter settings in one parsed config, or `None` when it has none.
fn settings(doc: &serde_yaml::Value) -> Option<(String, Vec<String>)> {
    let provider = str_at(doc, &["provider"]).or_else(|| str_at(doc, &["model", "provider"]));
    let overrides = doc
        .get("model_overrides")
        .and_then(|node| node.get("openrouter"))
        .and_then(serde_yaml::Value::as_mapping);
    let section = doc.get("providers").and_then(|node| node.get("openrouter"));
    let is_openrouter = provider.is_some_and(|name| name.eq_ignore_ascii_case("openrouter"));
    if !is_openrouter && overrides.is_none() && section.is_none() {
        return None;
    }
    let base_url = [
        section.and_then(|node| str_at(node, &["base_url"])),
        is_openrouter
            .then(|| str_at(doc, &["base_url"]).or_else(|| str_at(doc, &["model", "base_url"])))
            .flatten(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|url| !url.is_empty())
    .unwrap_or(OPENROUTER_DEFAULT_BASE_URL)
    .to_string();
    let mut models: Vec<String> = Vec::new();
    let mut push = |model: &str| {
        let model = model.trim();
        if !model.is_empty() && !models.iter().any(|known| known == model) {
            models.push(model.to_string());
        }
    };
    if is_openrouter {
        str_at(doc, &["model", "default"])
            .or_else(|| str_at(doc, &["model"]))
            .map(&mut push);
    }
    if let Some(model) = section.and_then(|node| str_at(node, &["model"])) {
        push(model);
    }
    for key in overrides.into_iter().flat_map(|map| map.keys()) {
        if let Some(model) = key.as_str() {
            push(model);
        }
    }
    Some((base_url, models))
}

/// `OPENROUTER_API_KEY` from the process environment, else Hermes' `.env`
/// beside the config.
fn find_key(config: &Path, env_key: Option<String>) -> Option<(String, String)> {
    if let Some(key) = env_key.filter(|key| !key.trim().is_empty()) {
        return Some((format!("{KEY_ENV} (environment)"), key.trim().to_string()));
    }
    let dotenv = config.with_file_name(".env");
    let text = std::fs::read_to_string(&dotenv).ok()?;
    text.lines().find_map(|line| {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let value = line.strip_prefix(KEY_ENV)?.trim_start().strip_prefix('=')?;
        let value = value.trim().trim_matches(['"', '\'']);
        (!value.is_empty()).then(|| (format!("{KEY_ENV} in {}", dotenv.display()), value.into()))
    })
}

/// Read-only detection. `hermes_home` and `env_key` are injected so tests
/// need not mutate the process environment.
pub fn detect(home: &Path, hermes_home: Option<PathBuf>, env_key: Option<String>) -> Detection {
    let mut unparsable = None;
    for path in candidates(home, hermes_home) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_yaml::from_str::<serde_yaml::Value>(&text) {
            Ok(doc) => {
                if let Some((base_url, models)) = settings(&doc) {
                    let key = find_key(&path, env_key);
                    return Detection::Found(Found {
                        path,
                        base_url,
                        models,
                        key,
                    });
                }
            }
            Err(error) => {
                unparsable.get_or_insert((path, error.to_string()));
            }
        }
    }
    unparsable.map_or(Detection::None, |(path, error)| {
        Detection::Unparsable(path, error)
    })
}

/// Detection against the real process environment. Setup-only.
pub fn detect_from_process(home: &Path) -> Detection {
    let hermes_home = std::env::var_os("HERMES_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    detect(home, hermes_home, std::env::var(KEY_ENV).ok())
}

/// How the user answered the offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    DryRun,
    /// `--yes`: import only when `--import-hermes` was passed.
    NonInteractive {
        import: bool,
    },
    /// Prompt, unless `--import-hermes` already answered.
    Interactive {
        import: bool,
    },
}

/// The step's result for the rest of setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing imported; setup continues as before.
    Skipped,
    /// Key and model stored; setup should make OpenRouter the default.
    Imported,
    /// Refused or failed with this exit code; setup stops.
    Failed(i32),
}

fn report(found: &Found, out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "hermes: found OpenRouter settings in {}",
        found.path.display()
    );
    let _ = writeln!(out, "  base_url: {}", found.base_url);
    let models = if found.models.is_empty() {
        "(none)".to_string()
    } else {
        found.models.join(", ")
    };
    let _ = writeln!(out, "  models: {models}");
    match &found.key {
        Some((source, _)) => {
            let _ = writeln!(out, "  key: found in {source} (not shown)");
        }
        None => {
            let _ = writeln!(out, "  key: not found ({KEY_ENV} is unset)");
        }
    }
}

fn ask(input: &mut dyn BufRead, out: &mut dyn Write) -> bool {
    let _ = write!(out, "{PROMPT}");
    let _ = out.flush();
    let mut answer = String::new();
    input.read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Offer the import. `store` performs the write (credential store, model,
/// provider); it is injected so tests can observe it.
pub fn offer(
    detection: &Detection,
    mode: Mode,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    metered_ok: bool,
    store: &mut dyn FnMut(&Found, &mut dyn Write) -> i32,
) -> Outcome {
    let requested = matches!(
        mode,
        Mode::NonInteractive { import: true } | Mode::Interactive { import: true }
    );
    let found = match detection {
        Detection::Found(found) => found,
        Detection::None if requested => {
            eprintln!("clud setup: --import-hermes: no Hermes OpenRouter settings found");
            return Outcome::Failed(crate::setup_command::EXIT_USAGE);
        }
        Detection::None => return Outcome::Skipped,
        Detection::Unparsable(path, error) => {
            let _ = writeln!(
                out,
                "hermes: could not parse {}; nothing imported ({error})",
                path.display()
            );
            return if requested {
                Outcome::Failed(crate::setup_command::EXIT_USAGE)
            } else {
                Outcome::Skipped
            };
        }
    };
    report(found, out);
    let accept = match mode {
        Mode::DryRun => {
            let _ = writeln!(
                out,
                "hermes: would import these settings with --import-hermes (dry run; nothing written)"
            );
            return Outcome::Skipped;
        }
        Mode::NonInteractive { import } => import,
        Mode::Interactive { import } => import || ask(input, out),
    };
    if !accept {
        let _ = writeln!(
            out,
            "hermes: not imported (pass --import-hermes to import these settings)"
        );
        return Outcome::Skipped;
    }
    import(found, out, metered_ok, store)
}

fn import(
    found: &Found,
    out: &mut dyn Write,
    metered_ok: bool,
    store: &mut dyn FnMut(&Found, &mut dyn Write) -> i32,
) -> Outcome {
    if found.key.is_none() {
        eprintln!(
            "clud setup: Hermes OpenRouter settings found in {} but no OpenRouter API key: \
             set {KEY_ENV} (Hermes usually keeps it outside config.yaml) and rerun; nothing imported",
            found.path.display()
        );
        return Outcome::Failed(crate::setup_command::EXIT_USAGE);
    }
    let _ = writeln!(
        out,
        "{}",
        metered_policy::metered_warning(ModelProvider::OpenRouter)
    );
    if !metered_ok {
        eprintln!(
            "clud setup: OpenRouter is metered (pay-per-token); importing needs \
             CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER=openrouter; nothing imported"
        );
        return Outcome::Failed(crate::setup_command::EXIT_USAGE);
    }
    if found.base_url.trim_end_matches('/') != OPENROUTER_DEFAULT_BASE_URL {
        let _ = writeln!(
            out,
            "hermes: base_url {} not imported; clud uses OpenRouter's own endpoint",
            found.base_url
        );
    }
    match store(found, out) {
        0 => Outcome::Imported,
        code => Outcome::Failed(code),
    }
}

/// Write an accepted import through the existing credential store and
/// settings: key, OpenRouter model (the first one found).
pub fn store_import(home: &Path, found: &Found, out: &mut dyn Write) -> i32 {
    let Some((_, key)) = &found.key else {
        return crate::setup_command::EXIT_USAGE;
    };
    let descriptor = crate::provider_registry::descriptor_for(ModelProvider::OpenRouter)
        .expect("OpenRouter descriptor");
    let _ = out.flush();
    let code = crate::provider_auth::login_with_key(descriptor, key.clone());
    if code != 0 {
        return code;
    }
    let Some(model) = found.models.first() else {
        return 0;
    };
    let patch = crate::clud_settings::GlobalSettingsPatch {
        provider_profiles: vec![crate::clud_settings::ProviderProfilePatch {
            provider: Some(ModelProvider::OpenRouter),
            model: Some(model.clone()),
            ..crate::clud_settings::ProviderProfilePatch::default()
        }],
        ..crate::clud_settings::GlobalSettingsPatch::default()
    };
    match crate::clud_settings::save_settings_patch_at(home, patch) {
        Ok(()) => {
            let _ = writeln!(
                out,
                "model: OpenRouter default set to {model} (from Hermes)"
            );
            0
        }
        Err(error) => {
            eprintln!("clud setup: could not save OpenRouter model: {error}");
            1
        }
    }
}

/// Metered consent for an import, from the real environment.
pub fn metered_consent() -> bool {
    metered_policy::process_allows_metered(ModelProvider::OpenRouter)
}

pub fn stdin_lock() -> io::StdinLock<'static> {
    io::stdin().lock()
}

#[cfg(test)]
#[path = "hermes_tests.rs"]
mod tests;

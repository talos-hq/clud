//! User-level clud settings persisted under `~/.clud/settings.json`.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use serde_json::{json, Map, Value};
use toml_edit::{DocumentMut, Item};

use crate::backend::{
    resolve_launch_target, Backend, HarnessSelection, LaunchTargetError, ModelProvider,
};
use crate::launch_setup::LaunchSetupScope;
use crate::provider_catalog::{self, EffortLevel};

pub const CLUD_DIR_NAME: &str = ".clud";
pub const SETTINGS_FILE_NAME: &str = "settings.json";
pub const LEGACY_SETTINGS_FILE_NAME: &str = "settings.toml";
pub const LOCK_FILE_NAME: &str = "settings.lock";

#[path = "settings_change.rs"]
pub mod authorized;
pub const DEFAULT_CODEX_GITHUB_PLUGIN_CONFIG_OVERRIDE: &str =
    "plugins.\"github@openai-curated\".enabled=false";
const CODEX_CONFIG_OVERRIDES_NOTE: &str =
    "clud passes these strings as repeated `codex -c` config overrides before the Codex subcommand. Edit config_overrides to change plugin/connector behavior.";
const SHELL_DISABLE_POWERSHELL_NOTE: &str =
    "When true, clud injects a PreToolUse hook into Claude and Codex that denies any Bash/Shell call resolving to powershell.exe / pwsh / *.ps1. For Claude it also sets CLAUDE_CODE_USE_POWERSHELL_TOOL=0 + CLAUDE_CODE_GIT_BASH_PATH to a vendored bash. Also sets CLUD_DISABLE_POWERSHELL=1 in the backend env so skills/CLAUDE.md content can branch on it. Per-backend overrides under shell.claude.disable_powershell / shell.codex.disable_powershell take precedence; null inherits the top-level value. Default false. See https://github.com/zackees/clud/issues/447.";
const GIT_PR_WAIT_FAIL_FAST_NOTE: &str =
    "When true, session gh PR-check watches use the bundled fail-fast waiter; cmd-scan still denies hand-rolled PR polling and native PR watches when the shim is unavailable; run watches are not PR waits (DD-189). On by default (DD-065); toggle with `clud settings`.";

const GIT_GH_READ_BROKER_NOTE: &str =
    "When true, session `gh api` GET calls are answered by the clud daemon's read broker: a shared ETag cache with a 30-60 s TTL that revalidates with free 304s and saves the GitHub REST budget. Output stays byte-identical; any miss runs the real gh. On by default (#1743, DD-150); toggle with `clud settings`.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalLaunchPreferences {
    pub model_provider: Option<ModelProvider>,
    pub harness: Option<HarnessSelection>,
}

/// How the Claude-harness bridge handles a discovered Codex CLI login.
/// Absent is intentionally distinct from `Never`: it means ask on an
/// interactive bridge launch, preserving the explicit-choice boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexCliLoginImport {
    Never,
    Always,
}

impl CodexCliLoginImport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Always => "always",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "never" => Some(Self::Never),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalSettingsPatch {
    pub openrouter_zdr: Option<bool>,
    pub model_provider: Option<ModelProvider>,
    pub harness: Option<HarnessSelection>,
    pub pr_wait_fail_fast: Option<bool>,
    /// `git.gh_read_broker` (#1743).
    pub gh_read_broker: Option<bool>,
    pub web_term: Option<bool>,
    /// `gc.delete_remote_branches` (#1603, #1608).
    pub gc_delete_remote_branches: Option<bool>,
    pub block_cd: Option<crate::repo_clud_config::BlockCd>,
    pub provider_profiles: Vec<ProviderProfilePatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfile {
    pub provider: ModelProvider,
    pub model: Option<String>,
    pub harness: Option<HarnessSelection>,
    pub effort: Option<EffortLevel>,
    pub context_window: Option<String>,
}

impl ProviderProfile {
    pub fn selection_defaults(&self) -> provider_catalog::ProviderSelectionDefaults<'_> {
        provider_catalog::ProviderSelectionDefaults {
            model: self.model.as_deref(),
            effort: self.effort,
            context_window: self.context_window.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderProfilePatch {
    pub provider: Option<ModelProvider>,
    pub model: Option<String>,
    pub harness: Option<HarnessSelection>,
    pub effort: Option<Option<EffortLevel>>,
    pub context_window: Option<Option<String>>,
}

/// #1304: the OpenRouter model a launch should remember as the next default,
/// if any. Only an explicit `--model` on a live `--openrouter` launch counts:
/// a dry run changes nothing, and re-saving the model already stored is a
/// no-op so the settings file is not rewritten on every launch.
pub fn openrouter_model_to_remember(
    provider: ModelProvider,
    cli_model: Option<&str>,
    dry_run: bool,
    resolved_model: Option<&str>,
    saved_model: Option<&str>,
) -> Option<String> {
    if provider != ModelProvider::OpenRouter || cli_model.is_none() || dry_run {
        return None;
    }
    resolved_model
        .filter(|model| Some(*model) != saved_model)
        .map(str::to_string)
}

/// Persist `model` as OpenRouter's saved default, leaving the rest of its
/// profile untouched.
pub fn save_openrouter_model(model: String) -> Result<(), SettingsError> {
    save_settings_patch(GlobalSettingsPatch {
        provider_profiles: vec![ProviderProfilePatch {
            provider: Some(ModelProvider::OpenRouter),
            model: Some(model),
            ..ProviderProfilePatch::default()
        }],
        ..GlobalSettingsPatch::default()
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchPreferencesSnapshot {
    pub openrouter_zdr: Option<bool>,
    pub global: GlobalLaunchPreferences,
    pub provider_profiles: Vec<ProviderProfile>,
}

impl LaunchPreferencesSnapshot {
    pub fn profile(&self, provider: ModelProvider) -> Option<&ProviderProfile> {
        self.provider_profiles
            .iter()
            .find(|profile| profile.provider == provider)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustOptimizeSettings {
    pub use_soldr_shims: bool,
    pub install_soldr: bool,
    pub soldr_version: String,
}

impl Default for RustOptimizeSettings {
    fn default() -> Self {
        Self {
            use_soldr_shims: true,
            install_soldr: true,
            soldr_version: "latest".to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("could not resolve user home directory")]
    NoHomeDir,
    #[error("{0}")]
    Io(io::Error),
    #[error("malformed settings in {}: {error}", path.display())]
    Parse { path: PathBuf, error: String },
    #[error("invalid launch preferences: {0}")]
    InvalidLaunchTarget(LaunchTargetError),
    #[error("invalid providers.{provider} settings: {error}")]
    InvalidProviderProfile {
        provider: ModelProvider,
        error: String,
    },
}

impl From<io::Error> for SettingsError {
    fn from(error: io::Error) -> Self {
        SettingsError::Io(error)
    }
}

pub fn settings_path_at(home: &Path) -> PathBuf {
    home.join(CLUD_DIR_NAME).join(SETTINGS_FILE_NAME)
}

pub fn legacy_settings_path_at(home: &Path) -> PathBuf {
    home.join(CLUD_DIR_NAME).join(LEGACY_SETTINGS_FILE_NAME)
}

pub fn default_codex_config_overrides() -> Vec<String> {
    vec![DEFAULT_CODEX_GITHUB_PLUGIN_CONFIG_OVERRIDE.to_string()]
}

pub fn home_dir_path() -> Result<PathBuf, SettingsError> {
    crate::home::user_home().ok_or(SettingsError::NoHomeDir)
}

pub fn seeded_global_settings_document() -> Value {
    let mut document = json!({});
    seed_global_settings_defaults(&mut document);
    document
}

pub fn seed_global_settings_defaults(document: &mut Value) {
    if let Some(shell) = seed_object_entry(document, "shell") {
        shell
            .entry("disable_powershell".to_string())
            .or_insert(Value::Bool(false));
    }

    if let Some(hook_health) = seed_object_entry(document, "hook_health") {
        hook_health
            .entry("auto_fix_hooks".to_string())
            .or_insert(Value::Bool(true));
    }

    if let Some(git) = seed_object_entry(document, "git") {
        git.entry("pr_wait_fail_fast".to_string())
            .or_insert(Value::Bool(true));
        git.entry("gh_read_broker".to_string())
            .or_insert(Value::Bool(true));
    }

    if let Some(web_term) = seed_object_entry(document, "web_term") {
        web_term
            .entry("enabled".to_string())
            .or_insert(Value::Bool(false));
    }

    if let Some(gc) = seed_object_entry(document, "gc") {
        // #1603: when the daemon reclaims a squash-merged repo worktree it
        // deletes the local branch; the remote branch only on this opt-in.
        gc.entry("delete_remote_branches".to_string())
            .or_insert(Value::Bool(false));
    }

    if let Some(worktrees) = seed_object_entry(document, "worktrees") {
        // #1485: warn-only size threshold for ~/.clud/worktrees; 0 disables.
        worktrees
            .entry("warn_bytes".to_string())
            .or_insert(json!(crate::gc::worktree_root::DEFAULT_WARN_BYTES));
    }

    if let Some(tmp) = seed_object_entry(document, "tmp") {
        // #1327: warn-only size threshold for ~/.clud/tmp; 0 disables.
        tmp.entry("warn_bytes".to_string()).or_insert(json!(
            crate::gc::worktree_size_cache::DEFAULT_TMP_WARN_BYTES
        ));
        // #2397 (DD-194): size cap, oldest idle sessions evicted first; 0 disables.
        tmp.entry("max_bytes".to_string())
            .or_insert(json!(crate::gc::session_tmp_cap::DEFAULT_TMP_MAX_BYTES));
    }

    if let Some(cache) = seed_object_entry(document, "cache") {
        // #1691: warn-only size threshold for ~/.clud/cache; 0 disables.
        cache.entry("warn_bytes".to_string()).or_insert(json!(
            crate::gc::worktree_size_cache::DEFAULT_CACHE_WARN_BYTES
        ));
        // #1691 (DD-145): size cap enforced via `uv cache clean`; 0 disables.
        cache
            .entry("max_bytes".to_string())
            .or_insert(json!(crate::gc::uv_cache::DEFAULT_MAX_BYTES));
    }

    if let Some(trash) = seed_object_entry(document, "trash") {
        // #1672: size cap on ~/.clud/trash, oldest evicted first; 0 disables.
        trash
            .entry("max_bytes".to_string())
            .or_insert(json!(crate::rm_tool::TRASH_MAX_BYTES_DEFAULT));
    }

    if let Some(daemon) = seed_object_entry(document, "daemon") {
        // #645: a daemon that has no owned work may retire after fifteen
        // minutes. Zero remains the documented explicit opt-out.
        daemon
            .entry("idle_timeout_secs".to_string())
            .or_insert(json!(900));
        let proc_sampler = daemon
            .entry("proc_sampler".to_string())
            .or_insert_with(|| json!({}));
        if let Some(proc_sampler) = proc_sampler.as_object_mut() {
            proc_sampler
                .entry("interval_ms".to_string())
                .or_insert(json!(2_000));
        }
        // #465 AC 1: the dead-originator sweep period and its grace window are
        // incident-response knobs, so they belong in settings rather than only
        // in an env var a running daemon cannot see changed.
        let orphan_sweep = daemon
            .entry("orphan_sweep".to_string())
            .or_insert_with(|| json!({}));
        if let Some(orphan_sweep) = orphan_sweep.as_object_mut() {
            orphan_sweep
                .entry("interval_ms".to_string())
                .or_insert(json!(60_000));
            orphan_sweep
                .entry("grace_ms".to_string())
                .or_insert(json!(10_000));
        }
    }

    seed_codex_config_override_defaults(document);
}

pub fn load_or_init_global_settings() -> Result<Value, SettingsError> {
    let home = home_dir_path()?;
    load_or_init_global_settings_at(&home)
}

pub fn load_or_init_global_settings_at(home: &Path) -> Result<Value, SettingsError> {
    let clud_dir = home.join(CLUD_DIR_NAME);
    let lock_path = clud_dir.join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let path = settings_path_at(home);
    let mut document = read_settings_or_legacy(home)?;
    let original = document.clone();
    seed_global_settings_defaults(&mut document);
    if document != original || !path.is_file() {
        write_settings(&path, &document)?;
    }
    Ok(document)
}

pub fn read_settings_json_file(path: &Path) -> Result<Value, SettingsError> {
    let text = fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    parse_json_settings(path, &text)
}

pub fn write_settings_json_file(path: &Path, document: &Value) -> Result<(), SettingsError> {
    write_settings(path, document)
}

pub fn merged_settings_document(global: &Value, local: Option<&Value>) -> Value {
    let mut merged = seeded_global_settings_document();
    merge_settings_into(&mut merged, global);
    if let Some(local) = local {
        merge_settings_into(&mut merged, local);
    }
    merged
}

pub fn load_or_init_codex_config_overrides(
    write_default: bool,
) -> Result<Vec<String>, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_or_init_codex_config_overrides_at(&home, write_default)
}

pub fn load_or_init_codex_config_overrides_at(
    home: &Path,
    write_default: bool,
) -> Result<Vec<String>, SettingsError> {
    let clud_dir = home.join(CLUD_DIR_NAME);
    let lock_path = clud_dir.join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let path = settings_path_at(home);
    let mut document = read_settings_or_legacy(home)?;

    match read_codex_config_overrides(&document, &path)? {
        Some(overrides) => Ok(overrides),
        None if write_default => {
            seed_global_settings_defaults(&mut document);
            write_settings(&path, &document)?;
            Ok(default_codex_config_overrides())
        }
        None => Ok(default_codex_config_overrides()),
    }
}

pub fn load_default_backend() -> Result<Option<Backend>, SettingsError> {
    Ok(load_default_model_provider()?.map(ModelProvider::native_harness))
}

/// Last harness chosen by the bare-launch picker. This is interaction history,
/// not provider-routing policy, so it deliberately lives outside
/// `backend.default` and `harness.default`.
pub fn load_last_launcher_harness() -> Result<Option<Backend>, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_last_launcher_harness_at(&home)
}

pub fn load_last_launcher_harness_at(home: &Path) -> Result<Option<Backend>, SettingsError> {
    let document = read_settings_or_legacy(home)?;
    Ok(document
        .get("launcher")
        .and_then(|value| value.get("last_harness"))
        .and_then(Value::as_str)
        .and_then(Backend::from_settings_str))
}

pub fn save_last_launcher_harness(backend: Backend) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_last_launcher_harness_at(&home, backend)
}

pub fn save_last_launcher_harness_at(home: &Path, backend: Backend) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        let launcher = object_entry(document, "launcher");
        launcher.insert(
            "last_harness".to_string(),
            Value::String(
                match backend {
                    Backend::Claude => "claude",
                    Backend::Codex => "codex",
                    Backend::DeepSeek => "deepseek",
                    Backend::OpenCode => "opencode",
                }
                .to_string(),
            ),
        );
    })
}

pub fn load_codex_cli_login_import_at(
    home: &Path,
) -> Result<Option<CodexCliLoginImport>, SettingsError> {
    let document = read_settings_or_legacy(home)?;
    Ok(document
        .get("codex")
        .and_then(|value| value.get("import_cli_login"))
        .and_then(Value::as_str)
        .and_then(CodexCliLoginImport::from_str))
}

pub fn save_codex_cli_login_import_at(
    home: &Path,
    preference: CodexCliLoginImport,
) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        object_entry(document, "codex").insert(
            "import_cli_login".to_string(),
            Value::String(preference.as_str().to_string()),
        );
    })
}

pub fn load_default_model_provider() -> Result<Option<ModelProvider>, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_default_model_provider_at(&home)
}

pub fn load_default_backend_at(home: &Path) -> Result<Option<Backend>, SettingsError> {
    Ok(load_default_model_provider_at(home)?.map(ModelProvider::native_harness))
}

pub fn load_default_model_provider_at(home: &Path) -> Result<Option<ModelProvider>, SettingsError> {
    Ok(load_global_launch_preferences_at(home)?.model_provider)
}

pub fn load_global_launch_preferences() -> Result<GlobalLaunchPreferences, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_global_launch_preferences_at(&home)
}

pub fn load_global_launch_preferences_read_only() -> Result<GlobalLaunchPreferences, SettingsError>
{
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    let document = read_settings_or_legacy(&home)?;
    Ok(global_launch_preferences_from_document(&document))
}

/// Read global launch preferences and every configured provider profile from
/// one document snapshot without seeding or writing defaults.
pub fn load_launch_preferences_read_only() -> Result<LaunchPreferencesSnapshot, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_launch_preferences_read_only_at(&home)
}

pub fn load_launch_preferences_read_only_at(
    home: &Path,
) -> Result<LaunchPreferencesSnapshot, SettingsError> {
    let document = read_settings_or_legacy(home)?;
    launch_preferences_from_document(&document)
}

fn launch_preferences_from_document(
    document: &Value,
) -> Result<LaunchPreferencesSnapshot, SettingsError> {
    let mut provider_profiles = Vec::new();
    for &provider in ModelProvider::ALL {
        if let Some(profile) = provider_profile_from_document(document, provider)? {
            provider_profiles.push(profile);
        }
    }
    Ok(LaunchPreferencesSnapshot {
        openrouter_zdr: match document.pointer("/providers/openrouter/zdr") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(enabled)) => Some(*enabled),
            Some(_) => {
                return Err(invalid_provider_profile(
                    ModelProvider::OpenRouter,
                    "zdr must be a boolean",
                ))
            }
        },
        global: global_launch_preferences_from_document(document),
        provider_profiles,
    })
}

fn provider_profile_from_document(
    document: &Value,
    provider: ModelProvider,
) -> Result<Option<ProviderProfile>, SettingsError> {
    let Some(value) = document
        .get("providers")
        .and_then(|providers| providers.get(provider.as_str()))
    else {
        return Ok(None);
    };
    let Some(profile) = value.as_object() else {
        return Err(invalid_provider_profile(provider, "expected an object"));
    };
    let read_string = |key: &str| -> Result<Option<&str>, SettingsError> {
        match profile.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.as_str())),
            Some(_) => Err(invalid_provider_profile(
                provider,
                format!("{key} must be a string"),
            )),
        }
    };

    let model = read_string("model")?
        .map(|value| {
            // A canonical ID retired by a provider rename (#1192) still loads,
            // normalized to its successor; wire IDs and shorthand stay rejected.
            let entry = provider_catalog::model_by_cli_id(value)
                .or_else(|| provider_catalog::model_by_retired_cli_id(value));
            let entry = match entry {
                Some(entry) => entry,
                // #1304: OpenRouter is a gateway whose inventory changes under
                // us (DD-054), so `--model <wire-id>` is remembered verbatim.
                // The final `resolve` below still vets it like a CLI value.
                None if provider == ModelProvider::OpenRouter
                    && provider_catalog::inferred_provider_from_wire(value)
                        .is_none_or(|owner| owner == ModelProvider::OpenRouter) =>
                {
                    return Ok(value.to_string());
                }
                None => {
                    return Err(invalid_provider_profile(
                        provider,
                        format!("model '{value}' is not a canonical catalog ID"),
                    ))
                }
            };
            if entry.provider != provider {
                return Err(invalid_provider_profile(
                    provider,
                    format!("model '{value}' belongs to provider '{}'", entry.provider),
                ));
            }
            Ok(entry.cli_id.to_string())
        })
        .transpose()?;
    let harness = read_string("harness")?
        .map(|value| {
            HarnessSelection::from_settings_str(value).ok_or_else(|| {
                invalid_provider_profile(
                    provider,
                    format!(
                        "harness '{value}' must be default, claude, codex, deepseek, or opencode"
                    ),
                )
            })
        })
        .transpose()?;
    let effort = read_string("effort")?
        .map(|value| {
            EffortLevel::parse(value).ok_or_else(|| {
                invalid_provider_profile(provider, format!("unknown effort '{value}'"))
            })
        })
        .transpose()?;
    let context_window = read_string("context_window")?
        .map(|value| match value {
            "auto" | "1m" => Ok(value.to_string()),
            _ => Err(invalid_provider_profile(
                provider,
                format!("context_window '{value}' must be auto or 1m"),
            )),
        })
        .transpose()?;

    provider_catalog::resolve(
        Some(provider),
        model.as_deref(),
        effort.map(EffortLevel::as_str),
        context_window.as_deref(),
    )
    .map_err(|error| invalid_provider_profile(provider, error.to_string()))?;
    if harness == Some(HarnessSelection::Codex) && provider != ModelProvider::Codex {
        return Err(invalid_provider_profile(
            provider,
            "the Codex harness cannot run this provider",
        ));
    }
    Ok(Some(ProviderProfile {
        provider,
        model,
        harness,
        effort,
        context_window,
    }))
}

fn invalid_provider_profile(provider: ModelProvider, error: impl Into<String>) -> SettingsError {
    SettingsError::InvalidProviderProfile {
        provider,
        error: error.into(),
    }
}

pub fn load_global_launch_preferences_at(
    home: &Path,
) -> Result<GlobalLaunchPreferences, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(global_launch_preferences_from_document(&document))
}

fn global_launch_preferences_from_document(document: &Value) -> GlobalLaunchPreferences {
    let model_provider = document
        .get("backend")
        .and_then(|item| item.get("default"))
        .and_then(Value::as_str)
        .and_then(ModelProvider::from_settings_str)
        .or_else(|| {
            infer_default_backend_from_launch_setup(document).map(Backend::as_model_provider)
        });
    let harness = document
        .get("harness")
        .and_then(|item| item.get("default"))
        .and_then(Value::as_str)
        .and_then(HarnessSelection::from_settings_str);
    GlobalLaunchPreferences {
        model_provider,
        harness,
    }
}

pub fn save_default_backend(backend: Backend) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_default_backend_at(&home, backend)
}

pub fn save_default_backend_at(home: &Path, backend: Backend) -> Result<(), SettingsError> {
    save_settings_patch_at(
        home,
        GlobalSettingsPatch {
            model_provider: Some(backend.as_model_provider()),
            ..GlobalSettingsPatch::default()
        },
    )
}

pub fn save_global_launch_setup_selection(
    backend: Backend,
    scope: LaunchSetupScope,
) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_global_launch_setup_selection_at(&home, backend, scope)
}

pub fn save_global_launch_setup_selection_at(
    home: &Path,
    backend: Backend,
    scope: LaunchSetupScope,
) -> Result<(), SettingsError> {
    save_settings_transaction_at(
        home,
        GlobalSettingsPatch {
            model_provider: Some(backend.as_model_provider()),
            ..GlobalSettingsPatch::default()
        },
        Some(scope),
    )
}

pub fn save_global_launch_preferences(
    provider: Option<ModelProvider>,
    harness: Option<HarnessSelection>,
    scope: LaunchSetupScope,
) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_global_launch_preferences_at(&home, provider, harness, scope)
}

pub fn save_global_launch_preferences_at(
    home: &Path,
    provider: Option<ModelProvider>,
    harness: Option<HarnessSelection>,
    scope: LaunchSetupScope,
) -> Result<(), SettingsError> {
    save_settings_transaction_at(
        home,
        GlobalSettingsPatch {
            model_provider: provider,
            harness,
            ..GlobalSettingsPatch::default()
        },
        Some(scope),
    )
}

pub fn save_settings_patch(patch: GlobalSettingsPatch) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_settings_patch_at(&home, patch)
}

pub fn save_settings_patch_at(
    home: &Path,
    patch: GlobalSettingsPatch,
) -> Result<(), SettingsError> {
    save_settings_transaction_at(home, patch, None)
}

fn save_settings_transaction_at(
    home: &Path,
    patch: GlobalSettingsPatch,
    setup_scope: Option<LaunchSetupScope>,
) -> Result<(), SettingsError> {
    let clud_dir = home.join(CLUD_DIR_NAME);
    fs::create_dir_all(&clud_dir)?;
    let lock_path = clud_dir.join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let path = settings_path_at(home);
    let mut document = read_settings_or_legacy(home)?;
    seed_global_settings_defaults(&mut document);

    if let Some(provider) = patch.model_provider {
        set_default_model_provider(&mut document, provider);
    }
    if let Some(harness) = patch.harness {
        set_default_harness(&mut document, harness);
    }
    if let Some(enabled) = patch.pr_wait_fail_fast {
        object_entry(&mut document, "git")
            .insert("pr_wait_fail_fast".to_string(), Value::Bool(enabled));
    }
    if let Some(enabled) = patch.gh_read_broker {
        object_entry(&mut document, "git")
            .insert("gh_read_broker".to_string(), Value::Bool(enabled));
    }
    if let Some(enabled) = patch.web_term {
        object_entry(&mut document, "web_term").insert("enabled".to_string(), Value::Bool(enabled));
    }
    if let Some(enabled) = patch.openrouter_zdr {
        let profile = object_entry(&mut document, "providers")
            .entry("openrouter")
            .or_insert_with(|| json!({}));
        profile
            .as_object_mut()
            .ok_or_else(|| {
                invalid_provider_profile(ModelProvider::OpenRouter, "expected an object")
            })?
            .insert("zdr".to_string(), Value::Bool(enabled));
    }
    if let Some(enabled) = patch.gc_delete_remote_branches {
        object_entry(&mut document, "gc")
            .insert("delete_remote_branches".to_string(), Value::Bool(enabled));
    }
    if let Some(block_cd) = patch.block_cd {
        object_entry(&mut document, "bash").insert("block_cd".to_string(), block_cd.as_json());
    }
    for profile in patch.provider_profiles {
        let Some(provider) = profile.provider else {
            continue;
        };
        set_provider_profile_patch(&mut document, provider, profile);
    }

    // Validate the complete post-patch document before the atomic write. This
    // catches cross-field capability errors such as Flash + 1m.
    let _ = launch_preferences_from_document(&document)?;

    let launch = global_launch_preferences_from_document(&document);
    let target = resolve_launch_target(
        false,
        false,
        false,
        None,
        launch.model_provider,
        launch.harness,
    )
    .map_err(SettingsError::InvalidLaunchTarget)?;
    if let Some(scope) = setup_scope {
        set_launch_setup_scope(&mut document, target.effective_harness, scope);
    }
    write_settings(&path, &document)
}

pub fn load_auto_fix_hooks_enabled() -> Result<bool, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_auto_fix_hooks_enabled_at(&home)
}

pub fn load_auto_fix_hooks_enabled_at(home: &Path) -> Result<bool, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(document
        .get("hook_health")
        .and_then(|item| item.get("auto_fix_hooks"))
        .and_then(Value::as_bool)
        .unwrap_or(true))
}

pub fn save_auto_fix_hooks_enabled(enabled: bool) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_auto_fix_hooks_enabled_at(&home, enabled)
}

pub fn save_auto_fix_hooks_enabled_at(home: &Path, enabled: bool) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        object_entry(document, "hook_health")
            .insert("auto_fix_hooks".to_string(), Value::Bool(enabled));
    })
}

/// PR-wait fail-fast git command improvements (`clud settings` toggle).
/// On by default (DD-065) — see `GIT_PR_WAIT_FAIL_FAST_NOTE`.
pub fn load_pr_wait_fail_fast_enabled() -> Result<bool, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_pr_wait_fail_fast_enabled_at(&home)
}

pub fn load_pr_wait_fail_fast_enabled_at(home: &Path) -> Result<bool, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(document
        .get("git")
        .and_then(|item| item.get("pr_wait_fail_fast"))
        .and_then(Value::as_bool)
        .unwrap_or(true))
}

pub fn save_pr_wait_fail_fast_enabled(enabled: bool) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_pr_wait_fail_fast_enabled_at(&home, enabled)
}

pub fn save_pr_wait_fail_fast_enabled_at(home: &Path, enabled: bool) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        let git = object_entry(document, "git");
        git.entry("pr_wait_fail_fast_note".to_string())
            .or_insert_with(|| Value::String(GIT_PR_WAIT_FAIL_FAST_NOTE.to_string()));
        git.insert("pr_wait_fail_fast".to_string(), Value::Bool(enabled));
    })
}

/// `git.gh_read_broker` (#1743): route session `gh api` GETs through the
/// daemon read broker. On by default — see `GIT_GH_READ_BROKER_NOTE`.
pub fn load_gh_read_broker_enabled() -> Result<bool, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_gh_read_broker_enabled_at(&home)
}

pub fn load_gh_read_broker_enabled_at(home: &Path) -> Result<bool, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(document
        .get("git")
        .and_then(|item| item.get("gh_read_broker"))
        .and_then(Value::as_bool)
        .unwrap_or(true))
}

/// `git.gh_read_broker_reserve_pct` (#1743): the share of the GitHub REST
/// rate limit (percent) the read broker keeps for interactive reads. Below
/// it, background refreshes are deferred until the window resets. `None`
/// when unset; the broker then uses 10.
pub fn load_gh_read_broker_reserve_pct() -> Result<Option<u64>, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_gh_read_broker_reserve_pct_at(&home)
}

pub fn load_gh_read_broker_reserve_pct_at(home: &Path) -> Result<Option<u64>, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(document
        .get("git")
        .and_then(|item| item.get("gh_read_broker_reserve_pct"))
        .and_then(Value::as_u64))
}

pub fn save_gh_read_broker_enabled_at(home: &Path, enabled: bool) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        let git = object_entry(document, "git");
        git.entry("gh_read_broker_note".to_string())
            .or_insert_with(|| Value::String(GIT_GH_READ_BROKER_NOTE.to_string()));
        git.insert("gh_read_broker".to_string(), Value::Bool(enabled));
    })
}

/// `gc.delete_remote_branches` (#1603): whether reclaiming a squash-merged
/// repo worktree also deletes its branch on `origin`. Off by default; the
/// daemon reads it each tick, and `CLUD_GC_DELETE_REMOTE_BRANCHES` overrides
/// it. See docs/architecture/gc-and-registry.md.
pub fn load_gc_delete_remote_branches() -> Result<bool, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_gc_delete_remote_branches_at(&home)
}

pub fn load_gc_delete_remote_branches_at(home: &Path) -> Result<bool, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(gc_delete_remote_branches_from(&document))
}

/// Pure reader: only a literal JSON `true` opts in.
pub fn gc_delete_remote_branches_from(document: &Value) -> bool {
    document
        .get("gc")
        .and_then(|item| item.get("delete_remote_branches"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `worktrees.warn_bytes` (#1485): when `~/.clud/worktrees` grows past this,
/// `clud gc list` warns. Warn-only: size never deletes pinned work. `0`
/// disables; absent or not a non-negative integer means the default.
pub fn load_worktrees_warn_bytes() -> Result<u64, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_worktrees_warn_bytes_at(&home)
}

pub fn load_worktrees_warn_bytes_at(home: &Path) -> Result<u64, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(worktrees_warn_bytes_from(&document))
}

/// Lock-free, failure-silent read of `worktrees.warn_bytes` for the launch
/// banner (#1610): a missing or unreadable settings file means the default.
/// No advisory lock, so a contended settings lock can never stall a launch.
pub fn peek_worktrees_warn_bytes_at(home: &Path) -> u64 {
    read_settings_or_legacy(home)
        .map(|document| worktrees_warn_bytes_from(&document))
        .unwrap_or(crate::gc::worktree_root::DEFAULT_WARN_BYTES)
}

/// Pure reader for `worktrees.warn_bytes`.
pub fn worktrees_warn_bytes_from(document: &Value) -> u64 {
    document
        .get("worktrees")
        .and_then(|item| item.get("warn_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(crate::gc::worktree_root::DEFAULT_WARN_BYTES)
}

/// `tmp.warn_bytes` (#1327): when `~/.clud/tmp` grows past this, `clud gc
/// list` and the launch banner warn. Warn-only: the 72 h age sweep stays the
/// only deleter of session temp. `0` disables; absent or not a non-negative
/// integer means the default.
pub fn load_tmp_warn_bytes() -> Result<u64, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_tmp_warn_bytes_at(&home)
}

pub fn load_tmp_warn_bytes_at(home: &Path) -> Result<u64, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(tmp_warn_bytes_from(&document))
}

/// Lock-free, failure-silent read of `tmp.warn_bytes` for the launch banner.
pub fn peek_tmp_warn_bytes_at(home: &Path) -> u64 {
    read_settings_or_legacy(home)
        .map(|document| tmp_warn_bytes_from(&document))
        .unwrap_or(crate::gc::worktree_size_cache::DEFAULT_TMP_WARN_BYTES)
}

/// Pure reader for `tmp.warn_bytes`.
pub fn tmp_warn_bytes_from(document: &Value) -> u64 {
    document
        .get("tmp")
        .and_then(|item| item.get("warn_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(crate::gc::worktree_size_cache::DEFAULT_TMP_WARN_BYTES)
}

/// `tmp.max_bytes` (#2397, DD-194): when `~/.clud/tmp` holds more than this,
/// the oldest idle session directories are evicted (never one written to
/// within `MIN_IDLE`). `0` disables; absent or not a non-negative integer
/// means the default.
pub fn load_tmp_max_bytes() -> Result<u64, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_tmp_max_bytes_at(&home)
}

pub fn load_tmp_max_bytes_at(home: &Path) -> Result<u64, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(tmp_max_bytes_from(&document))
}

/// Pure reader for `tmp.max_bytes`.
pub fn tmp_max_bytes_from(document: &Value) -> u64 {
    document
        .get("tmp")
        .and_then(|item| item.get("max_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(crate::gc::session_tmp_cap::DEFAULT_TMP_MAX_BYTES)
}

/// `trash.max_bytes` (#1672): when `~/.clud/trash` holds more than this, the
/// daemon evicts the oldest entries (never one younger than an hour). `0`
/// disables; absent or not a non-negative integer means the default.
pub fn load_trash_max_bytes() -> Result<u64, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_trash_max_bytes_at(&home)
}

pub fn load_trash_max_bytes_at(home: &Path) -> Result<u64, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(trash_max_bytes_from(&document))
}

/// Pure reader for `trash.max_bytes`.
pub fn trash_max_bytes_from(document: &Value) -> u64 {
    document
        .get("trash")
        .and_then(|item| item.get("max_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(crate::rm_tool::TRASH_MAX_BYTES_DEFAULT)
}

/// `cache.warn_bytes` (#1691): when `~/.clud/cache` (almost all uv's own
/// cache) grows past this, `clud gc list` and the launch banner warn.
/// Warn-only; the size cap is `cache.max_bytes` (DD-144, DD-145). `0`
/// disables; absent or not a non-negative integer means the default.
pub fn load_cache_warn_bytes() -> Result<u64, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_cache_warn_bytes_at(&home)
}

pub fn load_cache_warn_bytes_at(home: &Path) -> Result<u64, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(cache_warn_bytes_from(&document))
}

/// Lock-free, failure-silent read of `cache.warn_bytes` for the banner.
pub fn peek_cache_warn_bytes_at(home: &Path) -> u64 {
    read_settings_or_legacy(home)
        .map(|document| cache_warn_bytes_from(&document))
        .unwrap_or(crate::gc::worktree_size_cache::DEFAULT_CACHE_WARN_BYTES)
}

/// Pure reader for `cache.warn_bytes`.
pub fn cache_warn_bytes_from(document: &Value) -> u64 {
    document
        .get("cache")
        .and_then(|item| item.get("warn_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(crate::gc::worktree_size_cache::DEFAULT_CACHE_WARN_BYTES)
}

/// `cache.max_bytes` (#1691, DD-145): when clud's uv cache grows past this,
/// the daily uv sweep asks uv to empty it (`uv cache clean`) if no uv
/// process is running. `0` disables; absent or malformed means the default.
pub fn peek_cache_max_bytes_at(home: &Path) -> u64 {
    read_settings_or_legacy(home)
        .map(|document| cache_max_bytes_from(&document))
        .unwrap_or(crate::gc::uv_cache::DEFAULT_MAX_BYTES)
}

/// Pure reader for `cache.max_bytes`.
pub fn cache_max_bytes_from(document: &Value) -> u64 {
    document
        .get("cache")
        .and_then(|item| item.get("max_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(crate::gc::uv_cache::DEFAULT_MAX_BYTES)
}

/// The JSON key path of the human-set safe-rm root override (#1668,
/// DD-137): `safe_rm.extra_roots`, a list of `{"path", "reason"}` objects.
pub const SAFE_RM_EXTRA_ROOTS_KEY: &str = "safe_rm.extra_roots";

/// One `safe_rm.extra_roots` entry as written, before any path checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeRmExtraRootEntry {
    pub path: String,
    pub reason: String,
}

/// Lock-free, user-level-only read of `safe_rm.extra_roots` (#1668). Only
/// `~/.clud/settings.json` is read, never a repo's `.clud/settings.json`: an
/// agent can edit files in its checkout, so a repo layer would let it widen
/// its own deletion roots (DD-137). Returns the well-formed entries and one
/// message per rejected entry; an unreadable file is one message, no entries.
pub fn peek_safe_rm_extra_roots_at(home: &Path) -> (Vec<SafeRmExtraRootEntry>, Vec<String>) {
    match read_settings_or_legacy(home) {
        Ok(document) => safe_rm_extra_roots_from(&document),
        Err(error) => (
            Vec::new(),
            vec![format!(
                "{SAFE_RM_EXTRA_ROOTS_KEY} ignored: cannot read {}: {error}",
                settings_path_at(home).display()
            )],
        ),
    }
}

/// Pure reader for `safe_rm.extra_roots`. An entry without a non-empty
/// string `path` and a non-empty string `reason` is rejected (not honored)
/// with a message naming its index and what is missing.
pub fn safe_rm_extra_roots_from(document: &Value) -> (Vec<SafeRmExtraRootEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut rejected = Vec::new();
    let Some(value) = document
        .get("safe_rm")
        .and_then(|item| item.get("extra_roots"))
    else {
        return (entries, rejected);
    };
    let Some(list) = value.as_array() else {
        rejected.push(format!(
            "{SAFE_RM_EXTRA_ROOTS_KEY} ignored: must be a list of {{\"path\", \"reason\"}} objects"
        ));
        return (entries, rejected);
    };
    for (index, item) in list.iter().enumerate() {
        let text = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let shown = text("path").unwrap_or_else(|| "?".into());
        match (text("path"), text("reason")) {
            (Some(path), Some(reason)) => entries.push(SafeRmExtraRootEntry { path, reason }),
            (None, _) => rejected.push(format!(
                "{SAFE_RM_EXTRA_ROOTS_KEY}[{index}] ignored: missing a non-empty \"path\""
            )),
            (Some(_), None) => rejected.push(format!(
                "{SAFE_RM_EXTRA_ROOTS_KEY}[{index}] ({shown}) ignored: missing a non-empty \
                 \"reason\"; every override must say why it exists"
            )),
        }
    }
    (entries, rejected)
}

/// `bash.block_cd` as set at the user level (`~/.clud/settings.json`).
///
/// The same file `repo_clud_config` reads as its user layer, so this is a
/// writer for a key the hook already resolves — deliberately not a second
/// source of truth. A repo's own `.clud/settings.json` still overrides it.
pub fn load_block_cd() -> Result<crate::repo_clud_config::BlockCd, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_block_cd_at(&home)
}

pub fn load_block_cd_at(home: &Path) -> Result<crate::repo_clud_config::BlockCd, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(document
        .get("bash")
        .and_then(|item| item.get("block_cd"))
        .and_then(crate::repo_clud_config::BlockCd::from_json)
        .unwrap_or_default())
}

/// Whether bare interactive launches should open in clud's owned web terminal.
/// This preference is intentionally global: a project-local setting could make
/// a normal invocation unexpectedly require a desktop companion binary.
pub fn load_web_term_enabled() -> Result<bool, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_web_term_enabled_at(&home)
}

pub fn load_web_term_enabled_at(home: &Path) -> Result<bool, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(document
        .get("web_term")
        .and_then(|item| item.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

pub fn save_web_term_enabled(enabled: bool) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_web_term_enabled_at(&home, enabled)
}

pub fn save_web_term_enabled_at(home: &Path, enabled: bool) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        object_entry(document, "web_term").insert("enabled".to_string(), Value::Bool(enabled));
    })
}

pub fn load_launch_setup_scope(
    backend: Backend,
) -> Result<Option<LaunchSetupScope>, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_launch_setup_scope_at(&home, backend)
}

pub fn load_launch_setup_scope_at(
    home: &Path,
    backend: Backend,
) -> Result<Option<LaunchSetupScope>, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    let Some(scope) = document
        .get("launch_setup")
        .and_then(|item| item.get(backend_settings_key(backend)))
        .and_then(|item| item.get("scope"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    Ok(LaunchSetupScope::from_settings_str(scope))
}

pub fn save_launch_setup_scope(
    backend: Backend,
    scope: LaunchSetupScope,
) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_launch_setup_scope_at(&home, backend, scope)
}

pub fn save_launch_setup_scope_at(
    home: &Path,
    backend: Backend,
    scope: LaunchSetupScope,
) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        set_launch_setup_scope(document, backend, scope);
    })
}

pub fn load_shell_disable_powershell_for_backend(backend: Backend) -> Result<bool, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_shell_disable_powershell_for_backend_at(&home, backend)
}

pub fn load_shell_disable_powershell_for_backend_at(
    home: &Path,
    backend: Backend,
) -> Result<bool, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    Ok(resolve_shell_disable_powershell(&document, backend))
}

pub fn save_shell_disable_powershell(enabled: bool) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_shell_disable_powershell_at(&home, enabled)
}

pub fn save_shell_disable_powershell_at(home: &Path, enabled: bool) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        let shell = object_entry(document, "shell");
        shell
            .entry("disable_powershell_note".to_string())
            .or_insert_with(|| Value::String(SHELL_DISABLE_POWERSHELL_NOTE.to_string()));
        shell.insert("disable_powershell".to_string(), Value::Bool(enabled));
    })
}

pub fn save_shell_disable_powershell_for_backend(
    backend: Backend,
    enabled: Option<bool>,
) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_shell_disable_powershell_for_backend_at(&home, backend, enabled)
}

pub fn save_shell_disable_powershell_for_backend_at(
    home: &Path,
    backend: Backend,
    enabled: Option<bool>,
) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        let shell = object_entry(document, "shell");
        shell
            .entry("disable_powershell_note".to_string())
            .or_insert_with(|| Value::String(SHELL_DISABLE_POWERSHELL_NOTE.to_string()));
        let backend_entry = shell
            .entry(backend_settings_key(backend).to_string())
            .or_insert_with(|| json!({}));
        if !backend_entry.is_object() {
            *backend_entry = json!({});
        }
        let backend_obj = backend_entry.as_object_mut().unwrap();
        match enabled {
            Some(value) => {
                backend_obj.insert("disable_powershell".to_string(), Value::Bool(value));
            }
            None => {
                backend_obj.insert("disable_powershell".to_string(), Value::Null);
            }
        }
    })
}

fn resolve_shell_disable_powershell(document: &Value, backend: Backend) -> bool {
    let shell = document.get("shell");
    if let Some(per_backend) = shell
        .and_then(|item| item.get(backend_settings_key(backend)))
        .and_then(|item| item.get("disable_powershell"))
        .and_then(Value::as_bool)
    {
        return per_backend;
    }
    shell
        .and_then(|item| item.get("disable_powershell"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub fn save_rust_optimize_settings(settings: &RustOptimizeSettings) -> Result<(), SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    save_rust_optimize_settings_at(&home, settings)
}

pub fn save_rust_optimize_settings_at(
    home: &Path,
    settings: &RustOptimizeSettings,
) -> Result<(), SettingsError> {
    with_settings_document(home, |document| {
        let optimize = object_entry(document, "optimize");
        // No `soldr_version`: soldr cannot be pinned (it always follows latest).
        let rust = json!({
            "use_soldr_shims": settings.use_soldr_shims,
            "install_soldr": settings.install_soldr,
        });
        optimize.insert("rust".to_string(), rust);
    })
}

pub fn load_rust_optimize_settings_at(
    home: &Path,
) -> Result<Option<RustOptimizeSettings>, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    rust_optimize_from_json(&document)
}

/// Settings for the foreground CPU-burn banner (#466). Defaults: enabled,
/// 30 s heartbeat. JSON shape:
///
/// ```json
/// { "foreground": { "cpu_banner": { "enabled": false, "heartbeat_secs": 60 } } }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuBannerSettings {
    pub enabled: bool,
    pub heartbeat_secs: u64,
}

impl Default for CpuBannerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            heartbeat_secs: 30,
        }
    }
}

pub fn load_cpu_banner_settings() -> Result<CpuBannerSettings, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_cpu_banner_settings_at(&home)
}

pub fn load_cpu_banner_settings_at(home: &Path) -> Result<CpuBannerSettings, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    let mut out = CpuBannerSettings::default();
    let Some(section) = document
        .get("foreground")
        .and_then(|item| item.get("cpu_banner"))
    else {
        return Ok(out);
    };
    if let Some(enabled) = section.get("enabled").and_then(Value::as_bool) {
        out.enabled = enabled;
    }
    if let Some(secs) = section.get("heartbeat_secs").and_then(Value::as_u64) {
        out.heartbeat_secs = secs;
    }
    Ok(out)
}

/// `[foreground.toasts]` (#1189): in-terminal toasts and the Claude
/// status-line surface.
///
/// ```json
/// { "foreground": { "toasts": { "enabled": true, "claude_statusline": true } } }
/// ```
///
/// `enabled = false` restores silent sessions (no compositor, no status-line
/// injection). `claude_statusline = false` keeps in-terminal toasts but stops
/// clud from composing a `statusLine` into Claude launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastSettings {
    pub enabled: bool,
    pub claude_statusline: bool,
}

impl Default for ToastSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            claude_statusline: true,
        }
    }
}

pub fn load_toast_settings() -> Result<ToastSettings, SettingsError> {
    let home = crate::home::user_home().ok_or(SettingsError::NoHomeDir)?;
    load_toast_settings_at(&home)
}

pub fn load_toast_settings_at(home: &Path) -> Result<ToastSettings, SettingsError> {
    let lock_path = home.join(CLUD_DIR_NAME).join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let document = read_settings_or_legacy(home)?;
    let mut out = ToastSettings::default();
    let Some(section) = document
        .get("foreground")
        .and_then(|item| item.get("toasts"))
    else {
        return Ok(out);
    };
    if let Some(enabled) = section.get("enabled").and_then(Value::as_bool) {
        out.enabled = enabled;
    }
    if let Some(statusline) = section.get("claude_statusline").and_then(Value::as_bool) {
        out.claude_statusline = statusline;
    }
    Ok(out)
}

fn with_settings_document<F>(home: &Path, mutate: F) -> Result<(), SettingsError>
where
    F: FnOnce(&mut Value),
{
    let clud_dir = home.join(CLUD_DIR_NAME);
    fs::create_dir_all(&clud_dir)?;
    let lock_path = clud_dir.join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let path = settings_path_at(home);
    let mut document = read_settings_or_legacy(home)?;
    seed_global_settings_defaults(&mut document);
    mutate(&mut document);
    write_settings(&path, &document)
}

/// Read the optional launch landing preference from the user's settings.
pub fn load_land_bias_at(home: &Path) -> Result<Option<String>, SettingsError> {
    let document = read_settings_or_legacy(home)?;
    Ok(match document.get("launch") {
        None => None,
        Some(section) if !section.is_object() => Some(String::new()),
        Some(section) => section
            .get("land_bias")
            .map(|value| value.as_str().unwrap_or("").to_string()),
    })
}

fn read_settings_or_legacy(home: &Path) -> Result<Value, SettingsError> {
    let path = settings_path_at(home);
    match fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => return Ok(json!({})),
        Ok(text) => return parse_json_settings(&path, &text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(SettingsError::Io(error)),
    }

    let legacy_path = legacy_settings_path_at(home);
    match fs::read_to_string(&legacy_path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => parse_legacy_toml_settings(&legacy_path, &text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(SettingsError::Io(error)),
    }
}

fn parse_json_settings(path: &Path, text: &str) -> Result<Value, SettingsError> {
    let value: Value = serde_json::from_str(text).map_err(|error| SettingsError::Parse {
        path: path.to_path_buf(),
        error: error.to_string(),
    })?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(SettingsError::Parse {
            path: path.to_path_buf(),
            error: "root must be a JSON object".to_string(),
        })
    }
}

fn parse_legacy_toml_settings(path: &Path, text: &str) -> Result<Value, SettingsError> {
    let document = text
        .parse::<DocumentMut>()
        .map_err(|error| SettingsError::Parse {
            path: path.to_path_buf(),
            error: error.to_string(),
        })?;
    let mut root = json!({});

    if let Some(enabled) = document
        .get("hook_health")
        .and_then(|item| item.get("auto_fix_hooks"))
        .and_then(Item::as_bool)
    {
        object_entry(&mut root, "hook_health")
            .insert("auto_fix_hooks".to_string(), Value::Bool(enabled));
    }

    if let Some(default_backend) = document
        .get("backend")
        .and_then(|item| item.get("default"))
        .and_then(Item::as_str)
        .and_then(Backend::from_settings_str)
    {
        object_entry(&mut root, "backend").insert(
            "default".to_string(),
            Value::String(default_backend.executable_name().to_string()),
        );
    }

    if let Some(launch_setup) = document.get("launch_setup").and_then(Item::as_table) {
        for (backend, item) in launch_setup.iter() {
            if let Some(scope) = item
                .get("scope")
                .and_then(Item::as_str)
                .and_then(LaunchSetupScope::from_settings_str)
            {
                object_entry(&mut root, "launch_setup").insert(
                    backend.to_string(),
                    json!({ "scope": scope.as_str().to_string() }),
                );
            }
        }
    }

    if let Some(rust) = document
        .get("optimize")
        .and_then(|item| item.get("rust"))
        .and_then(Item::as_table)
    {
        let defaults = RustOptimizeSettings::default();
        object_entry(&mut root, "optimize").insert(
            "rust".to_string(),
            json!({
                "use_soldr_shims": rust
                    .get("use_soldr_shims")
                    .and_then(Item::as_bool)
                    .unwrap_or(defaults.use_soldr_shims),
                "install_soldr": rust
                    .get("install_soldr")
                    .and_then(Item::as_bool)
                    .unwrap_or(defaults.install_soldr),
                "soldr_version": rust
                    .get("soldr_version")
                    .and_then(Item::as_str)
                    .unwrap_or(&defaults.soldr_version),
            }),
        );
    }

    if let Some(shell) = document.get("shell").and_then(Item::as_table) {
        if let Some(enabled) = shell.get("disable_powershell").and_then(Item::as_bool) {
            object_entry(&mut root, "shell")
                .insert("disable_powershell".to_string(), Value::Bool(enabled));
        }
        for backend_key in ["claude", "codex"] {
            let Some(per_backend) = shell.get(backend_key).and_then(Item::as_table) else {
                continue;
            };
            let Some(value) = per_backend
                .get("disable_powershell")
                .and_then(Item::as_bool)
            else {
                continue;
            };
            object_entry(&mut root, "shell").insert(
                backend_key.to_string(),
                json!({ "disable_powershell": value }),
            );
        }
    }

    Ok(root)
}

fn rust_optimize_from_json(
    document: &Value,
) -> Result<Option<RustOptimizeSettings>, SettingsError> {
    let Some(table) = document
        .get("optimize")
        .and_then(|item| item.get("rust"))
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    let defaults = RustOptimizeSettings::default();
    Ok(Some(RustOptimizeSettings {
        use_soldr_shims: table
            .get("use_soldr_shims")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_soldr_shims),
        install_soldr: table
            .get("install_soldr")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.install_soldr),
        soldr_version: table
            .get("soldr_version")
            .and_then(Value::as_str)
            .unwrap_or(&defaults.soldr_version)
            .to_string(),
    }))
}

fn write_settings(path: &Path, document: &Value) -> Result<(), SettingsError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body =
        serde_json::to_string_pretty(document).map_err(|error| SettingsError::Parse {
            path: path.to_path_buf(),
            error: error.to_string(),
        })?;
    body.push('\n');
    crate::fs_private::write_private_atomic(path, body.as_bytes())?;
    Ok(())
}

fn read_codex_config_overrides(
    document: &Value,
    path: &Path,
) -> Result<Option<Vec<String>>, SettingsError> {
    let Some(value) = document
        .get("codex")
        .and_then(|item| item.get("config_overrides"))
    else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(items) = value.as_array() else {
        return Err(SettingsError::Parse {
            path: path.to_path_buf(),
            error: "codex.config_overrides must be an array of strings".to_string(),
        });
    };
    let mut overrides = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(SettingsError::Parse {
                path: path.to_path_buf(),
                error: "codex.config_overrides must be an array of strings".to_string(),
            });
        };
        if !text.trim().is_empty() {
            overrides.push(text.to_string());
        }
    }
    Ok(Some(overrides))
}

fn seed_codex_config_override_defaults(document: &mut Value) {
    let Some(codex) = seed_object_entry(document, "codex") else {
        return;
    };
    codex
        .entry("config_overrides_note".to_string())
        .or_insert_with(|| Value::String(CODEX_CONFIG_OVERRIDES_NOTE.to_string()));
    codex
        .entry("config_overrides".to_string())
        .or_insert_with(|| {
            Value::Array(
                default_codex_config_overrides()
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            )
        });
}

fn merge_settings_into(target: &mut Value, source: &Value) {
    let Some(source_obj) = source.as_object() else {
        return;
    };
    if !target.is_object() {
        *target = json!({});
    }
    let target_obj = target.as_object_mut().unwrap();
    for (key, source_value) in source_obj {
        if source_value.is_null() {
            continue;
        }
        match target_obj.get_mut(key) {
            Some(target_value) if target_value.is_object() && source_value.is_object() => {
                merge_settings_into(target_value, source_value);
            }
            Some(target_value) => {
                *target_value = source_value.clone();
            }
            None if source_value.is_object() => {
                let mut nested = json!({});
                merge_settings_into(&mut nested, source_value);
                target_obj.insert(key.clone(), nested);
            }
            None => {
                target_obj.insert(key.clone(), source_value.clone());
            }
        }
    }
}

#[path = "clud_settings_document.rs"]
mod clud_settings_document;
use clud_settings_document::{
    backend_settings_key, infer_default_backend_from_launch_setup, object_entry, seed_object_entry,
    set_default_harness, set_default_model_provider, set_launch_setup_scope,
    set_provider_profile_patch,
};
fn acquire_lock(path: &Path) -> io::Result<LockGuard> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    FileExt::lock_exclusive(&file)
        .map_err(|error| io::Error::other(format!("lock {}: {error}", path.display())))?;
    Ok(LockGuard { _file: file })
}

struct LockGuard {
    _file: File,
}

#[cfg(test)]
#[path = "clud_settings_tests.rs"]
mod tests;

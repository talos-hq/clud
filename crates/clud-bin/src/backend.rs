use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Model API/provider selected by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "lower")]
pub enum ModelProvider {
    Claude,
    Codex,
    #[serde(rename = "deepseek")]
    DeepSeek,
    Kimi,
    #[serde(rename = "openrouter")]
    OpenRouter,
}

/// Whether one provider owns the launch or the Claude harness routes among
/// several providers through the launch-scoped gateway.
///
/// This is deliberately separate from [`ModelProvider`] and from
/// [`LaunchMode`] (subprocess versus PTY).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    #[default]
    Direct,
    Unified,
}

impl RoutingMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Unified => "unified",
        }
    }
}

impl ModelProvider {
    /// Every provider variant. Registry guardrail tests iterate this so a new
    /// variant added without updating it fails loudly instead of silently
    /// falling through provider-inference/settings lookups.
    pub const ALL: &'static [ModelProvider] = &[
        Self::Claude,
        Self::Codex,
        Self::DeepSeek,
        Self::Kimi,
        Self::OpenRouter,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::DeepSeek => "deepseek",
            Self::Kimi => "kimi",
            Self::OpenRouter => "openrouter",
        }
    }

    pub fn from_settings_str(value: &str) -> Option<Self> {
        let lowered = value.to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|provider| provider.as_str() == lowered)
    }

    pub fn native_harness(self) -> Backend {
        match self {
            Self::Claude | Self::DeepSeek | Self::Kimi | Self::OpenRouter => Backend::Claude,
            Self::Codex => Backend::Codex,
        }
    }

    /// Provider-native wire-ID prefixes, used to infer a provider from a raw
    /// model wire ID that isn't in the catalog. Exhaustive match is
    /// deliberate: adding a provider must extend this or the build breaks.
    pub fn wire_prefixes(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["claude-"],
            Self::Codex => &["gpt-", "codex-"],
            Self::DeepSeek => &["deepseek-"],
            Self::Kimi => &["kimi-"],
            // OpenRouter's `anthropic/*` aliases are not an unambiguous
            // provider identity; require --openrouter or its clud catalog ID.
            Self::OpenRouter => &[],
        }
    }
}

impl std::fmt::Display for ModelProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// User-requested harness preference. `Default` resolves to the provider's
/// native executable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "lower")]
pub enum HarnessSelection {
    #[default]
    Default,
    Claude,
    Codex,
    #[serde(rename = "deepseek")]
    #[value(alias = "dsh")]
    DeepSeek,
    #[serde(rename = "opencode")]
    OpenCode,
}

impl HarnessSelection {
    pub const fn for_backend(backend: Backend) -> Self {
        match backend {
            Backend::Claude => Self::Claude,
            Backend::Codex => Self::Codex,
            Backend::DeepSeek => Self::DeepSeek,
            Backend::OpenCode => Self::OpenCode,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::DeepSeek => "deepseek",
            Self::OpenCode => "opencode",
        }
    }

    pub fn from_settings_str(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "deepseek" | "dsh" => Some(Self::DeepSeek),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }

    pub fn resolve(self, provider: ModelProvider) -> Backend {
        match self {
            Self::Default => provider.native_harness(),
            Self::Claude => Backend::Claude,
            Self::Codex => Backend::Codex,
            Self::DeepSeek => Backend::DeepSeek,
            Self::OpenCode => Backend::OpenCode,
        }
    }
}

impl std::fmt::Display for HarnessSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceSource {
    Cli,
    GlobalSetting,
    ProviderSetting,
    CredentialFallback,
    BuiltInDefault,
}

impl PreferenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::GlobalSetting => "global_setting",
            Self::ProviderSetting => "provider_setting",
            Self::CredentialFallback => "credential_fallback",
            Self::BuiltInDefault => "built_in_default",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedLaunchTarget {
    pub routing_mode: RoutingMode,
    pub model_provider: ModelProvider,
    pub requested_harness: HarnessSelection,
    pub effective_harness: Backend,
    pub provider_source: PreferenceSource,
    pub harness_source: PreferenceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LaunchTargetError {
    #[error("unsupported launch target: unified routing requires the Claude harness")]
    UnifiedViaOpenCodeUnsupported,
    #[error("unsupported launch target: Codex provider cannot use the OpenCode harness")]
    CodexViaOpenCodeUnsupported,
    #[error("unsupported launch target: Claude provider cannot use the Codex harness")]
    ClaudeViaCodexUnsupported,
    #[error("unsupported launch target: DeepSeek provider cannot use the Codex harness")]
    DeepSeekViaCodexUnsupported,
    #[error("unsupported launch target: Kimi provider cannot use the Codex harness")]
    KimiViaCodexUnsupported,
    #[error("unsupported launch target: OpenRouter provider cannot use the Codex harness")]
    OpenRouterViaCodexUnsupported,
    #[error("unsupported launch target: unified routing requires the Claude harness")]
    UnifiedViaCodexUnsupported,
    #[error("unsupported launch target: unified routing requires the Claude harness")]
    UnifiedViaDeepSeekUnsupported,
}

/// A harness's argument surface for launch-scoped configuration (#977).
///
/// clud compiles its settings into launch arguments and never writes a
/// harness's config files (DD-049), so what a harness can receive is bounded
/// by this surface. See `docs/architecture/hook-dispatch.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSurface {
    /// Child-local inline JSON configuration, with no clud hook adapter.
    InlineEnvironment,
    /// `--settings <file-or-json>`: an additional source that merges with the
    /// settings files; hook entries concatenate across levels.
    ClaudeSettingsArg,
    /// Repeated `-c key=value` overrides of `config.toml`. Codex hooks live
    /// in a separate `hooks.json` that no flag can point at, so hooks are not
    /// expressible; the installed PreToolUse line is codex's only coverage.
    CodexConfigOverrides,
    /// Only a named `--profile` switch; no known hook or settings argument.
    ProfileOnly,
}

impl SettingsSurface {
    /// Whether clud's declared hooks can be delivered as launch arguments.
    pub fn accepts_hooks(self) -> bool {
        matches!(self, SettingsSurface::ClaudeSettingsArg)
    }
}

/// Supported backend agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    Claude,
    Codex,
    DeepSeek,
    OpenCode,
}

impl Backend {
    pub const ALL: [Self; 4] = [Self::Claude, Self::Codex, Self::DeepSeek, Self::OpenCode];

    /// How clud hands this harness launch-scoped configuration (#977).
    /// Call sites ask this instead of matching on the harness.
    pub fn settings_surface(&self) -> SettingsSurface {
        match self {
            Backend::Claude => SettingsSurface::ClaudeSettingsArg,
            Backend::Codex => SettingsSurface::CodexConfigOverrides,
            Backend::DeepSeek => SettingsSurface::ProfileOnly,
            Backend::OpenCode => SettingsSurface::InlineEnvironment,
        }
    }

    /// The executable name to search for on PATH.
    pub fn executable_name(&self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Codex => "codex",
            Backend::DeepSeek => "dsh",
            Backend::OpenCode => "opencode",
        }
    }

    pub fn from_settings_str(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("claude") {
            Some(Backend::Claude)
        } else if value.eq_ignore_ascii_case("codex") {
            Some(Backend::Codex)
        } else if value.eq_ignore_ascii_case("deepseek") || value.eq_ignore_ascii_case("dsh") {
            Some(Backend::DeepSeek)
        } else if value.eq_ignore_ascii_case("opencode") {
            Some(Backend::OpenCode)
        } else {
            None
        }
    }

    pub fn as_model_provider(self) -> ModelProvider {
        match self {
            Self::Claude => ModelProvider::Claude,
            Self::Codex => ModelProvider::Codex,
            Self::DeepSeek => ModelProvider::DeepSeek,
            Self::OpenCode => ModelProvider::Claude,
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.executable_name())
    }
}

/// Supported process launch modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Subprocess,
    Pty,
}

impl LaunchMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            LaunchMode::Subprocess => "subprocess",
            LaunchMode::Pty => "pty",
        }
    }
}

impl std::fmt::Display for LaunchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Find the backend executable on PATH.
pub fn find_backend(backend: Backend) -> Option<PathBuf> {
    which::which(backend.executable_name()).ok()
}

/// Resolve which backend to use based on CLI flags.
/// Default is Claude.
pub fn resolve_backend(claude: bool, codex: bool) -> Backend {
    resolve_backend_with_default(claude, codex, None)
}

/// Resolve which backend to use based on CLI flags and a persisted default.
///
/// Explicit CLI backend flags always win. The persisted default only applies
/// to bare `clud` launches.
pub fn resolve_backend_with_default(
    claude: bool,
    codex: bool,
    default_backend: Option<Backend>,
) -> Backend {
    if codex {
        Backend::Codex
    } else if claude {
        Backend::Claude
    } else {
        default_backend.unwrap_or(Backend::Claude)
    }
}

/// Resolve model provider and harness independently.
///
/// Each dimension follows the same precedence: CLI > global setting >
/// built-in default. The concrete harness is validated after resolution so an
/// unsupported route never silently falls back.
pub fn resolve_launch_target(
    claude: bool,
    codex: bool,
    deepseek: bool,
    cli_harness: Option<HarnessSelection>,
    global_provider: Option<ModelProvider>,
    global_harness: Option<HarnessSelection>,
) -> Result<ResolvedLaunchTarget, LaunchTargetError> {
    let cli_provider = if deepseek {
        Some(ModelProvider::DeepSeek)
    } else if codex {
        Some(ModelProvider::Codex)
    } else if claude {
        Some(ModelProvider::Claude)
    } else {
        None
    };
    resolve_launch_target_with_provider(cli_provider, cli_harness, global_provider, global_harness)
}

/// Resolve a launch target from already-normalized CLI provider intent.
///
/// This is the provider-neutral entry point used by `--provider` and by a
/// qualified `--model` that infers its provider. The boolean wrapper above is
/// retained for existing callers and compatibility tests.
pub fn resolve_launch_target_with_provider(
    cli_provider: Option<ModelProvider>,
    cli_harness: Option<HarnessSelection>,
    global_provider: Option<ModelProvider>,
    global_harness: Option<HarnessSelection>,
) -> Result<ResolvedLaunchTarget, LaunchTargetError> {
    resolve_routed_launch_target(
        RoutingMode::Direct,
        cli_provider,
        cli_harness,
        global_provider,
        global_harness,
    )
}

pub fn resolve_routed_launch_target(
    routing_mode: RoutingMode,
    cli_provider: Option<ModelProvider>,
    cli_harness: Option<HarnessSelection>,
    global_provider: Option<ModelProvider>,
    global_harness: Option<HarnessSelection>,
) -> Result<ResolvedLaunchTarget, LaunchTargetError> {
    if routing_mode == RoutingMode::Unified {
        let requested_harness = cli_harness.unwrap_or(HarnessSelection::Default);
        if requested_harness == HarnessSelection::Codex {
            return Err(LaunchTargetError::UnifiedViaCodexUnsupported);
        }
        if requested_harness == HarnessSelection::DeepSeek {
            return Err(LaunchTargetError::UnifiedViaDeepSeekUnsupported);
        }
        if requested_harness == HarnessSelection::OpenCode {
            return Err(LaunchTargetError::UnifiedViaOpenCodeUnsupported);
        }
        return Ok(ResolvedLaunchTarget {
            routing_mode,
            model_provider: cli_provider.unwrap_or(ModelProvider::Claude),
            requested_harness,
            effective_harness: Backend::Claude,
            provider_source: if cli_provider.is_some() {
                PreferenceSource::Cli
            } else {
                PreferenceSource::BuiltInDefault
            },
            harness_source: if cli_harness.is_some() {
                PreferenceSource::Cli
            } else {
                PreferenceSource::BuiltInDefault
            },
        });
    }
    let (model_provider, provider_source) = if let Some(provider) = cli_provider {
        (provider, PreferenceSource::Cli)
    } else if let Some(provider) = global_provider {
        (provider, PreferenceSource::GlobalSetting)
    } else {
        (ModelProvider::Claude, PreferenceSource::BuiltInDefault)
    };

    let (requested_harness, harness_source) = if let Some(harness) = cli_harness {
        (harness, PreferenceSource::Cli)
    } else if let Some(harness) = global_harness {
        (harness, PreferenceSource::GlobalSetting)
    } else {
        (HarnessSelection::Default, PreferenceSource::BuiltInDefault)
    };
    let effective_harness = requested_harness.resolve(model_provider);
    if effective_harness == Backend::OpenCode && model_provider == ModelProvider::Codex {
        return Err(LaunchTargetError::CodexViaOpenCodeUnsupported);
    }
    if effective_harness == Backend::Codex {
        match model_provider {
            ModelProvider::Claude => return Err(LaunchTargetError::ClaudeViaCodexUnsupported),
            ModelProvider::DeepSeek => return Err(LaunchTargetError::DeepSeekViaCodexUnsupported),
            ModelProvider::Kimi => return Err(LaunchTargetError::KimiViaCodexUnsupported),
            ModelProvider::OpenRouter => {
                return Err(LaunchTargetError::OpenRouterViaCodexUnsupported)
            }
            ModelProvider::Codex => {}
        }
    }

    Ok(ResolvedLaunchTarget {
        routing_mode,
        model_provider,
        requested_harness,
        effective_harness,
        provider_source,
        harness_source,
    })
}

/// Validate options whose meaning depends on the resolved model provider.
pub fn validate_provider_options(
    _target: ResolvedLaunchTarget,
    _model: Option<&str>,
) -> Result<(), LaunchTargetError> {
    Ok(())
}

pub fn saved_harness_override_notice(
    target: ResolvedLaunchTarget,
    stderr_is_terminal: bool,
    structured_output: bool,
) -> Option<String> {
    if structured_output
        || !stderr_is_terminal
        || target.harness_source != PreferenceSource::GlobalSetting
        || target.requested_harness == HarnessSelection::Default
    {
        return None;
    }
    let name = match target.effective_harness {
        Backend::Claude => "Claude",
        Backend::Codex => "Codex",
        Backend::DeepSeek => "DeepSeek",
        Backend::OpenCode => "OpenCode",
    };
    Some(format!(
        "\x1b[32m[clud] Harness override: {name} (global setting)\x1b[0m"
    ))
}

/// Resolve how the backend should be launched (#691, [DD-086]).
///
/// One rule for every harness on every platform:
/// - `--pty` / `--subprocess` win.
/// - A console launch (stdin and stdout both TTYs) that is not `headless`
///   (Claude `-p`/`--print`, `codex exec`, DeepSeek's headless profile) runs
///   through the PTY pump. Harness TUIs expect a terminal.
/// - Everything else — headless, piped, or redirected to a log — runs as a
///   subprocess so its output reaches the destination byte-exact. Without a
///   terminal there is nothing for a PTY to drive.
///
/// History: subprocess mode for interactive TUIs came from an old event-loop
/// bug that dropped Codex's startup `\x1b[6n` reply (PR #47, titled `(#46)`;
/// see #737). The raw byte pump fixed it in the same PR.
pub fn resolve_launch_mode(
    pty: bool,
    subprocess: bool,
    headless: bool,
    parent_has_tty: bool,
) -> LaunchMode {
    if pty {
        LaunchMode::Pty
    } else if subprocess || headless || !parent_has_tty {
        LaunchMode::Subprocess
    } else {
        LaunchMode::Pty
    }
}

#[cfg(test)]
mod launch_mode_tests {
    use super::*;

    #[test]
    fn console_launch_uses_pty() {
        assert_eq!(
            resolve_launch_mode(false, false, false, true),
            LaunchMode::Pty
        );
    }

    #[test]
    fn headless_uses_subprocess_even_with_a_terminal() {
        assert_eq!(
            resolve_launch_mode(false, false, true, true),
            LaunchMode::Subprocess
        );
    }

    #[test]
    fn no_terminal_uses_subprocess() {
        assert_eq!(
            resolve_launch_mode(false, false, false, false),
            LaunchMode::Subprocess
        );
    }

    #[test]
    fn explicit_flags_win() {
        assert_eq!(
            resolve_launch_mode(true, false, true, false),
            LaunchMode::Pty
        );
        assert_eq!(
            resolve_launch_mode(false, true, false, true),
            LaunchMode::Subprocess
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_provider_all_has_no_duplicates_and_matches_variant_count() {
        // Fails loudly if a variant is added without updating `ALL` -- the
        // exact silent-fallback failure mode this registry hoist removes.
        let all = ModelProvider::ALL;
        for (index, provider) in all.iter().enumerate() {
            for other in &all[index + 1..] {
                assert_ne!(provider, other, "ModelProvider::ALL has a duplicate entry");
            }
        }
        // Compare against clap's derive-generated variant list rather than a
        // hardcoded count: a hardcoded `3` would silently keep passing if a
        // future variant were added without updating `ALL`, which is exactly
        // the silent-fallback failure mode this registry exists to remove.
        assert_eq!(
            all.len(),
            ModelProvider::value_variants().len(),
            "ModelProvider::ALL must list every enum variant exactly once"
        );
    }

    #[test]
    fn every_model_provider_round_trips_through_settings_str() {
        for provider in ModelProvider::ALL {
            let value = provider.as_str();
            assert_eq!(ModelProvider::from_settings_str(value), Some(*provider));
            // Case-insensitivity must survive the table-driven rewrite.
            assert_eq!(
                ModelProvider::from_settings_str(&value.to_ascii_uppercase()),
                Some(*provider)
            );
        }
        assert_eq!(ModelProvider::from_settings_str("not-a-provider"), None);
    }

    #[test]
    fn wire_prefixes_are_exhaustive_and_match_the_original_prefix_ladder() {
        assert_eq!(ModelProvider::Claude.wire_prefixes(), &["claude-"]);
        assert_eq!(ModelProvider::Codex.wire_prefixes(), &["gpt-", "codex-"]);
        assert_eq!(ModelProvider::DeepSeek.wire_prefixes(), &["deepseek-"]);
        assert_eq!(ModelProvider::Kimi.wire_prefixes(), &["kimi-"]);
    }

    #[test]
    fn test_default_is_claude() {
        assert_eq!(resolve_backend(false, false), Backend::Claude);
    }

    #[test]
    fn test_persisted_default_backend_wins_for_bare_launch() {
        assert_eq!(
            resolve_backend_with_default(false, false, Some(Backend::Codex)),
            Backend::Codex
        );
    }

    #[test]
    fn test_explicit_backend_flags_override_persisted_default() {
        assert_eq!(
            resolve_backend_with_default(true, false, Some(Backend::Codex)),
            Backend::Claude
        );
        assert_eq!(
            resolve_backend_with_default(false, true, Some(Backend::Claude)),
            Backend::Codex
        );
    }

    #[test]
    fn test_claude_flag() {
        assert_eq!(resolve_backend(true, false), Backend::Claude);
    }

    #[test]
    fn test_codex_flag() {
        assert_eq!(resolve_backend(false, true), Backend::Codex);
    }

    #[test]
    fn launch_target_precedence_is_independent_per_dimension() {
        let cli_providers = [
            (false, false, false, None),
            (true, false, false, Some(ModelProvider::Claude)),
            (false, true, false, Some(ModelProvider::Codex)),
            (false, false, true, Some(ModelProvider::DeepSeek)),
        ];
        let global_providers = [
            None,
            Some(ModelProvider::Claude),
            Some(ModelProvider::Codex),
            Some(ModelProvider::DeepSeek),
        ];
        let harnesses = [
            None,
            Some(HarnessSelection::Default),
            Some(HarnessSelection::Claude),
            Some(HarnessSelection::Codex),
            Some(HarnessSelection::DeepSeek),
        ];

        for (claude, codex, deepseek, cli_provider) in cli_providers {
            for global_provider in global_providers {
                for cli_harness in harnesses {
                    for global_harness in harnesses {
                        let provider = cli_provider
                            .or(global_provider)
                            .unwrap_or(ModelProvider::Claude);
                        let provider_source = if cli_provider.is_some() {
                            PreferenceSource::Cli
                        } else if global_provider.is_some() {
                            PreferenceSource::GlobalSetting
                        } else {
                            PreferenceSource::BuiltInDefault
                        };
                        let requested = cli_harness
                            .or(global_harness)
                            .unwrap_or(HarnessSelection::Default);
                        let harness_source = if cli_harness.is_some() {
                            PreferenceSource::Cli
                        } else if global_harness.is_some() {
                            PreferenceSource::GlobalSetting
                        } else {
                            PreferenceSource::BuiltInDefault
                        };
                        let result = resolve_launch_target(
                            claude,
                            codex,
                            deepseek,
                            cli_harness,
                            global_provider,
                            global_harness,
                        );
                        let effective = requested.resolve(provider);

                        if provider == ModelProvider::Claude && effective == Backend::Codex {
                            assert_eq!(
                                result,
                                Err(LaunchTargetError::ClaudeViaCodexUnsupported),
                                "cli_provider={cli_provider:?}, global_provider={global_provider:?}, \
                                 cli_harness={cli_harness:?}, global_harness={global_harness:?}"
                            );
                            continue;
                        }
                        if provider == ModelProvider::DeepSeek && effective == Backend::Codex {
                            assert_eq!(
                                result,
                                Err(LaunchTargetError::DeepSeekViaCodexUnsupported),
                                "cli_provider={cli_provider:?}, global_provider={global_provider:?}, \
                                 cli_harness={cli_harness:?}, global_harness={global_harness:?}"
                            );
                            continue;
                        }

                        let target = result.unwrap();
                        assert_eq!(target.model_provider, provider);
                        assert_eq!(target.requested_harness, requested);
                        assert_eq!(target.effective_harness, effective);
                        assert_eq!(target.provider_source, provider_source);
                        assert_eq!(target.harness_source, harness_source);
                    }
                }
            }
        }
    }

    #[test]
    fn default_harness_maps_to_provider_native_executable() {
        assert_eq!(
            HarnessSelection::Default.resolve(ModelProvider::Claude),
            Backend::Claude
        );
        assert_eq!(
            HarnessSelection::Default.resolve(ModelProvider::Codex),
            Backend::Codex
        );
        assert_eq!(
            HarnessSelection::Default.resolve(ModelProvider::DeepSeek),
            Backend::Claude
        );
        assert_eq!(
            HarnessSelection::Default.resolve(ModelProvider::Kimi),
            Backend::Claude
        );
    }

    #[test]
    fn unsupported_cross_route_is_an_error() {
        let error = resolve_launch_target(
            true,
            false,
            false,
            Some(HarnessSelection::Codex),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error, LaunchTargetError::ClaudeViaCodexUnsupported);
        assert_eq!(
            error.to_string(),
            "unsupported launch target: Claude provider cannot use the Codex harness"
        );
    }

    #[test]
    fn deepseek_rejects_the_codex_harness_but_accepts_a_qualified_model() {
        let target = resolve_launch_target(
            false,
            false,
            true,
            Some(HarnessSelection::Codex),
            None,
            None,
        );
        assert_eq!(target, Err(LaunchTargetError::DeepSeekViaCodexUnsupported));

        let target = resolve_launch_target(false, false, true, None, None, None).unwrap();
        assert_eq!(target.effective_harness, Backend::Claude);
        assert_eq!(
            validate_provider_options(target, Some("deepseek-v4-pro")),
            Ok(())
        );
        assert_eq!(validate_provider_options(target, None), Ok(()));
    }

    #[test]
    fn kimi_rejects_the_codex_harness_but_accepts_the_claude_harness() {
        let target = resolve_launch_target_with_provider(
            Some(ModelProvider::Kimi),
            Some(HarnessSelection::Codex),
            None,
            None,
        );
        assert_eq!(target, Err(LaunchTargetError::KimiViaCodexUnsupported));
        assert_eq!(
            target.unwrap_err().to_string(),
            "unsupported launch target: Kimi provider cannot use the Codex harness"
        );

        let target =
            resolve_launch_target_with_provider(Some(ModelProvider::Kimi), None, None, None)
                .unwrap();
        assert_eq!(target.effective_harness, Backend::Claude);
        assert_eq!(validate_provider_options(target, Some("kimi-k3")), Ok(()));
    }

    #[test]
    fn normalized_cli_provider_keeps_cli_source_metadata() {
        let target = resolve_launch_target_with_provider(
            Some(ModelProvider::Codex),
            None,
            Some(ModelProvider::Claude),
            None,
        )
        .unwrap();
        assert_eq!(target.model_provider, ModelProvider::Codex);
        assert_eq!(target.provider_source, PreferenceSource::Cli);
    }

    #[test]
    fn unified_routing_uses_claude_harness_without_importing_saved_preferences() {
        let target = resolve_routed_launch_target(
            RoutingMode::Unified,
            Some(ModelProvider::Codex),
            None,
            Some(ModelProvider::DeepSeek),
            Some(HarnessSelection::Codex),
        )
        .unwrap();
        assert_eq!(target.routing_mode, RoutingMode::Unified);
        assert_eq!(target.model_provider, ModelProvider::Codex);
        assert_eq!(target.effective_harness, Backend::Claude);
        assert_eq!(target.provider_source, PreferenceSource::Cli);
        assert_eq!(target.harness_source, PreferenceSource::BuiltInDefault);
    }

    #[test]
    fn unified_routing_rejects_an_explicit_codex_harness() {
        assert_eq!(
            resolve_routed_launch_target(
                RoutingMode::Unified,
                None,
                Some(HarnessSelection::Codex),
                None,
                None,
            ),
            Err(LaunchTargetError::UnifiedViaCodexUnsupported)
        );
    }

    #[test]
    fn unified_routing_rejects_an_explicit_deepseek_harness() {
        assert_eq!(
            resolve_routed_launch_target(
                RoutingMode::Unified,
                None,
                Some(HarnessSelection::DeepSeek),
                None,
                None,
            ),
            Err(LaunchTargetError::UnifiedViaDeepSeekUnsupported)
        );
    }

    #[test]
    fn saved_non_default_harness_notice_is_green_and_tty_only() {
        let target = resolve_launch_target(
            false,
            false,
            false,
            None,
            Some(ModelProvider::Codex),
            Some(HarnessSelection::Claude),
        )
        .unwrap();
        let notice = saved_harness_override_notice(target, true, false).unwrap();
        assert!(notice.starts_with("\x1b[32m"));
        assert!(notice.contains("Harness override: Claude (global setting)"));
        assert!(notice.ends_with("\x1b[0m"));
        assert_eq!(saved_harness_override_notice(target, false, false), None);
        assert_eq!(saved_harness_override_notice(target, true, true), None);

        let saved_default = resolve_launch_target(
            false,
            false,
            false,
            None,
            Some(ModelProvider::Codex),
            Some(HarnessSelection::Default),
        )
        .unwrap();
        assert_eq!(
            saved_harness_override_notice(saved_default, true, false),
            None
        );

        let cli_override = resolve_launch_target(
            false,
            true,
            false,
            Some(HarnessSelection::Claude),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            saved_harness_override_notice(cli_override, true, false),
            None
        );
    }

    /// #977: the registry is the one place that says which harness can take
    /// hooks as launch arguments. Codex cannot (no flag names a hooks file).
    #[test]
    fn settings_surface_registry_records_hook_support() {
        assert_eq!(
            Backend::Claude.settings_surface(),
            SettingsSurface::ClaudeSettingsArg
        );
        assert_eq!(
            Backend::Codex.settings_surface(),
            SettingsSurface::CodexConfigOverrides
        );
        assert!(Backend::Claude.settings_surface().accepts_hooks());
        assert!(!Backend::Codex.settings_surface().accepts_hooks());
        assert!(!Backend::DeepSeek.settings_surface().accepts_hooks());
    }

    #[test]
    fn test_executable_names() {
        assert_eq!(Backend::Claude.executable_name(), "claude");
        assert_eq!(Backend::Codex.executable_name(), "codex");
    }
}

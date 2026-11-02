//! Subscription-first provider policy from the environment.
//!
//! clud launches a subscription backend unless the operator deliberately opts
//! into pay-per-token billing with **two** overrides, and every such launch
//! prints a loud warning. Contradictory configuration is an error, never a
//! silent preference. See `docs/architecture/provider-selection.md`
//! ("Metered access needs two overrides").
//!
//! The policy is a pure function of an [`EnvSnapshot`] so the whole table is
//! testable without touching process state; [`EnvSnapshot::from_process`] is
//! the only reader of the real environment.

use crate::backend::ModelProvider;
use crate::failover::CostOwner;

/// The subscription backend used when nothing else selects a provider.
pub const SUBSCRIPTION_PROVIDER_ENV: &str = "CLUD_SUBSCRIPTION_PROVIDER";
/// Explicit provider selection.
pub const PROVIDER_ENV: &str = "CLUD_PROVIDER";
/// The operator's acknowledgement of pay-per-token billing (`1`).
pub const ALLOW_METERED_ENV: &str = "CLUD_ALLOW_METERED";
/// The metered provider being acknowledged; must equal `CLUD_PROVIDER`.
pub const METERED_PROVIDER_ENV: &str = "CLUD_METERED_PROVIDER";

/// The four policy variables, as read (unset and blank both mean `None`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvSnapshot {
    pub subscription_provider: Option<String>,
    pub provider: Option<String>,
    pub allow_metered: Option<String>,
    pub metered_provider: Option<String>,
}

impl EnvSnapshot {
    pub fn from_process() -> Self {
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        Self {
            subscription_provider: read(SUBSCRIPTION_PROVIDER_ENV),
            provider: read(PROVIDER_ENV),
            allow_metered: read(ALLOW_METERED_ENV),
            metered_provider: read(METERED_PROVIDER_ENV),
        }
    }
}

/// What the environment asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPolicy {
    /// The subscription default (`CLUD_SUBSCRIPTION_PROVIDER`, else Claude).
    pub subscription_default: ModelProvider,
    /// True when `CLUD_SUBSCRIPTION_PROVIDER` was unset and Claude was applied.
    pub subscription_default_applied: bool,
    /// `CLUD_PROVIDER`, when set.
    pub selected: Option<ModelProvider>,
    /// `Some` only for a legitimately enabled metered route.
    pub metered: Option<ModelProvider>,
}

impl EnvPolicy {
    /// The provider this policy resolves to when no CLI flag overrides it.
    pub fn provider(&self) -> ModelProvider {
        self.selected.unwrap_or(self.subscription_default)
    }

    /// The multi-line stderr warning for a metered route, `None` otherwise.
    pub fn warning(&self) -> Option<String> {
        self.metered.map(metered_warning)
    }

    /// Whether the Codex bridge may use `OPENAI_API_KEY` when no
    /// subscription login exists. Only the explicit metered Codex route may.
    pub fn allows_codex_api_key(&self) -> bool {
        self.metered == Some(ModelProvider::Codex)
    }
}

/// Every refused configuration. Each message names the remedy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    UnknownProvider {
        var: &'static str,
        value: String,
    },
    MeteredSubscriptionDefault(ModelProvider),
    MeteredWithoutOverrides(ModelProvider),
    MeteredWithoutAllow(ModelProvider),
    AllowWithoutMeteredProvider,
    MeteredProviderMismatch {
        provider: String,
        metered: String,
    },
    AllowWithSubscriptionProvider(ModelProvider),
    InvalidAllowValue(String),
    CliContradictsEnv {
        cli: ModelProvider,
        env: ModelProvider,
    },
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let two = "metered access needs both CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER=<provider> matching CLUD_PROVIDER";
        match self {
            Self::UnknownProvider { var, value } => write!(
                f,
                "{var}={value} is not a known provider; use claude (alias native), codex, \
                 deepseek, kimi or openrouter"
            ),
            Self::MeteredSubscriptionDefault(provider) => write!(
                f,
                "CLUD_SUBSCRIPTION_PROVIDER={provider} names a metered provider; the subscription \
                 default can only be claude (alias native) or codex. To use {provider}, set \
                 CLUD_PROVIDER={provider} with CLUD_ALLOW_METERED=1 and \
                 CLUD_METERED_PROVIDER={provider}"
            ),
            Self::MeteredWithoutOverrides(provider) => write!(
                f,
                "CLUD_PROVIDER={provider} is metered (pay-per-token API billing) and is refused: \
                 {two}. Set CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER={provider}, or unset \
                 CLUD_PROVIDER to use your subscription"
            ),
            Self::MeteredWithoutAllow(provider) => write!(
                f,
                "CLUD_METERED_PROVIDER={provider} is set without CLUD_ALLOW_METERED=1: {two}. Set \
                 CLUD_ALLOW_METERED=1 to acknowledge pay-per-token billing, or unset \
                 CLUD_METERED_PROVIDER"
            ),
            Self::AllowWithoutMeteredProvider => write!(
                f,
                "CLUD_ALLOW_METERED=1 is set without CLUD_METERED_PROVIDER: {two}. Name the \
                 provider you accept paying for, or unset CLUD_ALLOW_METERED"
            ),
            Self::MeteredProviderMismatch { provider, metered } => write!(
                f,
                "CLUD_METERED_PROVIDER={metered} disagrees with CLUD_PROVIDER={provider}: {two}. \
                 Make them the same provider"
            ),
            Self::AllowWithSubscriptionProvider(provider) => write!(
                f,
                "contradiction: CLUD_ALLOW_METERED / CLUD_METERED_PROVIDER are set while \
                 CLUD_PROVIDER={provider} is a subscription provider. Unset the metered \
                 overrides, or select the metered provider you meant"
            ),
            Self::InvalidAllowValue(value) => write!(
                f,
                "CLUD_ALLOW_METERED={value} is not valid; the only accepted value is 1"
            ),
            Self::CliContradictsEnv { cli, env } => write!(
                f,
                "contradiction: the command line selects provider {cli} but CLUD_PROVIDER={env}. \
                 Drop one of them"
            ),
        }
    }
}

impl std::error::Error for PolicyError {}

fn parse_provider(var: &'static str, value: &str) -> Result<ModelProvider, PolicyError> {
    if value.eq_ignore_ascii_case("native") {
        return Ok(ModelProvider::Claude);
    }
    ModelProvider::from_settings_str(value).ok_or_else(|| PolicyError::UnknownProvider {
        var,
        value: value.to_string(),
    })
}

fn is_metered(provider: ModelProvider) -> bool {
    CostOwner::for_provider(provider) == CostOwner::Metered
}

/// Resolve the environment policy. Pure: no I/O, no printing.
pub fn resolve(env: &EnvSnapshot) -> Result<EnvPolicy, PolicyError> {
    let (subscription_default, subscription_default_applied) = match &env.subscription_provider {
        Some(value) => {
            let provider = parse_provider(SUBSCRIPTION_PROVIDER_ENV, value)?;
            if is_metered(provider) {
                return Err(PolicyError::MeteredSubscriptionDefault(provider));
            }
            (provider, false)
        }
        None => (ModelProvider::Claude, true),
    };
    let allow = match env.allow_metered.as_deref() {
        None => false,
        Some("1") => true,
        Some(other) => return Err(PolicyError::InvalidAllowValue(other.to_string())),
    };
    let metered_provider = env
        .metered_provider
        .as_deref()
        .map(|value| parse_provider(METERED_PROVIDER_ENV, value))
        .transpose()?;
    let selected = env
        .provider
        .as_deref()
        .map(|value| parse_provider(PROVIDER_ENV, value))
        .transpose()?;
    let metered = check_overrides(selected, allow, metered_provider)?;
    Ok(EnvPolicy {
        subscription_default,
        subscription_default_applied,
        selected,
        metered,
    })
}

fn check_overrides(
    selected: Option<ModelProvider>,
    allow: bool,
    metered_provider: Option<ModelProvider>,
) -> Result<Option<ModelProvider>, PolicyError> {
    let overrides = allow || metered_provider.is_some();
    if metered_provider == Some(ModelProvider::Claude) {
        return Err(PolicyError::AllowWithSubscriptionProvider(
            ModelProvider::Claude,
        ));
    }
    // Without CLUD_PROVIDER, both overrides are consent for that provider
    // when the command line selects it; either one alone is incomplete.
    let Some(provider) = selected else {
        return match (allow, metered_provider) {
            (false, None) => Ok(None),
            (true, Some(named)) => Ok(Some(named)),
            (false, Some(named)) => Err(PolicyError::MeteredWithoutAllow(named)),
            (true, None) => Err(PolicyError::AllowWithoutMeteredProvider),
        };
    };
    // Claude has no metered mode clud selects; Codex's metered mode is the
    // OPENAI_API_KEY bridge, reachable only through the same two overrides.
    if provider == ModelProvider::Claude && overrides {
        return Err(PolicyError::AllowWithSubscriptionProvider(provider));
    }
    if !is_metered(provider) && !overrides {
        return Ok(None);
    }
    match (allow, metered_provider) {
        (false, None) => Err(PolicyError::MeteredWithoutOverrides(provider)),
        (false, Some(named)) => Err(PolicyError::MeteredWithoutAllow(named)),
        (true, None) => Err(PolicyError::AllowWithoutMeteredProvider),
        (true, Some(named)) if named != provider => Err(PolicyError::MeteredProviderMismatch {
            provider: provider.as_str().to_string(),
            metered: named.as_str().to_string(),
        }),
        (true, Some(_)) => Ok(Some(provider)),
    }
}

/// Reconcile the environment with a provider chosen on the command line
/// (a flag, `--provider`, or a qualified `--model`). A CLI choice may not
/// contradict `CLUD_PROVIDER`.
pub fn reconcile_cli(policy: &EnvPolicy, cli: Option<ModelProvider>) -> Result<(), PolicyError> {
    let Some(cli) = cli else {
        return Ok(());
    };
    match policy.selected {
        Some(env) if env != cli => Err(PolicyError::CliContradictsEnv { cli, env }),
        _ => Ok(()),
    }
}

/// The warning printed (and recorded on the launch) for a metered route.
pub fn metered_warning(provider: ModelProvider) -> String {
    format!(
        "[clud] WARNING: METERED PROVIDER IN USE: {provider}\n\
         [clud] WARNING: this launch uses pay-per-token API billing, not your subscription.\n\
         [clud] WARNING: enabled by CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER={provider}.\n\
         [clud] WARNING: unset both (and CLUD_PROVIDER) to return to your subscription."
    )
}

/// Whether the process environment permits the Codex bridge's metered
/// `OPENAI_API_KEY` fallback. A policy error refuses it.
pub fn process_allows_codex_api_key() -> bool {
    resolve(&EnvSnapshot::from_process()).is_ok_and(|policy| policy.allows_codex_api_key())
}

#[cfg(test)]
#[path = "metered_policy_tests.rs"]
mod tests;

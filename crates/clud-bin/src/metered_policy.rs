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
    /// `Some` only for a legitimately enabled single metered route.
    pub metered: Option<ModelProvider>,
    /// Every metered provider the operator consented to. Without
    /// `CLUD_PROVIDER`, `CLUD_METERED_PROVIDER` may list several
    /// (comma-separated) for a unified gateway session.
    pub consented: Vec<ModelProvider>,
    /// Set when incomplete or mismatched overrides were ignored because they
    /// could not touch a subscription (#2746): a one-line note for stderr.
    pub ignored_overrides: Option<String>,
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

    /// Whether `provider`'s metered billing was consented to.
    pub fn allows(&self, provider: ModelProvider) -> bool {
        self.consented.contains(&provider)
    }

    /// Whether the Codex bridge may use `OPENAI_API_KEY` when no
    /// subscription login exists. Only the explicit metered Codex route may.
    pub fn allows_codex_api_key(&self) -> bool {
        self.allows(ModelProvider::Codex)
    }
}

/// Every refused configuration. Each message names the remedy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    UnknownProvider {
        var: &'static str,
        value: String,
    },
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
    /// `--failover-allow-metered` without the environment consent.
    FailoverMeteredWithoutConsent,
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
            Self::FailoverMeteredWithoutConsent => write!(
                f,
                "--failover-allow-metered descends onto pay-per-token rungs and needs \
                 CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER=<provider> as well"
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
            // An always-metered provider here is simply the default provider,
            // like a saved setting (#2747, DD-209).
            (parse_provider(SUBSCRIPTION_PROVIDER_ENV, value)?, false)
        }
        None => (ModelProvider::Claude, true),
    };
    let allow = match env.allow_metered.as_deref() {
        None => false,
        Some("1") => true,
        Some(other) => return Err(PolicyError::InvalidAllowValue(other.to_string())),
    };
    let listed = env
        .metered_provider
        .as_deref()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| parse_provider(METERED_PROVIDER_ENV, name))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let selected = env
        .provider
        .as_deref()
        .map(|value| parse_provider(PROVIDER_ENV, value))
        .transpose()?;
    if listed.contains(&ModelProvider::Claude) {
        return Err(PolicyError::AllowWithSubscriptionProvider(
            ModelProvider::Claude,
        ));
    }
    let (consented, ignored_overrides) = match check_overrides(selected, allow, &listed) {
        Ok(consented) => (consented, None),
        // #2746: an incomplete pair can only matter to the Codex API-key route;
        // anywhere else it is a leftover and must not refuse the launch.
        Err(error) if is_leftover(&error, selected, &listed) => {
            (Vec::new(), Some(leftover_note(env)))
        }
        Err(error) => return Err(error),
    };
    let metered = match (selected, consented.as_slice()) {
        (Some(provider), [only]) if *only == provider => Some(provider),
        (None, [only]) => Some(*only),
        _ => None,
    };
    Ok(EnvPolicy {
        subscription_default,
        subscription_default_applied,
        selected,
        metered,
        consented,
        ignored_overrides,
    })
}

/// The one-line note for ignored leftover overrides (#2757): what is set, and
/// that it does nothing here. No advice to complete the pair.
fn leftover_note(env: &EnvSnapshot) -> String {
    let set: Vec<String> = [
        (ALLOW_METERED_ENV, &env.allow_metered),
        (METERED_PROVIDER_ENV, &env.metered_provider),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.as_ref().map(|value| format!("{name}={value}")))
    .collect();
    format!(
        "[clud] note: {} has no effect (only Codex's API-key route uses the metered \
         overrides); unset it to silence this",
        set.join(" ")
    )
}

/// Whether a refused override pair is a harmless leftover: it names no
/// subscription provider and the launch is not Codex's (#2746).
fn is_leftover(
    error: &PolicyError,
    selected: Option<ModelProvider>,
    listed: &[ModelProvider],
) -> bool {
    let incomplete = matches!(
        error,
        PolicyError::MeteredWithoutAllow(_)
            | PolicyError::AllowWithoutMeteredProvider
            | PolicyError::MeteredProviderMismatch { .. }
    );
    incomplete
        && selected.is_none_or(is_metered)
        && listed.iter().all(|provider| is_metered(*provider))
}

fn check_overrides(
    selected: Option<ModelProvider>,
    allow: bool,
    listed: &[ModelProvider],
) -> Result<Vec<ModelProvider>, PolicyError> {
    let overrides = allow || !listed.is_empty();
    // Without CLUD_PROVIDER, both overrides are consent for the listed
    // providers when the command line selects them; either one alone is
    // incomplete.
    let Some(provider) = selected else {
        return match (allow, listed.first()) {
            (false, None) => Ok(Vec::new()),
            (true, Some(_)) => Ok(listed.to_vec()),
            (false, Some(named)) => Err(PolicyError::MeteredWithoutAllow(*named)),
            (true, None) => Err(PolicyError::AllowWithoutMeteredProvider),
        };
    };
    if listed.len() > 1 {
        return Err(PolicyError::MeteredProviderMismatch {
            provider: provider.as_str().to_string(),
            metered: listed
                .iter()
                .map(|named| named.as_str())
                .collect::<Vec<_>>()
                .join(","),
        });
    }
    let metered_provider = listed.first().copied();
    // Claude has no metered mode clud selects; Codex's metered mode is the
    // OPENAI_API_KEY bridge, reachable only through the same two overrides.
    if provider == ModelProvider::Claude && overrides {
        return Err(PolicyError::AllowWithSubscriptionProvider(provider));
    }
    if !is_metered(provider) && !overrides {
        return Ok(Vec::new());
    }
    match (allow, metered_provider) {
        // DeepSeek, Kimi and OpenRouter have no subscription to protect
        // (DD-209): selecting one needs no overrides.
        (false, None) => Ok(Vec::new()),
        (false, Some(named)) => Err(PolicyError::MeteredWithoutAllow(named)),
        (true, None) => Err(PolicyError::AllowWithoutMeteredProvider),
        (true, Some(named)) if named != provider => Err(PolicyError::MeteredProviderMismatch {
            provider: provider.as_str().to_string(),
            metered: named.as_str().to_string(),
        }),
        (true, Some(_)) => Ok(vec![provider]),
    }
}

/// Reconcile the environment with a provider chosen on the command line
/// (a flag, `--provider`, or a qualified `--model`). A CLI choice may not
/// contradict `CLUD_PROVIDER`: a Claude or Codex launch is never moved onto
/// another provider, and the reverse (maintainer directive on #2748).
pub fn reconcile_cli(policy: &EnvPolicy, cli: Option<ModelProvider>) -> Result<(), PolicyError> {
    let Some(cli) = cli else {
        return Ok(());
    };
    match policy.selected {
        Some(env) if env != cli => Err(PolicyError::CliContradictsEnv { cli, env }),
        _ => Ok(()),
    }
}

/// Whether the user named this metered provider themselves on the command
/// line (`--openrouter`, `--deepseek`, `--kimi`, `--provider <p>`). Naming it
/// is consent: the gate exists to stop *implicit* routes (a saved setting, a
/// provider inferred from `--model`, a credential fallback, failover) from
/// silently moving a subscription user onto API billing.
pub fn explicitly_selected_metered(
    provider: ModelProvider,
    explicit_cli: Option<ModelProvider>,
) -> bool {
    is_metered(provider) && explicit_cli == Some(provider)
}

/// The final gate on a resolved launch. The policy protects only the Claude
/// and Codex subscriptions (DD-209): DeepSeek, Kimi and OpenRouter are always
/// pay-per-token, so reaching one by any route needs no consent. Only
/// `--failover-allow-metered`, which moves a subscription session onto API
/// billing, still needs the environment overrides.
pub fn enforce_resolved(
    policy: &EnvPolicy,
    failover_allow_metered: bool,
) -> Result<(), PolicyError> {
    if failover_allow_metered && policy.consented.is_empty() {
        return Err(PolicyError::FailoverMeteredWithoutConsent);
    }
    Ok(())
}

/// Whether `provider`'s metered key may be used. Always true for the
/// inherently metered providers (DD-209); Codex's API key needs consent.
pub fn process_allows_metered(provider: ModelProvider) -> bool {
    is_metered(provider)
        || resolve(&EnvSnapshot::from_process()).is_ok_and(|policy| policy.allows(provider))
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

/// Whether a launch on `provider` is pay-per-token.
pub fn provider_is_metered(provider: ModelProvider) -> bool {
    is_metered(provider)
}

/// The warning for a metered provider reached without the overrides, e.g. a
/// saved provider setting or a provider inferred from `--model`.
pub fn configured_metered_warning(provider: ModelProvider) -> String {
    format!(
        "[clud] WARNING: METERED PROVIDER IN USE: {provider}\n\
     [clud] WARNING: this launch uses pay-per-token API billing, not your subscription.\n\
     [clud] WARNING: selected by your configuration (saved setting, --model or CLUD_PROVIDER)."
    )
}

/// The warning for a metered provider the user named on the command line.
pub fn explicit_metered_warning(provider: ModelProvider) -> String {
    format!(
        "[clud] WARNING: METERED PROVIDER IN USE: {provider}\n\
     [clud] WARNING: this launch uses pay-per-token API billing, not your subscription.\n\
     [clud] WARNING: selected explicitly with --{provider}."
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

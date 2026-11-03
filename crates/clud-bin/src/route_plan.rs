//! One resolved route per launch (#1852 step 1, issue #1855).
//!
//! clud reaches non-Anthropic models through four backends, each of which used
//! to re-derive the same launch decisions from `LaunchPlan` and the provider
//! descriptor: the direct overlay, the unified gateway, the Codex-via-Claude
//! bridge, and the DeepSeek-native harness. This module makes that decision
//! **once**, as a pure function of the plan (plus the one piece of process
//! state the precedence rule genuinely needs, [`Ambient`]).
//!
//! Step 1 only *computes* the route and carries it on `LaunchPlan`; no backend
//! reads it yet. Steps 2-4 turn each backend into a renderer of this value and
//! then delete the copies it replaces, so the snapshots in
//! `route_snapshots_tests.rs` must stay byte-identical through those steps.
//!
//! Two deliberate deviations from the sketch in #1852:
//!
//! - `scrub`, `base_url` and the credential identifiers are owned `String`s
//!   rather than `&'static`, because the route rides on `LaunchPlan` and must
//!   survive a lossless serde round trip to a daemon worker.
//! - `CredentialSource::Vault(ModelProvider)` names the provider rather than a
//!   `{service, account}` pair. The identifiers are read from the provider's
//!   registry descriptor at spawn time, which is what removes the hard-coded
//!   vault constants the unified overlay carries today (divergence #5).
//!
//! A fifth [`RouteBackend::Native`] covers a plain Claude launch, which has no
//! provider overlay at all and therefore re-derives nothing.

use serde::{Deserialize, Serialize};

use crate::backend::{Backend, ModelProvider, RoutingMode};
use crate::codex_bridge::UNIFIED_GATEWAY_TOKEN_HEADER;
use crate::codex_history::ConversationRoute;
use crate::command::LaunchPlan;

/// The direct route's client timeout (#1263): the harness's own, since clud is
/// not in the request path and cannot time the stream out itself.
pub const DIRECT_TIMEOUT_MS: &str = "600000";
/// Every bridge route's request timeout (DD-028). Deliberately separate from
/// the direct value: a bridge has clud in the path and can watch the stream.
pub const BRIDGE_TIMEOUT_MS: &str = "3000000";

/// Env keys the direct Anthropic-compatible overlay owns. Removing them all is
/// a security guarantee, not an OS-semantics match: an ambient Anthropic key
/// must never leak into a child that is billed to another provider's key.
///
/// `CLAUDE_CODE_EFFORT_LEVEL` is deliberately absent (DD-059): an ambient user
/// value is preserved so the harness's own `/effort` control stays
/// authoritative, and the catalog default travels on the harness's `--effort`
/// flag instead. `ANTHROPIC_CUSTOM_MODEL_OPTION*` is a prefix scrub, applied
/// separately by every overlay that rewrites the model slots.
pub const ANTHROPIC_COMPAT_SCRUB: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
];

/// Env keys the Codex-via-Claude bridge overlay owns.
pub const CODEX_VIA_CLAUDE_SCRUB: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
];

/// Prefix, not a key: every `ANTHROPIC_CUSTOM_MODEL_OPTION*` spelling is
/// dropped by the overlays that rewrite the model slots.
pub const CUSTOM_MODEL_OPTION_PREFIX: &str = "ANTHROPIC_CUSTOM_MODEL_OPTION";

/// The gateway's current slot bindings on the Codex bridge. The bridge cannot
/// serve Anthropic model IDs, so both roles bind to honest Codex rows.
pub const CODEX_BRIDGE_OPUS_MODEL: &str = "clud-claude-codex-sol";
pub const CODEX_BRIDGE_SONNET_MODEL: &str = "clud-claude-codex-terra";

/// The environment the precedence rules consult, kept explicit so resolving a
/// route stays a pure function of its inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ambient {
    /// Explicit OpenCode endpoint override (Anthropic-compatible Messages API).
    pub opencode_base_url: Option<String>,
    /// An inherited `CLAUDE_CODE_SUBAGENT_MODEL`. It wins the subagent slot
    /// only on a constrained launch and only when the allowlist admits it
    /// (#1257): clud owns the boundary, the user chooses inside it.
    pub subagent_model: Option<String>,
    /// #1984: a debug-build, integration-test-only replacement for a direct
    /// route's upstream (`CLUD_TEST_DIRECT_UPSTREAM_URL`), so an end-to-end
    /// test can point `clud --openrouter` at a scripted local model. Release
    /// builds and ordinary runs never read it.
    pub direct_upstream_url: Option<String>,
}

/// The direct-route test upstream variable; see [`Ambient::direct_upstream_url`].
pub const TEST_DIRECT_UPSTREAM_ENV: &str = "CLUD_TEST_DIRECT_UPSTREAM_URL";

fn test_direct_upstream(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    let enabled =
        cfg!(debug_assertions) && lookup("CLUD_INTEGRATION_TESTS").as_deref() == Some("1");
    enabled
        .then(|| lookup(TEST_DIRECT_UPSTREAM_ENV))
        .flatten()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
}

impl Ambient {
    /// Read the ambient values from a child environment. The pinned launcher
    /// hands the runtime the environment the child will inherit, which is the
    /// one the precedence rule has to be evaluated against.
    pub fn from_child_env(env: &[(String, String)]) -> Self {
        Self {
            opencode_base_url: env
                .iter()
                .find(|(key, _)| key == "ANTHROPIC_BASE_URL")
                .map(|(_, value)| value.clone()),
            subagent_model: env
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("CLAUDE_CODE_SUBAGENT_MODEL"))
                .map(|(_, value)| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            direct_upstream_url: test_direct_upstream(|key| {
                env.iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            }),
        }
    }

    /// Read the ambient values from this process.
    pub fn from_process() -> Self {
        Self {
            opencode_base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
            subagent_model: std::env::var("CLAUDE_CODE_SUBAGENT_MODEL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            direct_upstream_url: test_direct_upstream(|key| std::env::var(key).ok()),
        }
    }
}

/// Which of clud's backends serves this launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteBackend {
    /// Plain Claude: no provider overlay, nothing re-derived.
    Native,
    /// A descriptor-backed provider straight from the Claude harness
    /// (`--openrouter`, `--deepseek`, `--kimi`).
    Direct,
    /// clud's launch-scoped gateway in front of the Claude harness.
    Unified,
    /// The Codex translation bridge behind the Claude harness.
    CodexBridge,
    /// The `dsh` harness, which owns its own provider configuration.
    DeepSeekNative,
    /// OpenCode calling a Messages-compatible provider directly.
    OpenCodeNative,
}

impl RouteBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Direct => "direct",
            Self::Unified => "unified",
            Self::CodexBridge => "codex_bridge",
            Self::DeepSeekNative => "deepseek_native",
            Self::OpenCodeNative => "opencode_native",
        }
    }
}

/// Where an upstream's credential comes from. Never the credential itself: the
/// secret is read once, at spawn time, from this description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "source", content = "value")]
pub enum CredentialSource {
    /// The Claude harness's own auth. clud neither reads nor injects it, which
    /// is what lets saved claude.ai OAuth reach the native upstream through
    /// the unified gateway.
    Harness,
    /// Codex's subscription credentials, resolved by `codex_upstream`.
    CodexAuth,
    /// clud's native credential vault. The identifiers come from the
    /// provider's registry descriptor, so there is one copy of them.
    Vault(ModelProvider),
    /// A key the child reads from its own environment. The DeepSeek-native
    /// harness keeps its keys in the process environment, and naming that
    /// explicitly is what makes the difference visible and testable.
    Env(String),
}

impl CredentialSource {
    /// The vault identifiers this source names, when it is a vault source.
    pub fn vault_identifiers(&self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Vault(provider) => crate::provider_registry::descriptor_for(*provider)
                .map(|descriptor| (descriptor.vault_service, descriptor.vault_account)),
            _ => None,
        }
    }
}

/// One upstream this launch can reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamRoute {
    pub provider: ModelProvider,
    pub base_url: String,
    pub credential: CredentialSource,
    pub conversation_route: ConversationRoute,
}

/// One slot's chosen model. The resolver decides *which row*; a renderer asks
/// for the id its backend needs -- `wire_id` for the direct route, which
/// writes the id verbatim, and `discovery_id` for the gateway, which must name
/// a row Claude Code can classify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotModel {
    /// The spelling the direct route writes verbatim.
    pub wire_id: String,
    /// The gateway discovery id, when the row has one. Claude Code classifies
    /// a raw wire id as an unknown provider id and falls back to a built-in
    /// Anthropic row, which is exactly the spend a pin exists to stop.
    pub discovery_id: Option<String>,
    /// The catalog row's human-readable name, for the two role aliases Claude
    /// Code displays (`ANTHROPIC_DEFAULT_*_MODEL_NAME`).
    pub display_name: Option<String>,
    /// Write `wire_id` as it stands, even on a gateway route that would
    /// otherwise substitute the row's discovery id. An ambient
    /// `CLAUDE_CODE_SUBAGENT_MODEL` the user set is an exact spelling the
    /// harness already resolved; clud admits it, it does not rewrite it
    /// (#1257).
    pub verbatim: bool,
}

impl SlotModel {
    /// The slot for an ambient id the user set: written exactly as given.
    pub fn verbatim(id: &str) -> Option<Self> {
        Self::of(id).map(|slot| Self {
            verbatim: true,
            ..slot
        })
    }

    /// The slot a named id resolves to, or `None` when nothing names it.
    pub fn of(id: &str) -> Option<Self> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        Some(match crate::provider_catalog::model_by_any_id(id) {
            Some(row) => Self {
                wire_id: id.to_string(),
                discovery_id: row.discovery_id.map(str::to_string),
                display_name: Some(row.display_name.to_string()),
                verbatim: false,
            },
            None => Self {
                wire_id: id.to_string(),
                discovery_id: None,
                display_name: None,
                verbatim: false,
            },
        })
    }
}

/// The six model slots Claude Code resolves a launch through. `None` means
/// this backend leaves that slot alone, which is a decision in its own right:
/// the unified gateway sets none of them when the launch is unpinned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotModels {
    pub main: Option<SlotModel>,
    pub opus: Option<SlotModel>,
    pub sonnet: Option<SlotModel>,
    pub haiku: Option<SlotModel>,
    pub fable: Option<SlotModel>,
    pub subagent: Option<SlotModel>,
}

/// What the gateway may advertise through `/v1/models` discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "policy", content = "allowlist")]
pub enum DiscoveryPolicy {
    /// Discovery is not requested; the model set is clud's own.
    Off,
    /// Discovery is on and may add any row the route can serve.
    On,
    /// Discovery is on but clud filters it to exactly these ids.
    Filtered(Vec<String>),
}

/// The context window a launch tells the harness about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPolicy {
    /// `CLAUDE_CODE_MAX_CONTEXT_TOKENS`: the real served window, when known.
    pub max_tokens: Option<u32>,
    /// `CLAUDE_CODE_AUTO_COMPACT_WINDOW`: the compact threshold, when the
    /// catalog reviewed one for this wire id.
    pub auto_compact_window: Option<u32>,
}

/// The per-turn effort capability (#1528).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortPolicy {
    /// The `CLAUDE_CODE_MODEL_CAPABILITIES` token, e.g.
    /// `vendor/model=per_turn_effort`. `None` means this launch advertises no
    /// capability, which is the safe default: the harness then keeps its own.
    pub per_turn_capability: Option<String>,
}

/// OpenRouter's upstream routing object (`--provider-only`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRouting {
    pub only: Vec<String>,
    pub allow_fallbacks: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub zdr: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<String>,
}

/// Whether a launch check is meaningful for a route, and why not when it is
/// not. `Unsupported` is a hard refusal that names the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "reason")]
pub enum Applicability {
    Applies,
    NotApplicable(String),
    Unsupported(String),
}

/// One launch check's placement and verdict, recorded so `--dry-run` can show
/// what ran. Step 5c (#1862) turns these into enforcement before the backend
/// branch; step 1 only classifies them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchCheck {
    pub name: String,
    pub applicability: Applicability,
    pub verdict: String,
}

/// How one scrubbed key matches a candidate environment key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrubMode {
    /// Case-insensitive on every platform. This is a security guarantee -- an
    /// ambient Anthropic key must never leak into a child billed to another
    /// provider's key -- not a mirror of per-OS env-var uniqueness.
    AnyCase,
    /// Per-OS environment-variable uniqueness, which is what the bridge
    /// overlays have always used.
    OsSemantics,
    /// Every key starting with this prefix: `ANTHROPIC_CUSTOM_MODEL_OPTION`
    /// has no fixed suffix and would otherwise survive a model rewrite.
    Prefix,
}

/// One key a renderer removes from the child environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubKey {
    pub key: String,
    pub mode: ScrubMode,
}

impl ScrubKey {
    pub fn any_case(key: &str) -> Self {
        Self {
            key: key.to_string(),
            mode: ScrubMode::AnyCase,
        }
    }

    pub fn os_semantics(key: &str) -> Self {
        Self {
            key: key.to_string(),
            mode: ScrubMode::OsSemantics,
        }
    }

    pub fn matches(&self, candidate: &str) -> bool {
        match self.mode {
            ScrubMode::AnyCase => candidate.eq_ignore_ascii_case(&self.key),
            ScrubMode::OsSemantics => os_env_key_eq(candidate, &self.key),
            ScrubMode::Prefix => candidate
                .to_ascii_uppercase()
                .starts_with(&self.key.to_ascii_uppercase()),
        }
    }
}

/// The per-OS rule for whether two environment-variable spellings name the
/// same variable.
pub fn os_env_key_eq(left: &str, right: &str) -> bool {
    if crate::platform::host::WINDOWS {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

/// Everything one launch decided, decided once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRoute {
    pub backend: RouteBackend,
    pub provider: ModelProvider,
    /// Every upstream this launch may reach, in preference order. One for the
    /// direct route; Claude first for the unified gateway.
    pub upstreams: Vec<UpstreamRoute>,
    pub slots: SlotModels,
    /// The boundary every slot, discovery row and bridge request is held to
    /// (#1257). Empty means unconstrained.
    pub allowlist: Vec<String>,
    pub discovery: DiscoveryPolicy,
    pub context: ContextPolicy,
    pub effort: EffortPolicy,
    pub upstream_routing: Option<ProviderRouting>,
    #[serde(default)]
    pub redaction: crate::secret_redaction::policy::Policy,
    /// The effective per-request timeout, in milliseconds. `None` means this
    /// backend does not set one.
    pub timeout_ms: Option<u64>,
    /// Env keys every renderer of this route removes, and how each matches.
    pub scrub: Vec<ScrubKey>,
    pub checks: Vec<LaunchCheck>,
}

impl ResolvedRoute {
    /// The environment variable a non-vault credential reads, when this route
    /// uses one. The native harnesses keep their key in the process
    /// environment, which is why they are the one `CredentialSource::Env`.
    pub fn credential_env(&self) -> Option<&str> {
        self.upstreams
            .iter()
            .find_map(|upstream| match &upstream.credential {
                CredentialSource::Env(var) => Some(var.as_str()),
                _ => None,
            })
    }
}

/// The one provider-to-route mapping. `failover::route_for` and
/// `codex_bridge::conversation_route_for` were identical copies of this table;
/// step 6 (#1864) deletes them and points their callers here.
pub const fn conversation_route(provider: ModelProvider) -> ConversationRoute {
    match provider {
        ModelProvider::Claude => ConversationRoute::Claude,
        ModelProvider::Codex => ConversationRoute::Codex,
        ModelProvider::DeepSeek => ConversationRoute::DeepSeek,
        ModelProvider::OpenRouter => ConversationRoute::OpenRouter,
        ModelProvider::Kimi => ConversationRoute::Kimi,
    }
}

fn is_codex_via_claude(plan: &LaunchPlan) -> bool {
    plan.model_provider() == ModelProvider::Codex && plan.effective_harness() == Backend::Claude
}

fn is_unified(plan: &LaunchPlan) -> bool {
    plan.routing_mode == RoutingMode::Unified && plan.effective_harness() == Backend::Claude
}

fn is_direct(plan: &LaunchPlan) -> bool {
    plan.effective_harness() == Backend::Claude
        && crate::provider_registry::descriptor_for(plan.model_provider()).is_some()
}

/// The active backend for a plan. Order matters and mirrors
/// `ForegroundRuntime::start_with_secret_store`'s branch, so the route and the
/// runtime can never disagree about which backend is running.
pub fn backend_for(plan: &LaunchPlan) -> RouteBackend {
    if is_unified(plan) {
        RouteBackend::Unified
    } else if is_codex_via_claude(plan) {
        RouteBackend::CodexBridge
    } else if is_direct(plan) {
        RouteBackend::Direct
    } else if plan.effective_harness() == Backend::OpenCode {
        RouteBackend::OpenCodeNative
    } else if plan.effective_harness() == Backend::DeepSeek {
        RouteBackend::DeepSeekNative
    } else {
        RouteBackend::Native
    }
}

/// The wire model a direct launch starts on: the selection's, else the
/// provider's reviewed catalog default.
pub(crate) fn direct_wire_model(
    provider: ModelProvider,
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
) -> Option<&str> {
    selection
        .and_then(|selection| selection.wire_model.as_deref())
        .or_else(|| {
            crate::provider_catalog::reviewed_default_model(provider).map(|row| row.wire_id)
        })
}

/// The boundary a pin creates, with the Codex bridge's own injected role rows
/// folded in (DD-038's substitutions are inside the boundary by construction).
pub fn codex_bridge_allowlist(plan: &LaunchPlan) -> Vec<String> {
    let mut allowed = plan.allowed_models.clone();
    if !allowed.is_empty() {
        for role in [CODEX_BRIDGE_OPUS_MODEL, CODEX_BRIDGE_SONNET_MODEL] {
            if !allowed.iter().any(|entry| entry.eq_ignore_ascii_case(role)) {
                allowed.push(role.to_string());
            }
        }
    }
    allowed
}

/// The slot decision for every descriptor-driven backend: which row each slot
/// names, with the ambient subagent precedence applied exactly once.
///
/// This is the single copy of a rule that today exists twice, and whose two
/// copies already differ (divergence #1, DD-077 / #1257).
fn descriptor_slots(
    descriptor: &'static crate::provider_registry::AnthropicCompatProvider,
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
    allowlist: &[String],
    ambient: &Ambient,
) -> SlotModels {
    let model = direct_wire_model(descriptor.provider, selection).unwrap_or_default();
    let role_models = descriptor.role_models;
    let served_subagent = crate::server_settings::provider_subagent_model(descriptor.provider)
        .unwrap_or(descriptor.subagent_wire_id);
    let constrained = crate::provider_catalog::model_allowlist_slot(allowlist, selection);
    let ambient_subagent = ambient
        .subagent_model
        .as_deref()
        .filter(|_| constrained.is_some())
        .filter(|value| crate::provider_catalog::model_allowlist_allows(allowlist, value));
    let (opus, sonnet, haiku, subagent, fable) = match constrained.as_deref() {
        // A pin covers every slot, and fable with it: an OpenRouter key bills
        // whatever id the harness sends, aliases included.
        Some(constrained) => (
            constrained,
            constrained,
            constrained,
            ambient_subagent.unwrap_or(constrained),
            Some(constrained),
        ),
        // Unconstrained: the provider's own role table is authoritative. A
        // provider without one is single-profile, so every slot is the main
        // model -- except fable, which the overlay pins to main only in that
        // case. A provider *with* a table pins fable only if the table names
        // one; `None` there means "leave the slot alone".
        None => (
            role_models.map_or(model, |roles| roles.opus),
            role_models.map_or(model, |roles| roles.sonnet),
            role_models.map_or(served_subagent, |roles| roles.haiku),
            ambient_subagent
                .unwrap_or_else(|| role_models.map_or(served_subagent, |roles| roles.subagent)),
            role_models.map_or(Some(model), |roles| roles.fable),
        ),
    };
    let main = constrained.as_deref().unwrap_or(model);
    SlotModels {
        main: SlotModel::of(main),
        opus: SlotModel::of(opus),
        sonnet: SlotModel::of(sonnet),
        haiku: SlotModel::of(haiku),
        subagent: if ambient_subagent == Some(subagent) {
            SlotModel::verbatim(subagent)
        } else {
            SlotModel::of(subagent)
        },
        fable: fable.and_then(SlotModel::of),
    }
}

/// The pinned slot decision for a launch whose provider has no descriptor of
/// its own. The gateway serves rows from every provider it holds a route for,
/// so a pin is a boundary over the allowlist rather than over the descriptor.
fn pinned_slots(
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
    allowlist: &[String],
    ambient: &Ambient,
) -> SlotModels {
    let Some(pinned) = crate::provider_catalog::model_allowlist_slot(allowlist, selection) else {
        return SlotModels::default();
    };
    let subagent = ambient
        .subagent_model
        .as_deref()
        .filter(|value| crate::provider_catalog::model_allowlist_allows(allowlist, value))
        .unwrap_or(&pinned);
    SlotModels {
        main: SlotModel::of(&pinned),
        opus: SlotModel::of(&pinned),
        sonnet: SlotModel::of(&pinned),
        haiku: SlotModel::of(&pinned),
        fable: SlotModel::of(&pinned),
        subagent: if subagent == pinned {
            SlotModel::of(&pinned)
        } else {
            SlotModel::verbatim(subagent)
        },
    }
}

/// The context window a wire id is served with: the catalog's reviewed compact
/// window and the served datasheet's exact window, keyed on the same id on
/// every backend (divergence #3).
fn context_policy(wire_id: Option<&str>) -> ContextPolicy {
    let Some(wire_id) = wire_id else {
        return ContextPolicy::default();
    };
    ContextPolicy {
        max_tokens: crate::server_settings::effective_context_window(wire_id),
        auto_compact_window: crate::provider_catalog::model_by_wire_id(wire_id)
            .and_then(|row| row.claude_compact_window),
    }
}

fn upstream_for(
    provider: ModelProvider,
    base_url: &str,
    credential: CredentialSource,
) -> UpstreamRoute {
    UpstreamRoute {
        provider,
        base_url: base_url.to_string(),
        credential,
        conversation_route: conversation_route(provider),
    }
}

fn scrub_list(backend: RouteBackend, provider: ModelProvider) -> Vec<ScrubKey> {
    let mut keys: Vec<ScrubKey> = match backend {
        // The direct overlay's rule is case-insensitive on every platform.
        RouteBackend::Direct => ANTHROPIC_COMPAT_SCRUB
            .iter()
            .map(|key| ScrubKey::any_case(key))
            .collect(),
        // The bridge overlays keep the per-OS uniqueness rule.
        // It also drops the user's traffic switch: discovery was already
        // refused when it is on, and any other value must not reach the child.
        RouteBackend::CodexBridge => CODEX_VIA_CLAUDE_SCRUB
            .iter()
            .chain(std::iter::once(&"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"))
            .map(|key| ScrubKey::os_semantics(key))
            .collect(),
        // The gateway replaces only its own two keys; it deliberately leaves
        // the Claude credential in place so native auth reaches the upstream.
        RouteBackend::Unified => vec![
            ScrubKey::os_semantics("ANTHROPIC_BASE_URL"),
            ScrubKey::os_semantics("ANTHROPIC_CUSTOM_HEADERS"),
        ],
        RouteBackend::Native | RouteBackend::DeepSeekNative | RouteBackend::OpenCodeNative => {
            Vec::new()
        }
    };
    // The direct route additionally drops this provider's own ambient key:
    // an OpenRouter child must never inherit one from the parent shell.
    if backend == RouteBackend::Direct && provider == ModelProvider::OpenRouter {
        keys.push(ScrubKey::any_case("OPENROUTER_API_KEY"));
    }
    // Only the overlays that rewrite the model slots drop the user's own
    // custom model options; the gateway leaves them alone entirely.
    if matches!(backend, RouteBackend::Direct | RouteBackend::CodexBridge) {
        keys.push(ScrubKey {
            key: CUSTOM_MODEL_OPTION_PREFIX.to_string(),
            mode: ScrubMode::Prefix,
        });
    }
    keys
}

/// One launch check's placement, verdict and -- when it refuses -- the message
/// the launch must fail with. `refusal` is the whole point: a check that only
/// classifies is documentation, not a gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckVerdict {
    pub check: LaunchCheck,
    pub refusal: Option<String>,
}

/// Every check a launch is subject to, evaluated against the route. This is
/// the one place a launch check's placement is decided; `Unsupported` on a
/// backend is a hard refusal that names it, and a new backend that does not
/// classify a check fails `every_check_is_classified_for_every_backend`.
pub fn evaluate(
    route: &ResolvedRoute,
    catalog: &crate::openrouter_catalog::Catalog,
) -> Vec<CheckVerdict> {
    vec![
        request_path_verdict(route.backend),
        free_cost_verdict(route, catalog),
        provider_only_verdict(route),
        redaction_verdict(route),
    ]
}

/// Request-path features (failover, retries, error translation, catalog
/// filtering) need clud in the request path. Recording that as a check is how
/// `--dry-run` can show a user why `--unified` recovers from an upstream error
/// and `--openrouter` cannot (#1852, D21).
fn request_path_verdict(backend: RouteBackend) -> CheckVerdict {
    let applicability = match backend {
        RouteBackend::Unified | RouteBackend::CodexBridge => Applicability::Applies,
        RouteBackend::DeepSeekNative => Applicability::NotApplicable(
            "the DeepSeek harness owns its own request path".to_string(),
        ),
        RouteBackend::Direct | RouteBackend::Native | RouteBackend::OpenCodeNative => {
            Applicability::NotApplicable(
                "clud is not in the request path on a direct route".to_string(),
            )
        }
    };
    let verdict = match applicability {
        Applicability::Applies => {
            "failover, retries, error translation and catalog filtering run in clud".to_string()
        }
        _ => "not offered here; the direct route has no clud egress by design".to_string(),
    };
    CheckVerdict {
        check: LaunchCheck {
            name: "request_path_features".to_string(),
            applicability,
            verdict,
        },
        refusal: None,
    }
}

fn redaction_verdict(route: &ResolvedRoute) -> CheckVerdict {
    let supported = route.backend == RouteBackend::Unified;
    let refusal = (route.redaction.enabled && !supported)
        .then(|| crate::secret_redaction::policy::GATEWAY_REQUIRED.to_string());
    let applicability = if let Some(reason) = &refusal {
        Applicability::Unsupported(reason.clone())
    } else if route.redaction.enabled {
        Applicability::Applies
    } else {
        Applicability::NotApplicable(route.redaction.reason.clone())
    };
    CheckVerdict {
        check: LaunchCheck {
            name: "secret_redaction".to_string(),
            applicability,
            verdict: route.redaction.reason.clone(),
        },
        refusal,
    }
}

/// #1833: a `:free` id is a cost promise the offline catalog has to prove. The
/// refusal is about the id, not the backend, so every backend that can bill it
/// is refused.
fn free_cost_verdict(
    route: &ResolvedRoute,
    catalog: &crate::openrouter_catalog::Catalog,
) -> CheckVerdict {
    let openrouter = route.provider == ModelProvider::OpenRouter;
    let wire = route
        .slots
        .main
        .as_ref()
        .map(|slot| slot.wire_id.as_str())
        .filter(|_| openrouter);
    let free = wire.map(|wire| {
        (
            wire.to_string(),
            crate::openrouter_free::check(wire, catalog),
        )
    });
    let applicability = if free.is_some() {
        Applicability::Applies
    } else if openrouter {
        Applicability::NotApplicable("the launch names no model".to_string())
    } else {
        Applicability::NotApplicable("the launch does not name an OpenRouter model".to_string())
    };
    let refusal = free
        .as_ref()
        .and_then(|(wire, verdict)| crate::openrouter_free::refusal(wire, verdict, catalog));
    let verdict = match (&free, &refusal) {
        (_, Some(refusal)) => format!("refused: {refusal}"),
        (Some((_, verdict)), None) => {
            format!("proved free by the offline catalog ({})", verdict.as_str())
        }
        (None, None) => "no `:free` wire id on this launch".to_string(),
    };
    CheckVerdict {
        check: LaunchCheck {
            name: "openrouter_free_cost".to_string(),
            applicability,
            verdict,
        },
        refusal,
    }
}

/// OpenRouter's upstream routing object. Only an OpenRouter launch clud routes
/// can carry it: the direct route writes `CLAUDE_CODE_EXTRA_BODY`, the gateway
/// injects it per request, and nothing else can receive it.
fn provider_only_verdict(route: &ResolvedRoute) -> CheckVerdict {
    let backend = route.backend;
    let openrouter = route.provider == ModelProvider::OpenRouter;
    let applicability = if route.upstream_routing.is_none() {
        Applicability::NotApplicable("no OpenRouter routing flags on this launch".to_string())
    } else if !openrouter || !matches!(backend, RouteBackend::Direct | RouteBackend::Unified) {
        Applicability::Unsupported(
            "--provider-only, --zdr and --no-data-collection apply only to an OpenRouter \
             launch clud routes"
                .to_string(),
        )
    } else {
        Applicability::Applies
    };
    let refusal = match &applicability {
        Applicability::Unsupported(reason) => Some(reason.clone()),
        _ => None,
    };
    let verdict = match (&refusal, route.upstream_routing.as_ref(), backend) {
        (Some(_), _, _) => "refused before launch".to_string(),
        (None, Some(routing), RouteBackend::Direct) => {
            format!(
                "rendered into CLAUDE_CODE_EXTRA_BODY for {:?}",
                routing.only
            )
        }
        (None, Some(_), RouteBackend::Unified) => {
            "the gateway injects provider.only per request (#1863)".to_string()
        }
        (None, Some(_), _) => "refused before launch".to_string(),
        (None, None, _) => "nothing to pin".to_string(),
    };
    CheckVerdict {
        check: LaunchCheck {
            name: "provider_only".to_string(),
            applicability,
            verdict,
        },
        refusal,
    }
}

/// The first refusal any check produces, with the check's name, or `None` when
/// the route may proceed. Callers refuse the launch before the backend branch.
pub fn first_refusal(
    route: &ResolvedRoute,
    catalog: &crate::openrouter_catalog::Catalog,
) -> Option<String> {
    evaluate(route, catalog).into_iter().find_map(|verdict| {
        verdict
            .refusal
            .map(|refusal| format!("{}: {refusal}", verdict.check.name))
    })
}

/// The route a plan carries, resolving it on demand for a payload built
/// before #1855. Production always carries one; this fallback is what keeps an
/// older daemon payload launchable.
pub fn route_of(plan: &LaunchPlan, ambient: &Ambient) -> ResolvedRoute {
    plan.route.clone().unwrap_or_else(|| resolve(plan, ambient))
}

/// Resolve the route against the process's cached catalog.
pub fn resolve(plan: &LaunchPlan, ambient: &Ambient) -> ResolvedRoute {
    resolve_with_catalog(
        plan,
        ambient,
        &crate::openrouter_catalog::catalog_cached_or_embedded(),
        crate::server_settings::per_turn_effort_enabled(),
    )
}

/// What one backend's branch contributes before the shared policies are keyed
/// on it.
struct BackendPlan {
    upstreams: Vec<UpstreamRoute>,
    slots: SlotModels,
    allowlist: Vec<String>,
    discovery: DiscoveryPolicy,
    /// A backend-level context decision that is not keyed on one wire id. The
    /// Codex bridge's process-wide override is the only one today: Claude Code
    /// 2.1.223+ makes its context override process-wide, so the catalog has to
    /// prove one common ceiling across every switchable Codex row.
    context: Option<ContextPolicy>,
}

/// The direct route: one descriptor, one key, clud not in the request path.
fn direct_plan(
    provider: ModelProvider,
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
    allowlist: Vec<String>,
    ambient: &Ambient,
) -> BackendPlan {
    let descriptor = crate::provider_registry::descriptor_for(provider)
        .expect("a direct route always has a descriptor");
    let slots = descriptor_slots(descriptor, selection, &allowlist, ambient);
    // #1257: discovery only adds rows and cannot subtract them (DD-054), so a
    // constrained launch does not ask for it at all rather than advertising a
    // set the allowlist would then have to be enforced against afterwards.
    let discovery = if descriptor.enable_gateway_model_discovery && allowlist.is_empty() {
        DiscoveryPolicy::On
    } else {
        DiscoveryPolicy::Off
    };
    BackendPlan {
        upstreams: vec![upstream_for(
            provider,
            ambient
                .direct_upstream_url
                .as_deref()
                .unwrap_or(descriptor.anthropic_base_url),
            CredentialSource::Vault(provider),
        )],
        slots,
        allowlist,
        discovery,
        context: None,
    }
}

/// The unified gateway: one local gateway in front of the harness, holding a
/// route per Anthropic-compatible provider plus the native Claude and Codex
/// upstreams.
fn unified_plan(
    provider: ModelProvider,
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
    allowlist: Vec<String>,
    ambient: &Ambient,
) -> BackendPlan {
    // Which of these can serve right now is a launch-time credential probe,
    // deliberately not a resolution input.
    let mut upstreams = vec![upstream_for(
        ModelProvider::Claude,
        crate::codex_bridge::ANTHROPIC_MESSAGES_BASE_URL,
        CredentialSource::Harness,
    )];
    for descriptor in crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS {
        upstreams.push(upstream_for(
            descriptor.provider,
            descriptor.anthropic_base_url,
            CredentialSource::Vault(descriptor.provider),
        ));
    }
    upstreams.push(upstream_for(
        ModelProvider::Codex,
        crate::codex_bridge::ANTHROPIC_MESSAGES_BASE_URL,
        CredentialSource::CodexAuth,
    ));
    // The slot decision is the direct route's, made by the same function:
    // divergence #1 is that today's gateway copy does not make it at all.
    let slots = match crate::provider_registry::descriptor_for(provider) {
        Some(descriptor) => descriptor_slots(descriptor, selection, &allowlist, ambient),
        None => pinned_slots(selection, &allowlist, ambient),
    };
    let discovery = if allowlist.is_empty() {
        DiscoveryPolicy::On
    } else {
        DiscoveryPolicy::Filtered(allowlist.clone())
    };
    BackendPlan {
        upstreams,
        slots,
        allowlist,
        discovery,
        context: None,
    }
}

/// The Codex translation bridge behind the Claude harness.
fn codex_bridge_plan(plan: &LaunchPlan) -> BackendPlan {
    let selection = plan.model_selection.as_ref();
    let main = crate::codex_model::ModelSpec::parse(
        selection
            .and_then(|selection| selection.wire_model.as_deref())
            .or(plan.codex_model.as_deref())
            .unwrap_or_default(),
    )
    .ok()
    .map(|spec| spec.model);
    let slots = SlotModels {
        main: main.as_deref().and_then(SlotModel::of),
        opus: SlotModel::of(CODEX_BRIDGE_OPUS_MODEL),
        sonnet: SlotModel::of(CODEX_BRIDGE_SONNET_MODEL),
        ..SlotModels::default()
    };
    BackendPlan {
        upstreams: vec![upstream_for(
            ModelProvider::Codex,
            crate::codex_bridge::ANTHROPIC_MESSAGES_BASE_URL,
            CredentialSource::CodexAuth,
        )],
        slots,
        allowlist: codex_bridge_allowlist(plan),
        discovery: DiscoveryPolicy::On,
        context: Some(ContextPolicy {
            max_tokens: crate::provider_catalog::common_claude_context_tokens(ModelProvider::Codex),
            auto_compact_window: None,
        }),
    }
}

/// The DeepSeek-native harness: `dsh` owns the request path, and clud hands it
/// a key in the environment plus, for OpenRouter, a `--patch` overlay.
fn deepseek_native_plan(
    provider: ModelProvider,
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
) -> BackendPlan {
    let key_provider = match provider {
        ModelProvider::OpenRouter => ModelProvider::OpenRouter,
        _ => ModelProvider::DeepSeek,
    };
    let var = crate::dsh_harness::key_env_for(key_provider)
        .unwrap_or("DEEPSEEK_API_KEY")
        .to_string();
    // dsh resolves its OpenRouter route from its own bundled catalog, so the
    // model clud names in the `--patch` overlay is `openrouter_model`'s. On
    // its DeepSeek route it reads the selected model directly.
    let main = match provider {
        ModelProvider::OpenRouter => crate::dsh_harness::openrouter_model(selection),
        _ => selection.and_then(|selection| selection.wire_model.as_deref()),
    }
    .map(str::to_string);
    BackendPlan {
        upstreams: vec![upstream_for(
            key_provider,
            crate::provider_registry::descriptor_for(key_provider)
                .map(|descriptor| descriptor.anthropic_base_url)
                .unwrap_or_default(),
            CredentialSource::Env(var),
        )],
        slots: SlotModels {
            main: main.as_deref().and_then(SlotModel::of),
            ..SlotModels::default()
        },
        allowlist: Vec::new(),
        discovery: DiscoveryPolicy::Off,
        context: None,
    }
}

/// OpenCode shares the Messages wire protocol with the descriptor-backed routes.
fn opencode_native_plan(
    provider: ModelProvider,
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
    ambient: &Ambient,
) -> BackendPlan {
    let descriptor = crate::provider_registry::descriptor_for(provider);
    let base = ambient
        .opencode_base_url
        .as_deref()
        .or(ambient.direct_upstream_url.as_deref())
        .unwrap_or_else(|| {
            descriptor.map_or(
                crate::codex_bridge::ANTHROPIC_MESSAGES_BASE_URL,
                |descriptor| descriptor.anthropic_base_url,
            )
        });
    BackendPlan {
        upstreams: vec![upstream_for(
            provider,
            base,
            descriptor.map_or(CredentialSource::Harness, |_| {
                CredentialSource::Vault(provider)
            }),
        )],
        slots: SlotModels {
            main: crate::opencode_harness::wire_model(provider, selection).and_then(SlotModel::of),
            ..SlotModels::default()
        },
        allowlist: Vec::new(),
        discovery: DiscoveryPolicy::Off,
        context: None,
    }
}

/// Resolve the route from explicit inputs, so a test never needs the daemon's
/// catalog cache or the served server settings.
pub fn resolve_with_catalog(
    plan: &LaunchPlan,
    ambient: &Ambient,
    catalog: &crate::openrouter_catalog::Catalog,
    per_turn_effort_enabled: bool,
) -> ResolvedRoute {
    let backend = backend_for(plan);
    let provider = plan.model_provider();
    let selection = plan.model_selection.as_ref();
    let planned = match backend {
        RouteBackend::Direct => {
            direct_plan(provider, selection, plan.allowed_models.clone(), ambient)
        }
        RouteBackend::Unified => {
            unified_plan(provider, selection, plan.allowed_models.clone(), ambient)
        }
        RouteBackend::CodexBridge => codex_bridge_plan(plan),
        RouteBackend::DeepSeekNative => deepseek_native_plan(provider, selection),
        RouteBackend::OpenCodeNative => opencode_native_plan(provider, selection, ambient),
        RouteBackend::Native => BackendPlan {
            upstreams: Vec::new(),
            slots: SlotModels::default(),
            allowlist: Vec::new(),
            discovery: DiscoveryPolicy::Off,
            context: None,
        },
    };
    let BackendPlan {
        upstreams,
        slots,
        allowlist,
        discovery,
        context,
    } = planned;
    // The main wire id the context and effort policies are keyed on: the
    // slot's own spelling, which is what every renderer actually writes.
    let main_wire = slots.main.as_ref().map(|slot| slot.wire_id.clone());
    let mut route = ResolvedRoute {
        backend,
        provider,
        upstreams,
        slots,
        allowlist,
        discovery,
        context: context.unwrap_or_else(|| context_policy(main_wire.as_deref())),
        effort: EffortPolicy {
            // Only OpenRouter publishes the underlying gate. DeepSeek and Kimi
            // direct routes hit their own vendor endpoints, so the claim must
            // never reach them (#1528).
            per_turn_capability: (provider == ModelProvider::OpenRouter)
                .then_some(main_wire.as_deref())
                .flatten()
                .and_then(|wire| {
                    crate::foreground_runtime::per_turn_effort_capability(
                        catalog,
                        wire,
                        per_turn_effort_enabled,
                    )
                }),
        },
        upstream_routing: provider_routing(plan),
        redaction: crate::secret_redaction::policy::Policy::resolve(
            provider,
            plan.model_selection
                .as_ref()
                .and_then(|selection| selection.wire_model.as_deref()),
            plan.redact_secrets,
            provider_routing(plan)
                .as_ref()
                .is_some_and(|routing| routing.data_collection.as_deref() == Some("allow")),
        ),
        timeout_ms: match backend {
            RouteBackend::Direct => DIRECT_TIMEOUT_MS.parse().ok(),
            RouteBackend::Unified | RouteBackend::CodexBridge => BRIDGE_TIMEOUT_MS.parse().ok(),
            RouteBackend::Native | RouteBackend::DeepSeekNative | RouteBackend::OpenCodeNative => {
                None
            }
        },
        scrub: scrub_list(backend, provider),
        checks: Vec::new(),
    };
    route.checks = evaluate(&route, catalog)
        .into_iter()
        .map(|verdict| verdict.check)
        .collect();
    route
}

/// One environment variable a renderer writes, and whether an ambient value
/// already present wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvEntry {
    pub key: String,
    pub value: String,
    /// `true` keeps an ambient value the user already set: the renderer only
    /// fills the key when the child environment does not name it at all.
    pub default: bool,
}

/// A route rendered into a child environment. Every renderer produces one of
/// these, so the scrub happens in exactly one place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvOverlay {
    pub scrub: Vec<ScrubKey>,
    pub entries: Vec<EnvEntry>,
}

impl EnvOverlay {
    pub fn new(scrub: &[ScrubKey]) -> Self {
        Self {
            scrub: scrub.to_vec(),
            entries: Vec::new(),
        }
    }

    /// Write `key`, replacing any existing spelling of it.
    pub fn set(&mut self, key: &str, value: &str) {
        self.entries.push(EnvEntry {
            key: key.to_string(),
            value: value.to_string(),
            default: false,
        });
    }

    /// Write `key` only when the child environment does not already name it,
    /// so an explicit ambient value keeps winning (the DD-059 precedence the
    /// direct overlay has always given `API_TIMEOUT_MS` and friends).
    pub fn set_default(&mut self, key: &str, value: &str) {
        self.entries.push(EnvEntry {
            key: key.to_string(),
            value: value.to_string(),
            default: true,
        });
    }

    /// Apply the route to a child environment: scrub exactly the keys this
    /// route owns, then write its entries in order. The overlay is a value, so
    /// a caller can render a route without touching any environment at all.
    pub fn apply(&self, env: &mut Vec<(String, String)>) {
        for key in &self.scrub {
            env.retain(|(candidate, _)| !key.matches(candidate));
        }
        for entry in &self.entries {
            if entry.default && env.iter().any(|(key, _)| os_env_key_eq(key, &entry.key)) {
                continue;
            }
            env.retain(|(key, _)| !os_env_key_eq(key, &entry.key));
            env.push((entry.key.clone(), entry.value.clone()));
        }
    }
}

/// Read the secret a route's upstream needs. The route itself never carries
/// one: it names where the credential comes from, and this is the single read.
pub fn read_credential(
    source: &CredentialSource,
    store: &dyn crate::provider_auth::SecretStore,
) -> Result<Option<String>, crate::provider_auth::SecretStoreError> {
    match source {
        CredentialSource::Vault(_) => store.get(),
        CredentialSource::Env(var) => Ok(std::env::var(var).ok()),
        CredentialSource::Harness | CredentialSource::CodexAuth => Ok(None),
    }
}

/// The direct route's child environment: the descriptor's base URL, the vault
/// secret, and the slot, discovery, context, effort and timeout decisions the
/// resolver already made. Nothing here decides anything.
pub fn render_direct_env(route: &ResolvedRoute, secret: &str) -> EnvOverlay {
    let descriptor = crate::provider_registry::descriptor_for(route.provider)
        .expect("a direct route always has a descriptor");
    let mut overlay = EnvOverlay::new(&route.scrub);
    if let Some(upstream) = route.upstreams.first() {
        overlay.set("ANTHROPIC_BASE_URL", &upstream.base_url);
    }
    overlay.set("ANTHROPIC_AUTH_TOKEN", secret);
    for (key, slot) in [
        ("ANTHROPIC_MODEL", &route.slots.main),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", &route.slots.opus),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", &route.slots.sonnet),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", &route.slots.haiku),
        ("CLAUDE_CODE_SUBAGENT_MODEL", &route.slots.subagent),
    ] {
        if let Some(slot) = slot {
            overlay.set(key, &slot.wire_id);
        }
    }
    if let Some(fable) = &route.slots.fable {
        overlay.set("ANTHROPIC_DEFAULT_FABLE_MODEL", &fable.wire_id);
    }
    if descriptor.explicitly_empty_anthropic_api_key {
        overlay.set("ANTHROPIC_API_KEY", "");
    }
    if matches!(route.discovery, DiscoveryPolicy::On) {
        overlay.set("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");
    }
    // Pushed, not defaulted: the compact threshold is clud's reviewed value
    // for this wire id. The window below keeps an ambient user value.
    if let Some(window) = route.context.auto_compact_window {
        overlay.set("CLAUDE_CODE_AUTO_COMPACT_WINDOW", &window.to_string());
    }
    if let Some(tokens) = route.context.max_tokens {
        overlay.set_default("CLAUDE_CODE_MAX_CONTEXT_TOKENS", &tokens.to_string());
    }
    if let Some(capability) = &route.effort.per_turn_capability {
        overlay.set_default("CLAUDE_CODE_MODEL_CAPABILITIES", capability);
    }
    if let Some(millis) = route.timeout_ms {
        overlay.set_default("API_TIMEOUT_MS", &millis.to_string());
    }
    overlay
}

/// The values the Codex bridge config needs that are a *request* rather than a
/// routing decision: which model and effort the launch asked to default to,
/// and the cli id Claude Code's `/model` will report back.
pub struct CodexBridgeRequest<'a> {
    pub default_model: Option<crate::codex_model::ModelSpec>,
    pub selected_model_cli_id: Option<&'a str>,
}

/// The Codex bridge's configuration, read from the route. Every field is a
/// value the route already decided; `max_body_bytes` and friends keep their
/// `BridgeConfig` defaults at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexBridgeRender {
    pub default_model: Option<crate::codex_model::ModelSpec>,
    pub selected_model_cli_id: Option<String>,
    pub allowed_models: Vec<String>,
}

/// Render the bridge configuration from the route plus the launch's request.
/// The boundary is the route's: a pinned launch widens it with the two rows
/// the bridge injects itself, and that widening was decided once in the
/// resolver.
pub fn render_codex_bridge(
    route: &ResolvedRoute,
    request: CodexBridgeRequest<'_>,
) -> CodexBridgeRender {
    CodexBridgeRender {
        default_model: request.default_model,
        selected_model_cli_id: request.selected_model_cli_id.map(str::to_string),
        allowed_models: route.allowlist.clone(),
    }
}

/// Whether the child environment forbids the non-essential traffic model
/// discovery needs. Every gateway route refuses the launch rather than
/// silently serving an unfiltered catalog (#998).
pub fn discovery_is_disabled(env: &[(String, String)]) -> bool {
    env.iter().any(|(key, value)| {
        os_env_key_eq(key, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
            && (value.trim() == "1" || value.trim().eq_ignore_ascii_case("true"))
    })
}

/// The Codex-via-Claude bridge's child environment: the bridge's loopback
/// address and bearer, the two role aliases the bridge can actually serve,
/// discovery, the process-wide context ceiling and the bridge timeout.
pub fn render_codex_bridge_env(
    route: &ResolvedRoute,
    base_url: &str,
    bearer_token: &str,
) -> EnvOverlay {
    let mut overlay = EnvOverlay::new(&route.scrub);
    overlay.set("ANTHROPIC_BASE_URL", base_url);
    overlay.set("ANTHROPIC_AUTH_TOKEN", bearer_token);
    // The built-in `opus` and `sonnet` aliases also select models for Claude
    // Code workflows and subagents. The bridge cannot serve Anthropic model
    // IDs, so bind them to honest, advertised Codex rows. Haiku is
    // deliberately left alone: it is used for Claude Code side work and has no
    // corresponding Codex tier policy.
    for (key, name_key, slot) in [
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            &route.slots.opus,
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            &route.slots.sonnet,
        ),
    ] {
        if let Some(slot) = slot {
            overlay.set(key, &slot.wire_id);
            if let Some(name) = &slot.display_name {
                overlay.set(name_key, name);
            }
        }
    }
    if matches!(
        route.discovery,
        DiscoveryPolicy::On | DiscoveryPolicy::Filtered(_)
    ) {
        overlay.set("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");
    }
    if let Some(tokens) = route.context.max_tokens {
        overlay.set("CLAUDE_CODE_MAX_CONTEXT_TOKENS", &tokens.to_string());
    }
    if let Some(millis) = route.timeout_ms {
        overlay.set_default("API_TIMEOUT_MS", &millis.to_string());
    }
    overlay
}

/// The unified gateway's configuration, built from the route's upstreams.
///
/// `codex_available` is a credential probe, not a routing decision: it says
/// whether Codex's own auth resolves at this moment, which is why it stays an
/// input rather than something the route asserts.
pub fn render_unified_config(
    route: &ResolvedRoute,
    codex_available: bool,
    key_for: &dyn Fn(ModelProvider) -> Option<String>,
) -> crate::codex_bridge::UnifiedGatewayConfig {
    // Insertion order is part of the gateway's Debug view and of the order its
    // `/v1/models` rows appear in, so the two Anthropic-compatible providers
    // keep the order this gateway has always built them in.
    let mut config = crate::codex_bridge::UnifiedGatewayConfig::new(
        key_for(ModelProvider::DeepSeek),
        codex_available,
    )
    .with_openrouter(key_for(ModelProvider::OpenRouter));
    for upstream in &route.upstreams {
        if matches!(
            upstream.provider,
            ModelProvider::Claude
                | ModelProvider::Codex
                | ModelProvider::DeepSeek
                | ModelProvider::OpenRouter
        ) {
            continue;
        }
        config = config.with_route(upstream.provider, key_for(upstream.provider));
    }
    // #1863: the gateway owns the request path, so it honours --provider-only
    // itself instead of refusing it.
    config.with_upstream_routing(route.upstream_routing.clone())
}

/// The unified gateway's child environment.
///
/// Every slot the route decided is written, pinned or not: the resolver gives
/// an unpinned launch the descriptor's role mappings and the served subagent
/// model, and gives a pinned one the pin. Until #1861 this block was written
/// only when the launch was pinned, so an unpinned gateway launch ignored
/// `role_models` and `server_settings::provider_subagent_model` entirely
/// (divergence #1b).
///
/// A pinned value is the row's *discovery* id, because Claude Code classifies
/// a raw wire id as an unknown provider id and falls back to a built-in
/// Anthropic row -- exactly the spend the pin exists to stop. An admitted
/// ambient subagent is written verbatim instead: the user's spelling already
/// resolved, and clud admits it rather than rewriting it.
pub fn render_unified_env(
    route: &ResolvedRoute,
    base_url: &str,
    bearer_token: &str,
    ambient_custom_headers: Option<&str>,
) -> EnvOverlay {
    let mut overlay = EnvOverlay::new(&route.scrub);
    overlay.set("ANTHROPIC_BASE_URL", base_url);
    // The gateway's own header must come first, and whatever the user already
    // set is kept below it.
    let gateway_header = format!("{UNIFIED_GATEWAY_TOKEN_HEADER}: {bearer_token}");
    let headers = ambient_custom_headers
        .map(|existing| format!("{gateway_header}\n{existing}"))
        .unwrap_or(gateway_header);
    overlay.set("ANTHROPIC_CUSTOM_HEADERS", &headers);
    overlay.set("CLUD_GATEWAY_TOKEN", bearer_token);
    if matches!(
        route.discovery,
        DiscoveryPolicy::On | DiscoveryPolicy::Filtered(_)
    ) {
        overlay.set("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");
    }
    if let Some(millis) = route.timeout_ms {
        overlay.set_default("API_TIMEOUT_MS", &millis.to_string());
    }
    // Divergences #3 and #4, fixed here (#1860): the gateway route used to set
    // neither of these, so the harness clamped every gateway model at its own
    // 200k default and never advertised the per-turn effort capability the
    // direct route advertises for the very same wire id.
    if let Some(window) = route.context.auto_compact_window {
        overlay.set("CLAUDE_CODE_AUTO_COMPACT_WINDOW", &window.to_string());
    }
    if let Some(tokens) = route.context.max_tokens {
        overlay.set_default("CLAUDE_CODE_MAX_CONTEXT_TOKENS", &tokens.to_string());
    }
    if let Some(capability) = &route.effort.per_turn_capability {
        overlay.set_default("CLAUDE_CODE_MODEL_CAPABILITIES", capability);
    }
    for (slot, key) in [
        (&route.slots.opus, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
        (&route.slots.sonnet, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
        (&route.slots.haiku, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
        (&route.slots.fable, "ANTHROPIC_DEFAULT_FABLE_MODEL"),
        (&route.slots.subagent, "CLAUDE_CODE_SUBAGENT_MODEL"),
    ] {
        if let Some(slot) = slot {
            let id = if slot.verbatim {
                &slot.wire_id
            } else {
                slot.discovery_id.as_deref().unwrap_or(&slot.wire_id)
            };
            overlay.set(key, id);
        }
    }
    overlay
}

/// The DeepSeek-native harness's render. `dsh` owns its own provider
/// configuration, so the route contributes exactly three things: the model to
/// serve, the environment variable its key travels in, and the base URL of the
/// OpenRouter overlay clud writes for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshRender {
    pub provider: ModelProvider,
    pub wire_model: Option<String>,
    pub key_env: Option<String>,
    pub base_url: Option<String>,
}

pub fn render_dsh(route: &ResolvedRoute) -> DshRender {
    let upstream = route.upstreams.first();
    DshRender {
        provider: upstream.map_or(route.provider, |upstream| upstream.provider),
        wire_model: route.slots.main.as_ref().map(|slot| slot.wire_id.clone()),
        key_env: route.credential_env().map(str::to_string),
        base_url: upstream.map(|upstream| upstream.base_url.clone()),
    }
}

/// OpenRouter's upstream routing object, when the launch asked for one.
/// Validation of the slugs themselves stays where the refusal is printed.
pub(crate) fn provider_routing(plan: &LaunchPlan) -> Option<ProviderRouting> {
    let slugs = &plan.provider_only;
    // The route records what the launch *asked* for, so a launch check can
    // classify it on every backend. Only OpenRouter can receive the object,
    // and the `provider_only` check refuses the rest before they launch.
    if slugs.is_empty() && !plan.zdr && !plan.no_data_collection {
        return None;
    }
    Some(ProviderRouting {
        only: slugs.to_vec(),
        allow_fallbacks: slugs.is_empty(),
        zdr: plan.zdr,
        data_collection: plan.no_data_collection.then(|| "deny".to_string()),
    })
}

#[cfg(test)]
#[path = "route_plan_tests.rs"]
mod tests;

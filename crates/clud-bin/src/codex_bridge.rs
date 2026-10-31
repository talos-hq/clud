//! Authenticated loopback bridge used by Codex-provider launches through the
//! Claude harness (issue #626).

use crate::anthropic_usage::{Provider as AnthropicUsageProvider, StreamingUsage};
use crate::backend::ModelProvider;
use crate::bridge_log::{unix_ms, BridgeLog};
use crate::cache_health::{CacheHealth, CacheHealthTracker, TokenUsage, UsageTotals};
use crate::codex_history::{ConversationKey, ConversationRoute, ConversationStore, HistoryLimits};
use crate::codex_model::ModelSpec;
use crate::codex_pipeline::{Pipeline, PipelineError, ProviderFailure};
use crate::codex_sse::InBandFailure;
use crate::codex_upstream::{
    ApiKeyCredentials, CredentialSource, FailureClass, ResolvedCredentials, UpstreamClient,
    UpstreamConfig, UpstreamError, UpstreamFailure, UpstreamTarget,
};
use crate::failover::{FailoverLadder, FailoverRung};
use crate::provider_catalog;
use crate::provider_registry::AnthropicCompatRoute;
use crate::route_health::{RouteLedger, RouteState, RouteVerdict};
use crate::secret_redaction::stream::SharedSession;
use base64::Engine as _;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The bridge must not be stricter than the endpoint it impersonates. Claude
/// Code sizes its requests against the real Anthropic API, so a cap below what
/// that API accepts turns a legitimate request into a bridge-only `413` that
/// looks like a client bug. A single base64 screenshot already exceeds the
/// fixture-era 1 MiB; see `a_representative_request_fits_the_body_cap`.
/// What the operator log and the stderr banner say when the exhaustion
/// arrived in-band. The client already received the translator's own
/// synthesized `error` frame; this is the copy for the human watching the
/// terminal.
const IN_BAND_QUOTA_MESSAGE: &str = "upstream account quota exhausted mid-stream -- check your plan usage, or switch providers with --claude";

const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_MAX_HEADER_BYTES: usize = 32 * 1024;
/// Header reads stay on a short absolute deadline: it is the slowloris
/// defence, and a well-behaved client sends its headers in one segment.
const DEFAULT_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// Body reads get their own budget. A large transcript or image upload is
/// legitimately slower than a header, but is still bounded.
const DEFAULT_BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// Streaming responses are governed by an *idle* timeout between frames, not
/// by a total deadline: a model that thinks for ten minutes before its first
/// token is healthy, whereas a socket that accepts no bytes for five minutes
/// is not. Phase 3 replaces the fixture frames but keeps this primitive.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Maximum wait for the first upstream byte. It is deliberately generous:
/// lengthy model reasoning is healthy, but a timeout here still leaves the
/// downstream status uncommitted.
const DEFAULT_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(300);
/// Claude Code issues several requests at once: the foreground turn plus
/// background side-model calls and any subagents. The bound still exists, but
/// exceeding it now queues in the listen backlog rather than failing.
///
/// Set to 2, the smallest bound under which two connections can be in flight
/// at once: one slot for the foreground turn and one for anything else, be it
/// a background side-model call or a subagent. At 1, any second request had
/// to wait for the foreground turn's connection to release the sole worker,
/// so a subagent could not put a frame on the wire until the turn finished
/// and the wait was indistinguishable from a hang. Two is not sufficient in
/// every case -- a subagent can still queue behind a background side-model
/// call, and if that happens the next question is which request it queued
/// behind, where the answer may be a separate admission lane rather than a
/// larger pool. The only guarantee the bound makes is that a second
/// connection can be in flight at the same time as the first.
///
/// The footprint argument from #778 still bounds the total: forensics
/// captured 15 bridges constructed inside one millisecond in a single pid,
/// each advertising a 16-worker ceiling; the ceilings are lazy, but the
/// advertised total is the number an operator has to reason about when the
/// host is already saturated. At 2 (#989) that arithmetic is 15 x 2 = 30
/// advertised workers instead of 15 x 16 = 240, so the per-bridge footprint
/// stays flat and small no matter how many bridges a process stands up.
/// Excess connections wait in the listen backlog until a worker becomes
/// available or the bridge shuts down. This keeps local contention out of the
/// harness-visible API surface without accepting or buffering their bodies.
const DEFAULT_MAX_CONCURRENCY: usize = 2;
const ACCEPT_POLL: Duration = Duration::from_millis(5);
/// Upper bound on a single blocking read. Reads are resumed until their phase
/// deadline expires; the cap exists so a worker parked on a quiet socket still
/// observes shutdown promptly instead of holding teardown for its full budget.
const READ_POLL: Duration = Duration::from_millis(100);
/// Pseudo-status meaning "stop without replying": the connection is being torn
/// down, so there is nobody left to receive an error document.
const ABANDON: u16 = 0;

type ActiveConnections = Arc<Mutex<HashMap<usize, TcpStream>>>;
type SharedBridgeLog = Arc<Mutex<BridgeLog>>;
type SharedCacheHealth = Arc<Mutex<CacheHealthTracker>>;
/// Launch-wide terminal usage is deliberately separate from the per-agent
/// cache-health windows. Compaction and sibling conversations reset their
/// safety-fuse state, but must never make a user's visible totals decrease.
#[derive(Default)]
struct StatusUsageState {
    writer: Option<Arc<crate::toast::statusline::StatusStateWriter>>,
    totals: UsageTotals,
    /// Bumped by `/clear`; the status line shows one conversation (#2045).
    generation: u64,
}

type SharedStatusUsage = Arc<Mutex<StatusUsageState>>;

pub(crate) const ANTHROPIC_MESSAGES_BASE_URL: &str = "https://api.anthropic.com";
pub const UNIFIED_GATEWAY_TOKEN_HEADER: &str = "X-Clud-Gateway-Token";

/// A launch-scoped multiplexer configuration. Secret material is intentionally
/// opaque in Debug output and never reaches launch plans or daemon wire state.
#[derive(Clone)]
pub struct UnifiedGatewayConfig {
    /// One entry per Anthropic-compatible provider (a `provider_registry`
    /// descriptor) whose key this launch holds. A provider without an entry is
    /// absent from discovery and refused if a stale picker selects it.
    anthropic_routes: Vec<AnthropicCompatRoute>,
    /// Test-only upstream replacements, kept apart from the routes so a key
    /// added after an override still lands on the overridden URL.
    route_base_url_overrides: Vec<(ModelProvider, String)>,
    codex_available: bool,
    anthropic_base_url: String,
    /// Ordered fallback routes. Empty by default, so a launch that did not ask
    /// for failover keeps exactly today's behavior and today's spend.
    failover: FailoverLadder,
    /// `--provider-only` (#1863): the OpenRouter upstream routing object every
    /// request forwarded to OpenRouter must carry. `None` leaves OpenRouter's
    /// own routing alone, which is the pre-#1863 behavior everywhere.
    upstream_routing: Option<crate::route_plan::ProviderRouting>,
    redaction: Option<SharedSession>,
    redaction_override: Option<bool>,
    launch_model: Option<(ModelProvider, String)>,
    /// Which routes can serve right now. Shared across connections because a
    /// route drained on one turn must stay drained for the next one, and
    /// launch-scoped because a wedged account in this session must never
    /// suppress a route in another.
    route_ledger: Arc<Mutex<RouteLedger>>,
}

impl UnifiedGatewayConfig {
    pub fn new(deepseek_api_key: Option<String>, codex_available: bool) -> Self {
        Self {
            anthropic_routes: Vec::new(),
            route_base_url_overrides: Vec::new(),
            codex_available,
            anthropic_base_url: ANTHROPIC_MESSAGES_BASE_URL.to_string(),
            failover: FailoverLadder::default(),
            upstream_routing: None,
            redaction: None,
            redaction_override: None,
            launch_model: None,
            route_ledger: Arc::new(Mutex::new(RouteLedger::new())),
        }
        .with_route(ModelProvider::DeepSeek, deepseek_api_key)
    }

    /// OpenRouter's key, when the launch has one. Absent leaves the route out
    /// of discovery entirely rather than advertising a row that cannot serve.
    pub fn with_openrouter(self, api_key: Option<String>) -> Self {
        self.with_route(ModelProvider::OpenRouter, api_key)
    }

    /// Set (or, with `None`, remove) one Anthropic-compatible provider's
    /// route. A provider with no registry descriptor has no direct route and
    /// is ignored: Claude is served natively and Codex through translation.
    pub fn with_route(mut self, provider: ModelProvider, api_key: Option<String>) -> Self {
        self.anthropic_routes
            .retain(|route| route.provider != provider);
        let Some(descriptor) = crate::provider_registry::descriptor_for(provider) else {
            return self;
        };
        if let Some(api_key) = api_key {
            let base_url = self
                .base_url_override(provider)
                .unwrap_or(descriptor.anthropic_base_url)
                .to_string();
            self.anthropic_routes.push(AnthropicCompatRoute {
                provider,
                base_url,
                api_key,
            });
        }
        self
    }

    fn base_url_override(&self, provider: ModelProvider) -> Option<&str> {
        self.route_base_url_overrides
            .iter()
            .find(|(candidate, _)| *candidate == provider)
            .map(|(_, url)| url.as_str())
    }

    fn override_route_base_url(&mut self, provider: ModelProvider, base_url: String) {
        self.route_base_url_overrides
            .retain(|(candidate, _)| *candidate != provider);
        for route in &mut self.anthropic_routes {
            if route.provider == provider {
                route.base_url = base_url.clone();
            }
        }
        self.route_base_url_overrides.push((provider, base_url));
    }

    /// The direct route for `provider`, when this launch holds its key.
    fn route_for(&self, provider: ModelProvider) -> Option<&AnthropicCompatRoute> {
        self.anthropic_routes
            .iter()
            .find(|route| route.provider == provider)
    }

    /// Whether the gateway can serve `provider` at all. The single source for
    /// discovery, the error-path ID list, and dispatch, so the three cannot
    /// disagree about which rows exist.
    fn provider_available(&self, provider: ModelProvider) -> bool {
        match provider {
            // Native Claude IDs pass through untouched and are never
            // advertised as clud rows.
            ModelProvider::Claude => false,
            ModelProvider::Codex => self.codex_available,
            other => self.route_for(other).is_some(),
        }
    }

    pub fn with_failover(mut self, failover: FailoverLadder) -> Self {
        self.failover = failover;
        self
    }

    /// The upstream routing object this gateway must inject (#1863).
    pub fn with_upstream_routing(
        mut self,
        routing: Option<crate::route_plan::ProviderRouting>,
    ) -> Self {
        self.upstream_routing = routing;
        self
    }

    /// Configure after credentials and test upstream overrides have resolved.
    pub fn with_redaction(mut self, explicit: Option<bool>) -> Result<Self, getrandom::Error> {
        self.redaction_override = explicit;
        self.redaction = if explicit == Some(false) {
            None
        } else {
            let keys = self
                .anthropic_routes
                .iter()
                .map(|route| route.api_key.clone())
                .collect();
            Some(Arc::new(Mutex::new(
                crate::secret_redaction::reversible::Session::new(keys)?,
            )))
        };
        Ok(self)
    }

    /// Keep a user-selected provider wire ID routable without a static catalog row.
    pub fn with_launch_model(mut self, provider: ModelProvider, wire: Option<&str>) -> Self {
        self.launch_model = wire.map(|wire| (provider, wire.to_string()));
        self
    }

    fn redactor_for(&self, provider: ModelProvider, model: Option<&str>) -> Option<&SharedSession> {
        let training_allowed = provider == ModelProvider::OpenRouter
            && self
                .upstream_routing
                .as_ref()
                .is_some_and(|routing| routing.data_collection.as_deref() == Some("allow"));
        crate::secret_redaction::policy::Policy::resolve(
            provider,
            model,
            self.redaction_override,
            training_allowed,
        )
        .enabled
        .then_some(self.redaction.as_ref())
        .flatten()
    }

    /// The body to forward for `provider`: the client's own bytes, or -- for an
    /// OpenRouter route with a `--provider-only` pin -- the same document with
    /// `provider` merged in. A body that is not a JSON object is forwarded
    /// untouched rather than refused: the pin is a routing hint, not a
    /// validation gate, and the upstream's own error is more useful.
    fn routed_body(&self, provider: ModelProvider, body: &[u8]) -> Option<Vec<u8>> {
        if provider != ModelProvider::OpenRouter {
            return None;
        }
        let routing = self.upstream_routing.as_ref()?;
        let mut document: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(body).ok()?;
        document.insert(
            "provider".to_string(),
            serde_json::to_value(routing).expect("routing contains only JSON values"),
        );
        serde_json::to_vec(&serde_json::Value::Object(document)).ok()
    }

    /// Point every unified proxy destination at the deterministic foreground
    /// integration fakes.  The only caller obtains this value through the
    /// debug-and-explicit-opt-in process gate below; production launches never
    /// receive fixture credentials or fixture URLs.
    pub(crate) fn with_integration_test_upstreams(
        mut self,
        upstreams: &UnifiedIntegrationUpstreams,
    ) -> Self {
        // #901: with the test vault active, keys come from what `clud auth`
        // stored there, so a logout really removes its route from discovery.
        if !crate::provider_auth::test_vault_active() {
            self = self
                .with_route(
                    ModelProvider::DeepSeek,
                    Some("clud-test-deepseek-key".to_string()),
                )
                .with_route(
                    ModelProvider::OpenRouter,
                    Some("clud-test-openrouter-key".to_string()),
                );
            if upstreams.kimi_base_url.is_some() {
                self = self.with_route(ModelProvider::Kimi, Some("clud-test-kimi-key".to_string()));
            }
        }
        self.codex_available = true;
        self.anthropic_base_url = upstreams.anthropic_base_url.clone();
        self.override_route_base_url(ModelProvider::DeepSeek, upstreams.deepseek_base_url.clone());
        self.override_route_base_url(
            ModelProvider::OpenRouter,
            upstreams.openrouter_base_url.clone(),
        );
        match &upstreams.kimi_base_url {
            Some(url) => self.override_route_base_url(ModelProvider::Kimi, url.clone()),
            // The Kimi fake is optional; with none, a vault-held Kimi key must
            // not reach the real Moonshot endpoint from a test run.
            None => self = self.with_route(ModelProvider::Kimi, None),
        }
        self
    }

    /// Point the standalone server at explicitly enabled integration fakes.
    pub(crate) fn with_standalone_test_upstreams(
        mut self,
        upstreams: &UnifiedIntegrationUpstreams,
    ) -> Self {
        self.anthropic_base_url = upstreams.anthropic_base_url.clone();
        self.override_route_base_url(
            ModelProvider::OpenRouter,
            upstreams.openrouter_base_url.clone(),
        );
        self
    }

    #[cfg(test)]
    fn with_upstreams(mut self, anthropic_base_url: String, deepseek_base_url: String) -> Self {
        self.anthropic_base_url = anthropic_base_url;
        self.override_route_base_url(ModelProvider::DeepSeek, deepseek_base_url);
        self
    }

    #[cfg(test)]
    fn with_openrouter_upstream(mut self, base_url: String) -> Self {
        self.override_route_base_url(ModelProvider::OpenRouter, base_url);
        self
    }

    #[cfg(test)]
    fn with_route_upstream(mut self, provider: ModelProvider, base_url: String) -> Self {
        self.override_route_base_url(provider, base_url);
        self
    }

    /// Record a verdict against a route and report the state it leaves behind.
    /// A poisoned ledger degrades to "no health known" rather than killing the
    /// turn: failover is a recovery mechanism and must not become a new way to
    /// fail.
    fn record_route(&self, route: ConversationRoute, verdict: RouteVerdict) -> Option<RouteState> {
        let mut ledger = self.route_ledger.lock().ok()?;
        Some(ledger.record(route, verdict, Instant::now()))
    }

    fn record_route_success(&self, route: ConversationRoute) {
        if let Ok(mut ledger) = self.route_ledger.lock() {
            ledger.record_success(route);
        }
    }

    /// The next rung below `after` that the ledger says can serve.
    fn next_rung(&self, after: Option<&str>, current: ConversationRoute) -> Option<FailoverRung> {
        let ledger = self.route_ledger.lock().ok()?;
        self.failover
            .next_available_excluding(after, Some(current), &ledger, Instant::now())
            .cloned()
    }
}

impl fmt::Debug for UnifiedGatewayConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnifiedGatewayConfig")
            .field(
                "configured_routes",
                &self
                    .anthropic_routes
                    .iter()
                    .map(|route| route.provider.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("codex_available", &self.codex_available)
            .field("failover_rungs", &self.failover.rungs().len())
            .finish()
    }
}

#[derive(Clone, Debug)]
enum GatewayMode {
    Codex,
    Unified(Box<UnifiedGatewayConfig>),
}

/// Credential obtained by a standalone gateway for a native Claude request.
/// Kept separate from the credential that authenticates the local client.
pub enum StandaloneClaudeCredential {
    ApiKey(String),
    Bearer(String),
}

/// Resolves an upstream credential per request, allowing rotating credentials.
/// Failure carries no provider response or secret into the client error.
pub trait StandaloneClaudeCredentialSource: Send + Sync {
    fn credential(&self) -> Option<StandaloneClaudeCredential>;
}

/// Resource and test-seam policy for one bridge launch.
#[derive(Clone)]
pub struct BridgeConfig {
    pub max_body_bytes: usize,
    pub max_header_bytes: usize,
    pub header_timeout: Duration,
    pub body_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub first_frame_timeout: Duration,
    pub max_concurrency: usize,
    /// Default model+effort selection, from `--model` on the launch. `None`
    /// keeps the built-in default. A request that names its own model still
    /// wins over this.
    default_model: Option<ModelSpec>,
    selected_model_cli_id: Option<String>,
    /// Every model a request may name on this bridge (#1257). Empty means
    /// unconstrained -- byte-for-byte the pre-#1257 behavior. The
    /// codex-via-claude route folds in its own injected role rows when it
    /// builds this (DD-038's substitutions are inside the boundary by
    /// construction).
    allowed_models: Vec<String>,
    gateway_mode: GatewayMode,
    standalone_client_auth: bool,
    standalone_port: u16,
    standalone_client_token: Option<String>,
    standalone_request_ids: Arc<AtomicUsize>,
    standalone_upstreams: Arc<crate::serve_transport::UpstreamSockets>,
    standalone_claude_credential: Option<Arc<dyn StandaloneClaudeCredentialSource>>,
    history_limits: HistoryLimits,
    log_path: Option<std::path::PathBuf>,
    log_max_bytes: usize,
    test_upstream_url: Option<String>,
    status_usage: SharedStatusUsage,
    /// How many `POST /v1/messages` turns the harness has asked this bridge to
    /// serve. Shared with [`BridgeHandle`] so a launch that exited without ever
    /// asking can be told apart from one that asked and was refused (#998).
    turn_requests: Arc<AtomicUsize>,
    /// A committed in-band auth failure cannot replay its turn. The next
    /// request forces a credential check before sending new model input.
    auth_recheck: Arc<Mutex<Option<UpstreamTarget>>>,
    #[cfg(test)]
    request_hold: Duration,
    #[cfg(test)]
    frame_hold: Duration,
    #[cfg(test)]
    admission_notifier: Option<std::sync::mpsc::SyncSender<()>>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            header_timeout: DEFAULT_HEADER_TIMEOUT,
            body_timeout: DEFAULT_BODY_TIMEOUT,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            first_frame_timeout: DEFAULT_FIRST_FRAME_TIMEOUT,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            default_model: None,
            selected_model_cli_id: None,
            allowed_models: Vec::new(),
            gateway_mode: GatewayMode::Codex,
            standalone_client_auth: false,
            standalone_port: 0,
            standalone_client_token: None,
            standalone_request_ids: Arc::new(AtomicUsize::new(0)),
            standalone_upstreams: Arc::new(crate::serve_transport::UpstreamSockets::default()),
            standalone_claude_credential: None,
            history_limits: HistoryLimits::default(),
            log_path: default_bridge_log_path(),
            log_max_bytes: crate::bridge_log::DEFAULT_MAX_BYTES,
            test_upstream_url: test_upstream_override_from_process(),
            status_usage: Arc::new(Mutex::new(StatusUsageState::default())),
            turn_requests: Arc::new(AtomicUsize::new(0)),
            auth_recheck: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            request_hold: Duration::ZERO,
            #[cfg(test)]
            frame_hold: Duration::ZERO,
            #[cfg(test)]
            admission_notifier: None,
        }
    }
}

impl BridgeConfig {
    /// Pin the selection used when a request carries no model of its own.
    pub fn with_default_model(mut self, model: Option<ModelSpec>) -> Self {
        self.default_model = model;
        self
    }

    /// Catalog row selected at launch. The discovery ID for this row must
    /// retain the resolved launch wire ID, including an explicit --model pin.
    pub fn with_selected_model_cli_id(mut self, cli_id: Option<String>) -> Self {
        self.selected_model_cli_id = cli_id;
        self
    }

    /// Constrain every request's model to the launch's allowlist (#1257).
    /// Empty leaves the bridge unconstrained.
    pub fn with_allowed_models(mut self, allowed: Vec<String>) -> Self {
        self.allowed_models = allowed;
        self
    }

    /// Turn the existing launch-scoped bridge into the unified multiplexer.
    /// This remains an in-process foreground listener; it is never a daemon.
    pub fn with_unified_gateway(mut self, config: UnifiedGatewayConfig) -> Self {
        self.gateway_mode = GatewayMode::Unified(Box::new(config));
        self
    }

    /// Authenticate independent API clients using their bearer or API key.
    pub fn with_standalone_client_auth(mut self) -> Self {
        self.standalone_client_auth = true;
        self
    }

    /// Set a loopback port and a caller-supplied client token for the API server.
    pub fn with_standalone_listener(
        mut self,
        port: u16,
        token: String,
    ) -> Result<Self, BridgeError> {
        if token.len() < 32 || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            return Err(BridgeError::Settings(
                "client token must contain at least 32 printable non-space ASCII characters"
                    .to_string(),
            ));
        }
        self.standalone_port = port;
        self.standalone_client_token = Some(token);
        self.standalone_client_auth = true;
        Ok(self)
    }

    /// Supply the standalone server's upstream Claude credential source.
    pub fn with_standalone_claude_credential(
        mut self,
        source: Arc<dyn StandaloneClaudeCredentialSource>,
    ) -> Self {
        self.standalone_claude_credential = Some(source);
        self
    }

    /// Bind the existing Responses fixture seam to an already validated
    /// all-provider foreground integration configuration. This avoids a
    /// partial test launch ever treating the real Codex credential source as a
    /// fake upstream.
    pub(crate) fn with_integration_test_codex_upstream(
        mut self,
        upstreams: &UnifiedIntegrationUpstreams,
    ) -> Self {
        self.test_upstream_url = Some(upstreams.codex_base_url.clone());
        self
    }

    #[cfg(test)]
    fn with_test_upstream_url(mut self, url: Option<String>) -> Self {
        self.test_upstream_url = url;
        self
    }

    #[cfg(test)]
    fn with_history_limits(mut self, limits: HistoryLimits) -> Self {
        self.history_limits = limits;
        self
    }

    #[cfg(test)]
    fn with_log_path(mut self, path: std::path::PathBuf) -> Self {
        self.log_path = Some(path);
        self
    }

    #[cfg(test)]
    fn with_log_max_bytes(mut self, max_bytes: usize) -> Self {
        self.log_max_bytes = max_bytes;
        self
    }

    #[cfg(test)]
    fn with_request_hold(mut self, hold: Duration) -> Self {
        self.request_hold = hold;
        self
    }

    /// Delay inserted between streamed frames so a test can observe partial
    /// delivery. Without it, a fixture stream completes too fast to prove the
    /// difference between progressive flushing and one buffered write.
    #[cfg(test)]
    fn with_frame_hold(mut self, hold: Duration) -> Self {
        self.frame_hold = hold;
        self
    }

    #[cfg(test)]
    fn with_admission_notifier(mut self, notifier: std::sync::mpsc::SyncSender<()>) -> Self {
        self.admission_notifier = Some(notifier);
        self
    }
}

impl fmt::Debug for BridgeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BridgeConfig")
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_header_bytes", &self.max_header_bytes)
            .field("header_timeout", &self.header_timeout)
            .field("body_timeout", &self.body_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("first_frame_timeout", &self.first_frame_timeout)
            .field("max_concurrency", &self.max_concurrency)
            .field(
                "default_model",
                &self.default_model.as_ref().map(ModelSpec::display),
            )
            .field("gateway_mode", &self.gateway_mode)
            .field("history_limits", &self.history_limits)
            .field(
                "test_upstream_url",
                &self.test_upstream_url.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

/// Startup/shutdown failures deliberately contain no endpoint or token text.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("failed to bind loopback bridge: {0}")]
    Bind(io::Error),
    #[error("failed to create bridge credentials: {0}")]
    Random(String),
    #[error("failed to start bridge worker: {0}")]
    Spawn(io::Error),
    #[error("bridge worker panicked during shutdown")]
    Join,
    /// The launch named a model or effort that does not parse. Carries the
    /// selector's own message, which names the valid values.
    #[error("{0}")]
    Model(String),
    /// The launch supplied Claude settings that could not be composed with
    /// the bridge's session-local lifecycle hooks.
    #[error("{0}")]
    Settings(String),
    /// Descriptor-backed provider credentials could not be read at the
    /// child-spawn boundary. The error is provider-neutral and secret-free.
    #[error("provider credentials are unavailable at the child-spawn boundary")]
    AnthropicCompatCredentials,
    /// Direct Codex-via-Claude admission failed before the listener or child
    /// exists. The renderer is shared by foreground and daemon launches.
    #[error("{0}")]
    CodexBridgeCredentials(crate::codex_upstream::CodexBridgeCredentialError),
    /// The caller has explicitly disabled the discovery request unified mode
    /// needs, so launching would silently present a misleading picker.
    #[error("this Claude gateway requires model discovery, but CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC is enabled")]
    DiscoveryDisabled,
    /// The launch declared a failover rung the gateway cannot route. Carries
    /// the ladder's own message, which names the offending rung.
    #[error("--failover: {0}")]
    Failover(String),
}

/// Owns the listener, bearer, shutdown signal, and join handle for a launch.
/// Debug output is intentionally structural: even the ephemeral base URL is
/// omitted so error snapshots cannot become an accidental credential map.
pub struct BridgeHandle {
    #[cfg(test)]
    socket_addr: std::net::SocketAddr,
    base_url: String,
    bearer_token: String,
    shutdown: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    conversations: ConversationStore,
    connections: ActiveConnections,
    upstream_sockets: Arc<crate::serve_transport::UpstreamSockets>,
    log: Option<SharedBridgeLog>,
    turn_requests: Arc<AtomicUsize>,
    status_usage: SharedStatusUsage,
    join: Option<JoinHandle<()>>,
}

impl BridgeHandle {
    pub fn start(mut config: BridgeConfig) -> Result<Self, BridgeError> {
        // A cloned configuration must not share shutdown state across listeners.
        config.standalone_upstreams = Arc::new(crate::serve_transport::UpstreamSockets::default());
        // Resolve the bounded remote/local model checks before the listener is
        // handed to Claude. A first /v1/messages request must not spend its
        // short response deadline waiting for Codex app-server model/list.
        if !config.standalone_client_auth {
            let _ = crate::codex_runtime::active_choice();
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.standalone_port))
            .map_err(BridgeError::Bind)?;
        listener.set_nonblocking(true).map_err(BridgeError::Bind)?;
        let socket_addr = listener.local_addr().map_err(BridgeError::Bind)?;
        debug_assert_eq!(socket_addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

        let bearer_token = match &config.standalone_client_token {
            Some(token) => token.clone(),
            None => {
                let mut token_bytes = [0_u8; 32];
                getrandom::fill(&mut token_bytes)
                    .map_err(|error| BridgeError::Random(error.to_string()))?;
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes)
            }
        };
        let mut fallback_bytes = [0_u8; 16];
        getrandom::fill(&mut fallback_bytes)
            .map_err(|error| BridgeError::Random(error.to_string()))?;
        // This is intentionally independent from the bearer: it is safe to
        // send upstream as the root of a headerless conversation but remains
        // stable only for this bridge listener's lifetime.
        let fallback_root = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(fallback_bytes);
        let base_url = format!("http://{socket_addr}");
        let shutdown = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let conversations = ConversationStore::new(config.history_limits);
        let connections = Arc::new(Mutex::new(HashMap::new()));
        let log = config.log_path.clone().map(|path| {
            Arc::new(Mutex::new(BridgeLog::with_max_bytes(
                path,
                config.log_max_bytes,
            )))
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let turn_requests = Arc::clone(&config.turn_requests);
        let status_usage = Arc::clone(&config.status_usage);
        let upstream_sockets = Arc::clone(&config.standalone_upstreams);
        let cache_health = Arc::new(Mutex::new(CacheHealthTracker::default()));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_active = Arc::clone(&active);
        let thread_conversations = conversations.clone();
        let thread_connections = Arc::clone(&connections);
        let thread_log = log.clone();
        let thread_token = bearer_token.clone();
        let thread_fallback_root = fallback_root.clone();
        let thread_cache_health = Arc::clone(&cache_health);
        let join = thread::Builder::new()
            .name("clud-codex-bridge".to_string())
            .spawn(move || {
                let _ = ready_tx.send(());
                serve(
                    listener,
                    config,
                    thread_token,
                    thread_shutdown,
                    thread_active,
                    thread_conversations,
                    thread_connections,
                    thread_log,
                    thread_fallback_root,
                    thread_cache_health,
                )
            })
            .map_err(BridgeError::Spawn)?;
        ready_rx.recv().map_err(|_| BridgeError::Join)?;

        Ok(Self {
            #[cfg(test)]
            socket_addr,
            base_url,
            bearer_token,
            shutdown,
            active,
            conversations,
            connections,
            upstream_sockets,
            log,
            turn_requests,
            status_usage,
            join: Some(join),
        })
    }

    #[cfg(test)]
    pub(crate) fn socket_addr(&self) -> std::net::SocketAddr {
        self.socket_addr
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) fn bearer_token(&self) -> &str {
        &self.bearer_token
    }

    /// How many `POST /v1/messages` turns the harness asked this bridge for.
    /// Zero on a launch that never reached the gateway -- see
    /// `launch_log::silent_bridge_reason` (#998).
    pub(crate) fn turn_requests(&self) -> usize {
        self.turn_requests.load(Ordering::Acquire)
    }

    /// Attach the launch-owned status writer after the bridge starts. This is
    /// intentionally a late binding because foreground startup creates the
    /// bridge before composing Claude's launch-scoped settings file.
    pub(crate) fn set_status_usage_writer(
        &self,
        writer: Arc<crate::toast::statusline::StatusStateWriter>,
    ) {
        self.status_usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .writer = Some(writer);
    }

    #[cfg(test)]
    fn active_requests(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub fn shutdown(&mut self) -> Result<(), BridgeError> {
        let connections = lock_connections(&self.connections);
        self.shutdown.store(true, Ordering::Release);
        self.upstream_sockets.cancel();
        for stream in connections.values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        drop(connections);
        let was_running = self.join.is_some();
        if let Some(join) = self.join.take() {
            join.join().map_err(|_| BridgeError::Join)?;
        }
        if was_running {
            self.conversations.clear();
            if let Some(log) = &self.log {
                let mut log = lock_log(log);
                log.flush();
                if log.has_notable_records() {
                    eprintln!("[clud] codex bridge log: {}", log.path().display());
                }
            }
        }
        Ok(())
    }
}

impl fmt::Debug for BridgeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BridgeHandle")
            .field("active", &self.join.is_some())
            .field("active_requests", &self.active.load(Ordering::Acquire))
            .finish()
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn serve(
    listener: TcpListener,
    config: BridgeConfig,
    bearer_token: String,
    shutdown: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    conversations: ConversationStore,
    connections: ActiveConnections,
    log: Option<SharedBridgeLog>,
    fallback_root: String,
    cache_health: SharedCacheHealth,
) {
    let mut workers = Vec::<JoinHandle<()>>::new();
    let mut next_worker_id = 0_usize;
    // When every slot is busy, decline to *accept*: pending connections wait in
    // the kernel's listen backlog instead of being answered 503. The active
    // worker still has first-frame and stream-idle timeouts; healthy streaming
    // work can run for hours, so queue age must not be a local failure policy.
    let mut full_since: Option<Instant> = None;
    while !shutdown.load(Ordering::Acquire) {
        workers.retain(|worker| !worker.is_finished());
        let limit = config.max_concurrency.max(1);
        if active.load(Ordering::Acquire) >= limit {
            if full_since.is_none() {
                full_since = Some(Instant::now());
            }
            thread::sleep(ACCEPT_POLL);
            continue;
        } else {
            // Reserve before accepting.  The check above is only a hint: an
            // old worker can exit (or a new worker can reserve) between it and
            // this point. Reserving first means no accepted socket is ever
            // discarded because admission raced after `accept`.
            if !reserve_worker(&active, limit) {
                continue;
            }
            match listener.accept() {
                Ok((stream, _peer)) => {
                    // The listener is non-blocking so the accept loop can poll for
                    // shutdown. On Windows an accepted socket inherits that mode,
                    // and a non-blocking socket ignores SO_RCVTIMEO: every read
                    // that outruns the client's next segment returns WouldBlock,
                    // which the readers below classify as a timeout. The result is
                    // an instant 408 for any request whose body lands in a
                    // separate segment from its headers. Restore blocking mode per
                    // connection so the read deadlines are the real bound.
                    if stream.set_nonblocking(false).is_err() {
                        active.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    // `full_since` begins at saturation, before the OS exposes
                    // a readable pending socket, so `wait_ms` is a secret-free
                    // aggregate upper bound rather than a per-client identity.
                    // Only emit events now that an actual queued socket has
                    // been accepted; a lone occupied worker produces neither.
                    if let Some(wait) = full_since.take().map(|started| started.elapsed()) {
                        record_admission_queued(log.as_ref());
                        record_admission_acquired(log.as_ref(), wait);
                    }
                    let shutdown_stream = match stream.try_clone() {
                        Ok(stream) => stream,
                        Err(_) => {
                            active.fetch_sub(1, Ordering::AcqRel);
                            continue;
                        }
                    };
                    let worker_id = next_worker_id;
                    next_worker_id = next_worker_id.wrapping_add(1);
                    {
                        let mut registered = lock_connections(&connections);
                        registered.insert(worker_id, shutdown_stream);
                        if shutdown.load(Ordering::Acquire) {
                            if let Some(stream) = registered.get(&worker_id) {
                                let _ = stream.shutdown(Shutdown::Both);
                            }
                        }
                    }
                    let worker_active = Arc::clone(&active);
                    let worker_conversations = conversations.clone();
                    let worker_connections = Arc::clone(&connections);
                    let worker_shutdown = Arc::clone(&shutdown);
                    let worker_config = config.clone();
                    let worker_token = bearer_token.clone();
                    let worker_log = log.clone();
                    let worker_fallback_root = fallback_root.clone();
                    let worker_cache_health = Arc::clone(&cache_health);
                    match thread::Builder::new()
                        .name("clud-codex-bridge-request".to_string())
                        .spawn(move || {
                            let _guard = ActiveWorker {
                                active: worker_active,
                                connections: worker_connections,
                                worker_id,
                            };
                            handle_connection(
                                stream,
                                &worker_config,
                                &worker_token,
                                &worker_shutdown,
                                &worker_conversations,
                                worker_log.as_ref(),
                                &worker_fallback_root,
                                &worker_cache_health,
                            );
                        }) {
                        Ok(worker) => workers.push(worker),
                        Err(_) => {
                            active.fetch_sub(1, Ordering::AcqRel);
                            lock_connections(&connections).remove(&worker_id);
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    active.fetch_sub(1, Ordering::AcqRel);
                    // There was no pending socket, so any saturation interval
                    // was not an admission queue and must not reach the log.
                    full_since = None;
                    thread::sleep(ACCEPT_POLL);
                }
                Err(_) => {
                    active.fetch_sub(1, Ordering::AcqRel);
                    break;
                }
            }
        }
    }
    drop(listener);
    for worker in workers {
        let _ = worker.join();
    }
}

fn reserve_worker(active: &AtomicUsize, limit: usize) -> bool {
    active
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < limit).then_some(current + 1)
        })
        .is_ok()
}

fn lock_connections(
    connections: &ActiveConnections,
) -> std::sync::MutexGuard<'_, HashMap<usize, TcpStream>> {
    connections
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct ActiveWorker {
    active: Arc<AtomicUsize>,
    connections: ActiveConnections,
    worker_id: usize,
}

impl Drop for ActiveWorker {
    fn drop(&mut self) {
        lock_connections(&self.connections).remove(&self.worker_id);
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_connection(
    mut stream: TcpStream,
    config: &BridgeConfig,
    bearer_token: &str,
    shutdown: &AtomicBool,
    conversations: &ConversationStore,
    log: Option<&SharedBridgeLog>,
    fallback_root: &str,
    cache_health: &SharedCacheHealth,
) {
    #[cfg(test)]
    if let Some(notifier) = &config.admission_notifier {
        let _ = notifier.try_send(());
    }
    #[cfg(test)]
    if !config.request_hold.is_zero() {
        thread::sleep(config.request_hold);
    }
    // Each phase gets its own budget. Sharing one deadline across header read,
    // body read, and response write is what made a real (multi-minute) model
    // response indistinguishable from a stalled socket.
    let header_deadline = Instant::now() + config.header_timeout;
    if stream
        .set_write_timeout(Some(config.header_timeout))
        .is_err()
    {
        return;
    }

    let Some(parsed) = admit_request(
        &mut stream,
        config,
        bearer_token,
        shutdown,
        log,
        header_deadline,
    ) else {
        return;
    };
    let standalone_root =
        (config.standalone_client_auth && parsed.session_id.is_none()).then(|| {
            format!(
                "{}-{}",
                fallback_root,
                config
                    .standalone_request_ids
                    .fetch_add(1, Ordering::Relaxed)
            )
        });
    let conversation_key = conversation_key_from_headers(
        parsed.session_id.as_deref(),
        parsed.agent_id.as_deref(),
        standalone_root.as_deref().unwrap_or(fallback_root),
    );
    let request_conversations = standalone_root
        .as_ref()
        .map(|_| ConversationStore::new(config.history_limits));
    let connection = ConnectionContext {
        config,
        shutdown,
        conversations: request_conversations.as_ref().unwrap_or(conversations),
        log,
        fallback_root,
        cache_health,
    };
    route_request(&mut stream, &connection, parsed, &conversation_key);
}

/// The per-connection state every route handler reads.
struct ConnectionContext<'a> {
    config: &'a BridgeConfig,
    shutdown: &'a AtomicBool,
    conversations: &'a ConversationStore,
    log: Option<&'a SharedBridgeLog>,
    fallback_root: &'a str,
    cache_health: &'a SharedCacheHealth,
}

/// Read the request headers and admit the request: size and credential
/// checks. A refusal is recorded and answered here; `None` means the
/// connection is done.
fn admit_request(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    bearer_token: &str,
    shutdown: &AtomicBool,
    log: Option<&SharedBridgeLog>,
    header_deadline: Instant,
) -> Option<ParsedRequest> {
    let mut parsed = match read_headers(stream, config.max_header_bytes, header_deadline, shutdown)
    {
        Ok(parsed) => parsed,
        Err(ABANDON) => return None,
        Err(status) => {
            record_rejection(log, status, "request_headers");
            let _ = write_error(stream, status);
            return None;
        }
    };

    if parsed.content_length > config.max_body_bytes {
        record_rejection(log, 413, "request_body_too_large");
        let _ = write_error(stream, 413);
        return None;
    }
    let authenticated = if config.standalone_client_auth {
        standalone_request_is_authenticated(&parsed, bearer_token)
    } else {
        request_is_authenticated(&parsed, bearer_token, &config.gateway_mode)
    };
    if !authenticated {
        let reason = match config.gateway_mode {
            GatewayMode::Codex => "bearer_mismatch",
            GatewayMode::Unified(_) => "gateway_token_mismatch",
        };
        record_rejection(log, 401, reason);
        if config.standalone_client_auth {
            let remaining = parsed
                .content_length
                .saturating_sub(parsed.body_prefix.len());
            let _ = crate::serve_transport::drain_rejected_body(stream, remaining, shutdown);
        }
        let _ = write_error(stream, 401);
        return None;
    }
    if config.standalone_client_auth {
        // These credentials authenticate the local caller, not an upstream.
        parsed.headers.retain(|(name, _)| {
            !name.eq_ignore_ascii_case("authorization") && !name.eq_ignore_ascii_case("x-api-key")
        });
        parsed.authorization = None;
    }
    Some(parsed)
}

/// Dispatch one admitted request to its route handler.
fn route_request(
    stream: &mut TcpStream,
    connection: &ConnectionContext<'_>,
    mut parsed: ParsedRequest,
    conversation_key: &ConversationKey,
) {
    let (config, log) = (connection.config, connection.log);
    // Route on the path alone. Claude Code sends `POST /v1/messages?beta=true`,
    // so matching the raw request target 404s a request that is perfectly
    // valid -- a defect only a live client surfaces, since the mock probe
    // sends a bare path.
    let route = parsed
        .path
        .split('?')
        .next()
        .unwrap_or(parsed.path.as_str())
        .to_string();
    let method = parsed.method.clone();
    match (method.as_str(), route.as_str()) {
        ("POST", "/_clud/context/compact") => {
            let control = ContextControl::Compact;
            if let Some(key) =
                read_context_key(stream, connection, &mut parsed, control, conversation_key)
            {
                serve_context_compact(
                    stream,
                    config,
                    connection.shutdown,
                    connection.conversations,
                    log,
                    key,
                    connection.cache_health,
                );
            }
        }
        ("POST", "/_clud/context/clear") => {
            let control = ContextControl::Clear;
            if let Some(key) =
                read_context_key(stream, connection, &mut parsed, control, conversation_key)
            {
                serve_context_clear(
                    stream,
                    connection.conversations,
                    key,
                    connection.cache_health,
                    &config.status_usage,
                );
            }
        }
        ("POST", "/_clud/context/compact-finished") => {
            let control = ContextControl::CompactFinished;
            if let Some(key) =
                read_context_key(stream, connection, &mut parsed, control, conversation_key)
            {
                serve_context_compact_finished(stream, connection.conversations, log, key);
            }
        }
        ("GET", "/v1/models") => match &config.gateway_mode {
            GatewayMode::Codex => serve_codex_catalog(stream, config, log),
            GatewayMode::Unified(_) => serve_unified_catalog(stream, config, log),
        },
        ("GET", "/_clud/route/status") => match &config.gateway_mode {
            GatewayMode::Unified(unified) => serve_route_status(stream, unified),
            GatewayMode::Codex => {
                record_rejection(log, 404, "route_status_unsupported");
                let _ = write_error(stream, 404);
            }
        },
        ("POST", "/_clud/route/clear") => route_clear(stream, connection, &mut parsed),
        ("HEAD", "/v1/messages") => {
            let _ = write_response(stream, 200, "application/json", b"", true);
        }
        ("POST", "/v1/messages/count_tokens") => {
            route_count_tokens(stream, connection, &mut parsed, conversation_key);
        }
        ("POST", "/v1/messages") => {
            route_messages(stream, connection, &mut parsed, conversation_key);
        }
        ("POST", "/v1/chat/completions") => {
            route_chat_completions(stream, connection, &mut parsed, conversation_key);
        }
        _ => {
            if std::env::var_os("CLUD_CODEX_BRIDGE_DEBUG").is_some_and(|value| value == "1") {
                eprintln!(
                    "[clud] codex bridge: unhandled {} {}",
                    parsed.method, parsed.path
                );
            }
            record_rejection(log, 404, "unrouted_request");
            let _ = write_error(stream, 404);
        }
    }
}

/// Read a request body under the body budget. A failure is recorded under
/// `reason` and answered here; `None` means the connection is done.
fn read_request_body(
    stream: &mut TcpStream,
    connection: &ConnectionContext<'_>,
    parsed: &mut ParsedRequest,
    reason: &'static str,
) -> Option<Vec<u8>> {
    let body_deadline = Instant::now() + connection.config.body_timeout;
    match read_body(
        stream,
        std::mem::take(&mut parsed.body_prefix),
        parsed.content_length,
        body_deadline,
        connection.shutdown,
    ) {
        Ok(body) => Some(body),
        Err(ABANDON) => None,
        Err(status) => {
            record_rejection(connection.log, status, reason);
            let _ = write_error(stream, status);
            None
        }
    }
}

/// Read a `/_clud/context/*` body and resolve the conversation it names,
/// falling back to the one the request headers name.
fn read_context_key(
    stream: &mut TcpStream,
    connection: &ConnectionContext<'_>,
    parsed: &mut ParsedRequest,
    control: ContextControl,
    conversation_key: &ConversationKey,
) -> Option<ConversationKey> {
    let body_deadline = Instant::now() + connection.config.body_timeout;
    match read_context_control_body(
        stream,
        std::mem::take(&mut parsed.body_prefix),
        parsed.content_length,
        body_deadline,
        connection.shutdown,
        control,
        connection.fallback_root,
    ) {
        Ok(body_key) => Some(body_key.unwrap_or_else(|| conversation_key.clone())),
        Err(ABANDON) => None,
        Err(status) => {
            record_rejection(connection.log, status, "context_control_body");
            let _ = write_error(stream, status);
            None
        }
    }
}

/// `POST /_clud/route/clear`: unified gateway only.
fn route_clear(
    stream: &mut TcpStream,
    connection: &ConnectionContext<'_>,
    parsed: &mut ParsedRequest,
) {
    let Some(body) = read_request_body(stream, connection, parsed, "route_control_body") else {
        return;
    };
    match &connection.config.gateway_mode {
        GatewayMode::Unified(unified) => serve_route_clear(stream, unified, &body),
        GatewayMode::Codex => {
            record_rejection(connection.log, 404, "route_clear_unsupported");
            let _ = write_error(stream, 404);
        }
    }
}

/// `POST /v1/messages/count_tokens`: unified gateway only.
fn route_count_tokens(
    stream: &mut TcpStream,
    connection: &ConnectionContext<'_>,
    parsed: &mut ParsedRequest,
    conversation_key: &ConversationKey,
) {
    if !matches!(connection.config.gateway_mode, GatewayMode::Unified(_)) {
        record_rejection(connection.log, 404, "token_counting_unsupported");
        let _ = write_response(
            stream,
            404,
            "application/json",
            br#"{"error":{"type":"not_found_error","message":"token counting is not supported by the Codex bridge"}}"#,
            false,
        );
        return;
    }
    let Some(body) = read_request_body(stream, connection, parsed, "request_body") else {
        return;
    };
    serve_unified_count_tokens(
        stream,
        connection.config,
        connection.shutdown,
        connection.conversations,
        &body,
        conversation_key,
        &parsed.headers,
        connection.cache_health,
    );
}

/// `POST /v1/messages`: one model turn, through the unified gateway or the
/// Codex discovery translator.
fn route_messages(
    stream: &mut TcpStream,
    connection: &ConnectionContext<'_>,
    parsed: &mut ParsedRequest,
    conversation_key: &ConversationKey,
) {
    let (config, log) = (connection.config, connection.log);
    // Counted before the body is read, so a turn the bridge later
    // refuses still counts as "the harness talked to us" (#998). The
    // discovery route's own refusals are the bridge log's story; the
    // classification this feeds is only for total silence.
    config.turn_requests.fetch_add(1, Ordering::Release);
    let Some(body) = read_request_body(stream, connection, parsed, "request_body") else {
        return;
    };
    let json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(json) => json,
        Err(_) => {
            record_rejection(log, 400, "invalid_json");
            let _ = write_error(stream, 400);
            return;
        }
    };
    record_request_effort(&config.status_usage, &json, parsed.agent_id.is_some());
    let streaming = json.get("stream").and_then(serde_json::Value::as_bool) == Some(true);
    if matches!(config.gateway_mode, GatewayMode::Unified(_)) {
        serve_unified_messages(
            stream,
            config,
            connection.shutdown,
            connection.conversations,
            &body,
            streaming,
            conversation_key,
            &parsed.headers,
            log,
            connection.cache_health,
            None,
        );
    } else {
        serve_codex_discovery_messages(
            stream,
            config,
            connection.shutdown,
            connection.conversations,
            &body,
            streaming,
            conversation_key,
            log,
            connection.cache_health,
        );
    }
}

/// `POST /v1/chat/completions`: an OpenAI-shaped turn.
///
/// clud's routing, failover, catalog and credentials all speak Anthropic
/// Messages, so an OpenAI client is served by translating its request into that
/// shape at the door (`openai_ingress::translate_chat_request`) and translating
/// the answer back on the way out. This is CLIProxyAPI's
/// `internal/api/server_routes.go:73` (`v1.POST("/chat/completions")`) ported
/// onto the gateway clud already has: no route in the ladder learns a second
/// protocol, and nothing here changes the launch-scoped contract.
fn route_chat_completions(
    stream: &mut TcpStream,
    connection: &ConnectionContext<'_>,
    parsed: &mut ParsedRequest,
    conversation_key: &ConversationKey,
) {
    let (config, log) = (connection.config, connection.log);
    config.turn_requests.fetch_add(1, Ordering::Release);
    let Some(body) = read_request_body(stream, connection, parsed, "request_body") else {
        return;
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            record_rejection(log, 400, "invalid_json");
            refuse_chat(stream, 400, "request body is not valid JSON");
            return;
        }
    };
    let Some(model) = request.get("model").and_then(serde_json::Value::as_str) else {
        record_rejection(log, 400, "missing_model");
        refuse_chat(stream, 400, "request is missing a model");
        return;
    };
    let model = model.to_string();
    let streaming = request.get("stream").and_then(serde_json::Value::as_bool) == Some(true);
    if !matches!(config.gateway_mode, GatewayMode::Unified(_)) {
        record_rejection(log, 501, "chat_completions_unsupported");
        refuse_chat(
            stream,
            501,
            "POST /v1/chat/completions needs the unified gateway; this launch exposes the Codex discovery bridge only",
        );
        return;
    }
    let translated = match crate::openai_ingress::translate_chat_request(&body) {
        Ok(translated) => translated,
        Err(error) => {
            record_rejection(log, 400, "untranslatable_request");
            refuse_chat(stream, 400, &error.to_string());
            return;
        }
    };
    let translated = serde_json::to_vec(&translated).unwrap_or_default();
    serve_unified_messages(
        stream,
        config,
        connection.shutdown,
        connection.conversations,
        &translated,
        streaming,
        conversation_key,
        &parsed.headers,
        log,
        connection.cache_health,
        Some(ChatTranslation { model: &model }),
    );
}

/// Answer a refused `/v1/chat/completions` turn in the shape its client speaks.
fn refuse_chat(stream: &mut TcpStream, status: u16, message: &str) {
    // Every refusal on this route is an OpenAI error envelope: the client did
    // not ask for, and cannot parse, an Anthropic one.
    let envelope = crate::openai_ingress::render_chat_error(status, message);
    let _ = write_response(stream, status, "application/json", &envelope, false);
}

/// The client shape a translated `/v1/chat/completions` turn is answered in.
///
/// `model` is whatever the client asked for: OpenAI's reply echoes it, and a
/// provider that renames the wire model must not leak that name back.
#[derive(Clone, Copy)]
struct ChatTranslation<'a> {
    model: &'a str,
}

/// `GET /_clud/route/status` -- the configured ladder and each route's health.
///
/// Sits beside `/_clud/context/*` and carries the same gateway-token
/// requirement. Deliberately names only the public route, its cost owner, and a
/// reset clock: never a credential, base URL, prompt, or response body.
fn serve_route_status(stream: &mut TcpStream, unified: &UnifiedGatewayConfig) {
    let now = Instant::now();
    let ladder: Vec<serde_json::Value> = unified
        .failover
        .rungs()
        .iter()
        .map(|rung| {
            let state = unified
                .route_ledger
                .lock()
                .map(|ledger| ledger.state(rung.route, now))
                .unwrap_or(RouteState::Available);
            serde_json::json!({
                "spec": rung.spec,
                "route": rung.route.as_str(),
                "cost": rung.cost.as_str(),
                "withheld_for_consent": !unified.failover.allows_metered()
                    && rung.cost == crate::failover::CostOwner::Metered,
                "state": route_state_json(state),
            })
        })
        .collect();
    let routes: Vec<serde_json::Value> = unified
        .route_ledger
        .lock()
        .map(|ledger| {
            ledger
                .snapshot(now)
                .into_iter()
                .map(|(route, state)| {
                    serde_json::json!({
                        "route": route.as_str(),
                        "state": route_state_json(state),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let body = serde_json::json!({ "ladder": ladder, "routes": routes }).to_string();
    let _ = write_response(stream, 200, "application/json", body.as_bytes(), false);
}

fn route_state_json(state: RouteState) -> serde_json::Value {
    match state {
        RouteState::Available => serde_json::json!({"status": "available"}),
        RouteState::Cooling { remaining, reason } => serde_json::json!({
            "status": "cooling",
            "reason": reason,
            "retry_in_seconds": remaining.as_secs(),
        }),
        RouteState::Down { reason } => serde_json::json!({
            "status": "down",
            "reason": reason,
        }),
    }
}

/// `POST /_clud/route/clear` -- forget one route's health.
///
/// The escape hatch for a wedged session: a route marked drained has no clock,
/// so after the operator tops up a balance or replaces a key nothing else can
/// bring it back.
fn serve_route_clear(stream: &mut TcpStream, unified: &UnifiedGatewayConfig, body: &[u8]) {
    let requested = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("route")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    let route = match requested.as_deref() {
        Some("claude") => Some(ConversationRoute::Claude),
        Some("codex") => Some(ConversationRoute::Codex),
        Some("deepseek") => Some(ConversationRoute::DeepSeek),
        Some("kimi") => Some(ConversationRoute::Kimi),
        Some("openrouter") => Some(ConversationRoute::OpenRouter),
        _ => None,
    };
    let Some(route) = route else {
        let _ = write_response(
            stream,
            400,
            "application/json",
            br#"{"error":{"type":"invalid_request_error","message":"route must be one of claude, codex, deepseek, kimi, openrouter"}}"#,
            false,
        );
        return;
    };
    if let Ok(mut ledger) = unified.route_ledger.lock() {
        ledger.clear(route);
    }
    let body = serde_json::json!({"cleared": route.as_str()}).to_string();
    let _ = write_response(stream, 200, "application/json", body.as_bytes(), false);
}

fn serve_unified_catalog(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    log: Option<&SharedBridgeLog>,
) {
    let GatewayMode::Unified(unified) = &config.gateway_mode else {
        unreachable!("catalog route is guarded by the unified mode match arm");
    };
    let advertised = provider_catalog::MODELS
        .iter()
        .filter(|entry| unified.provider_available(entry.provider))
        .filter_map(|entry| entry.discovery_id.map(|id| (id, entry.display_name)))
        // #1257: a pinned launch may only be *advertised* rows it would also
        // serve. Discovery adds rows and cannot subtract them from the
        // harness's built-in catalog (DD-054), but the gateway's own listing
        // is clud's to filter -- and the refusal path backs it up for any row
        // the harness already knew.
        .filter(|(id, _)| provider_catalog::model_allowlist_allows(&config.allowed_models, id))
        .collect::<Vec<_>>();
    // #1021: unified had no `catalog_advertised` entry, which mattered more
    // here than on the direct route -- that one advertises a fixed three rows,
    // while this set varies with which provider credentials happen to be
    // stored. "What did the gateway actually offer?" is not answerable after
    // the fact without it.
    record_catalog_advertised(
        log,
        &advertised.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
    );
    let mut data = advertised
        .iter()
        .map(|(id, display_name)| {
            serde_json::json!({
                "id": id,
                "display_name": display_name,
                "type": "model",
            })
        })
        .collect::<Vec<_>>();
    let mut has_more = false;
    if config.standalone_claude_credential.is_some() {
        let Ok((mut native, more)) = fetch_standalone_claude_models(config, unified) else {
            let _ = write_error(stream, 502);
            return;
        };
        native.append(&mut data);
        data = native;
        has_more = more;
    }
    let body = if config.standalone_client_auth {
        let first = data.first().and_then(|row| row.get("id")).cloned();
        let last = data.last().and_then(|row| row.get("id")).cloned();
        serde_json::json!({"data": data, "has_more": has_more, "first_id": first, "last_id": last})
    } else {
        serde_json::json!({"data": data, "has_more": has_more})
    }
    .to_string();
    let _ = write_response(stream, 200, "application/json", body.as_bytes(), false);
}

/// Fetch native Claude inventory using the server's credential, not the client's.
fn fetch_standalone_claude_models(
    config: &BridgeConfig,
    unified: &UnifiedGatewayConfig,
) -> Result<(Vec<serde_json::Value>, bool), ()> {
    let mut rows = Vec::new();
    let mut cursor = None;
    for _ in 0..50 {
        let page = fetch_standalone_claude_model_page(config, unified, cursor.as_deref())?;
        let page_rows = page
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or(())?;
        if page_rows
            .iter()
            .any(|row| row.get("id").and_then(serde_json::Value::as_str).is_none())
        {
            return Err(());
        }
        rows.extend(page_rows.iter().cloned());
        if rows.len() > 10_000 {
            return Err(());
        }
        if !page
            .get("has_more")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Ok((rows, false));
        }
        let next = page
            .get("last_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(())?;
        if cursor.as_deref() == Some(next) {
            return Err(());
        }
        cursor = Some(next.to_string());
    }
    Err(())
}

fn fetch_standalone_claude_model_page(
    config: &BridgeConfig,
    unified: &UnifiedGatewayConfig,
    cursor: Option<&str>,
) -> Result<serde_json::Value, ()> {
    let headers = standalone_claude_headers(config, ConversationRoute::Claude, &[])?.ok_or(())?;
    let builder = ureq::AgentBuilder::new().timeout(Duration::from_secs(15));
    let agent = crate::serve_transport::tracked_agent(builder, &config.standalone_upstreams);
    let mut request = agent
        .get(&format!(
            "{}/v1/models",
            unified.anthropic_base_url.trim_end_matches('/')
        ))
        .set("anthropic-version", "2023-06-01");
    if let Some(cursor) = cursor {
        request = request.query("after_id", cursor);
    }
    for (name, value) in headers {
        request = request.set(&name, &value);
    }
    let response = request.call().map_err(|_| ())?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > 1_048_576 {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn serve_codex_catalog(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    log: Option<&SharedBridgeLog>,
) {
    let advertised = provider_catalog::MODELS
        .iter()
        .filter(|entry| entry.provider == ModelProvider::Codex)
        .filter_map(|entry| entry.discovery_id.map(|id| (id, entry.display_name)))
        // #1257: same rule as the unified catalog -- a pinned launch is
        // advertised only rows it would also serve (plus the route's own
        // injected role rows, which the launch folds into this allowlist).
        .filter(|(id, _)| provider_catalog::model_allowlist_allows(&config.allowed_models, id))
        .collect::<Vec<_>>();
    record_catalog_advertised(
        log,
        &advertised.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
    );
    let data = advertised
        .iter()
        .map(|(id, display_name)| {
            serde_json::json!({
                "id": id,
                "display_name": display_name,
                "type": "model",
            })
        })
        .collect::<Vec<_>>();
    let body = serde_json::json!({"data": data, "has_more": false}).to_string();
    let _ = write_response(stream, 200, "application/json", body.as_bytes(), false);
}

/// Reject ambiguous credentials instead of choosing one of several headers.
fn standalone_request_is_authenticated(request: &ParsedRequest, token: &str) -> bool {
    let mut credentials = request.headers.iter().filter(|(name, _)| {
        name.eq_ignore_ascii_case("x-api-key") || name.eq_ignore_ascii_case("authorization")
    });
    let Some((name, value)) = credentials.next() else {
        return false;
    };
    if credentials.next().is_some() || token.is_empty() {
        return false;
    }
    let provided = if name.eq_ignore_ascii_case("x-api-key") {
        Some(value.as_str())
    } else {
        value.strip_prefix("Bearer ")
    };
    provided.is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()))
}

fn request_is_authenticated(request: &ParsedRequest, token: &str, mode: &GatewayMode) -> bool {
    let provided = match mode {
        GatewayMode::Codex => request.authorization.as_deref(),
        GatewayMode::Unified(_) => request.header(UNIFIED_GATEWAY_TOKEN_HEADER),
    };
    let expected = match mode {
        GatewayMode::Codex => format!("Bearer {token}"),
        GatewayMode::Unified(_) => token.to_string(),
    };
    provided.is_some_and(|provided| constant_time_eq(provided.as_bytes(), expected.as_bytes()))
}

#[derive(Clone, Copy)]
enum ContextControl {
    Compact,
    CompactFinished,
    Clear,
}

fn read_context_control_body(
    stream: &mut TcpStream,
    prefix: Vec<u8>,
    content_length: usize,
    deadline: Instant,
    shutdown: &AtomicBool,
    control: ContextControl,
    fallback_root: &str,
) -> Result<Option<ConversationKey>, u16> {
    let body = read_body(stream, prefix, content_length, deadline, shutdown)?;
    if body.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| 400_u16)?;
    let matches_lifecycle_event = match control {
        ContextControl::Compact => {
            value
                .get("hook_event_name")
                .and_then(serde_json::Value::as_str)
                == Some("PreCompact")
                && matches!(
                    value.get("trigger").and_then(serde_json::Value::as_str),
                    Some("manual" | "auto")
                )
        }
        ContextControl::Clear => {
            value
                .get("hook_event_name")
                .and_then(serde_json::Value::as_str)
                == Some("SessionStart")
                && value.get("source").and_then(serde_json::Value::as_str) == Some("clear")
        }
        ContextControl::CompactFinished => {
            value
                .get("hook_event_name")
                .and_then(serde_json::Value::as_str)
                == Some("SessionStart")
                && value.get("source").and_then(serde_json::Value::as_str) == Some("compact")
        }
    };
    if !matches_lifecycle_event {
        return Err(400);
    }
    let session_id = value.get("session_id").and_then(serde_json::Value::as_str);
    let agent_id = value.get("agent_id").and_then(serde_json::Value::as_str);
    Ok((session_id.is_some() || agent_id.is_some())
        .then(|| conversation_key_from_headers(session_id, agent_id, fallback_root)))
}

fn conversation_key_from_headers(
    session_id: Option<&str>,
    agent_id: Option<&str>,
    fallback_root: &str,
) -> ConversationKey {
    match session_id.filter(|session| !session.is_empty()) {
        Some(session_id) => ConversationKey::from_headers(Some(session_id), agent_id),
        None => ConversationKey::bridge_fallback(fallback_root, agent_id),
    }
}

fn serve_context_compact(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    shutdown: &AtomicBool,
    conversations: &ConversationStore,
    log: Option<&SharedBridgeLog>,
    conversation_key: ConversationKey,
    cache_health: &SharedCacheHealth,
) {
    let operation = conversations.with_history(&conversation_key.id, |history| {
        let snapshot = history.snapshot();
        if snapshot.is_empty() {
            history.begin_harness_compaction_fallback();
            return Ok(Ok(()));
        }
        // Claude may start compaction while a tool batch is outstanding, and
        // some credential routes may not expose `/responses/compact`. Neither
        // condition may block Claude's own compaction lifecycle. Fall back to
        // a two-phase clear: allow the compaction inference to replay the old
        // transcript, then discard that temporary replay when
        // `SessionStart(compact)` confirms Claude installed its summary.
        let result = crate::codex_pipeline::validate_canonical_history(&snapshot)
            .and_then(|()| {
                build_pipeline(config, log, &conversation_key, codex_redactor(config))
                    .map_err(PipelineError::Upstream)
            })
            .and_then(|pipeline| pipeline.compact_canonical_history(snapshot, shutdown))
            .and_then(|replacement| {
                history
                    .install_provider_compaction(&replacement)
                    .map_err(|error| {
                        PipelineError::Translate(crate::codex_translate::TranslateError::Invalid(
                            error.to_string(),
                        ))
                    })
            });
        if let Err(error) = result {
            log_context_compact_fallback(&error, &conversation_key, log);
            history.begin_harness_compaction_fallback();
        }
        Ok(Ok(()))
    });
    match operation {
        Ok(Ok(())) => {
            lock_cache_health(cache_health).mark_boundary(&conversation_key.id);
            let _ = write_response(stream, 204, "application/json", b"", false);
        }
        Ok(Err(error)) => {
            let _ = write_pipeline_error(stream, &error, log);
        }
        Err(error) => {
            let failure = PipelineError::Translate(
                crate::codex_translate::TranslateError::Invalid(error.to_string()),
            );
            let _ = write_pipeline_error(stream, &failure, log);
        }
    }
}

fn serve_context_compact_finished(
    stream: &mut TcpStream,
    conversations: &ConversationStore,
    log: Option<&SharedBridgeLog>,
    conversation_key: ConversationKey,
) {
    let operation = conversations.with_history(&conversation_key.id, |history| {
        Ok(history.finish_harness_compaction_fallback())
    });
    match operation {
        Ok(reset) => {
            if reset {
                if let Some(log) = log {
                    lock_log(log).record(serde_json::json!({
                        "ts_ms": unix_ms(),
                        "event": "context_compact_fallback_finished",
                        "conversation_scope": conversation_key.scope(),
                    }));
                }
            }
            let _ = write_response(stream, 204, "application/json", b"", false);
        }
        Err(error) => {
            let failure = PipelineError::Translate(
                crate::codex_translate::TranslateError::Invalid(error.to_string()),
            );
            let _ = write_pipeline_error(stream, &failure, log);
        }
    }
}

fn log_context_compact_fallback(
    error: &PipelineError,
    conversation_key: &ConversationKey,
    log: Option<&SharedBridgeLog>,
) {
    let Some(log) = log else {
        return;
    };
    let mut event = serde_json::json!({
        "ts_ms": unix_ms(),
        "event": "context_compact_fallback",
        "conversation_scope": conversation_key.scope(),
        "kind": pipeline_error_kind(error),
    });
    if let PipelineError::ContinuationInvariant(failure) = error {
        event["unmatched_call_count"] = failure.unmatched_call_count.into();
        event["input_count"] = failure.input_count.into();
    }
    if let PipelineError::Upstream(UpstreamError::Status(failure)) = error {
        add_failure_fields(&mut event, failure);
    }
    lock_log(log).record(event);
}

fn serve_context_clear(
    stream: &mut TcpStream,
    conversations: &ConversationStore,
    conversation_key: ConversationKey,
    cache_health: &SharedCacheHealth,
    status_usage: &SharedStatusUsage,
) {
    conversations.clear_session(&conversation_key.session_prefix);
    reset_status_usage(status_usage);
    let result = Ok::<(), crate::codex_history::HistoryError>(());
    match result {
        Ok(()) => {
            lock_cache_health(cache_health).clear_session(&conversation_key.session_prefix);
            let _ = write_response(stream, 204, "application/json", b"", false);
        }
        Err(error) => {
            let failure = PipelineError::Translate(
                crate::codex_translate::TranslateError::Invalid(error.to_string()),
            );
            let _ = write_pipeline_error(stream, &failure, None);
        }
    }
}

/// Build the pipeline for one request.
///
/// The debug seam repoints the *upstream base URL* at a Responses-shaped fake.
/// Phase 2 used it to pass an Anthropic body through unchanged, which meant the
/// end-to-end tests proved transport and auth but nothing about translation.
fn build_pipeline(
    config: &BridgeConfig,
    log: Option<&SharedBridgeLog>,
    conversation_key: &ConversationKey,
    redaction: Option<&SharedSession>,
) -> Result<Pipeline<ResolvedCredentials>, UpstreamError> {
    let mut credentials = match config.test_upstream_url.as_deref() {
        Some(base_url) => ResolvedCredentials::ApiKey(ApiKeyCredentials::new(
            "clud-test-upstream-key",
            Some(base_url.into()),
        )?),
        None => ResolvedCredentials::resolve_default()?,
    };
    if config.test_upstream_url.is_none()
        && recover_marked_credentials(&credentials, &config.auth_recheck)?
    {
        credentials = ResolvedCredentials::resolve_default()?;
    }
    let upstream_config = UpstreamConfig {
        first_frame_timeout: Some(config.first_frame_timeout),
        read_timeout: config.stream_idle_timeout,
        ..UpstreamConfig::default()
    };
    // `build_pipeline` runs for every Messages request. A fresh client UUID
    // here would rotate the upstream prompt-cache key and session headers on
    // every turn, defeating cache reuse for a long Claude conversation.
    let mut client = UpstreamClient::new(credentials, upstream_config)
        .with_session_id(conversation_key.id.clone())
        .with_redaction(redaction.cloned());
    if config.standalone_client_auth {
        client = client.with_shutdown_sockets(Arc::clone(&config.standalone_upstreams));
    }
    if let Some(log) = log.cloned() {
        client = client.with_retry_observer(move |error, attempt, budget, backoff| {
            record_retry(&log, error, attempt, budget, backoff);
        });
    }
    let mut pipeline = Pipeline::new(client);
    if let Some(model) = config.default_model.clone() {
        pipeline = pipeline.with_default_model(model);
    }
    Ok(pipeline)
}

fn codex_redactor(config: &BridgeConfig) -> Option<&SharedSession> {
    match &config.gateway_mode {
        GatewayMode::Unified(unified) => unified.redactor_for(ModelProvider::Codex, None),
        _ => None,
    }
}

fn recover_marked_credentials<C: CredentialSource>(
    credentials: &C,
    marker: &Mutex<Option<UpstreamTarget>>,
) -> Result<bool, UpstreamError> {
    let Some(rejected) = marker
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
    else {
        return Ok(false);
    };
    let result = credentials.recover_after_unauthorized(&rejected);
    if result.is_err() {
        let mut pending = marker.lock().unwrap_or_else(|poison| poison.into_inner());
        if pending.is_none() {
            *pending = Some(rejected);
        }
    }
    result.map(|_| true)
}

fn mark_auth_recheck(
    marker: &Mutex<Option<UpstreamTarget>>,
    pipeline: &Pipeline<ResolvedCredentials>,
) {
    if let Ok(target) = pipeline.credential_target() {
        *marker.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(target);
    }
}

/// Route one unified request before the legacy Codex translator sees it.
#[allow(clippy::too_many_arguments)]
fn serve_unified_messages(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    shutdown: &AtomicBool,
    conversations: &ConversationStore,
    body: &[u8],
    streaming: bool,
    conversation_key: &ConversationKey,
    headers: &[(String, String)],
    log: Option<&SharedBridgeLog>,
    cache_health: &SharedCacheHealth,
    chat: Option<ChatTranslation<'_>>,
) {
    let GatewayMode::Unified(unified) = &config.gateway_mode else {
        unreachable!("unified request dispatch is guarded by the caller");
    };
    let mut request: serde_json::Value = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => {
            let _ = write_error(stream, 400);
            return;
        }
    };
    let model = match request.get("model").and_then(serde_json::Value::as_str) {
        Some(model) => model,
        None => {
            let _ = write_error(stream, 400);
            return;
        }
    };
    let resolved = resolve_unified_catalog(model);
    if model.starts_with("clud-claude-") && resolved.entry.is_none() {
        refuse_unknown_gateway_model(stream, log, unified, model);
        return;
    }
    // #1257: a known model that the launch's allowlist does not admit is the
    // pin doing its job -- refused here rather than at the upstream, with the
    // boundary named so the fix is one line to read.
    if refuse_if_outside_allowlist(stream, log, &config.allowed_models, model) {
        return;
    }
    let original_model = model.to_string();
    let mut attempt = UnifiedAttempt::for_launch(unified, &resolved, model);
    let _redaction_report = redaction_launch_report(config, unified);
    let mut turn = UnifiedTurn {
        config,
        unified,
        shutdown,
        conversations,
        streaming,
        conversation_key,
        headers,
        log,
        cache_health,
        redaction: None,
        chat,
    };

    // Descend the ladder. Probing is enabled only while a further rung exists,
    // so a launch that configured no failover takes exactly today's path and
    // never buffers a byte it would not have buffered before.
    loop {
        turn.retain_redaction(&attempt);
        let payload = match &attempt.model {
            Some(model) if model != &original_model => {
                request["model"] = serde_json::Value::String(model.clone());
                serde_json::to_vec(&request).unwrap_or_default()
            }
            _ => body.to_vec(),
        };
        let fallback = unified.next_rung(attempt.spec.as_deref(), attempt.route);
        let probe = fallback.is_some();

        let outcome = dispatch_unified_attempt(stream, &turn, &attempt, &payload, probe);

        match outcome {
            ProxyOutcome::Committed { status } => {
                // Only a served turn is evidence the route recovered. A
                // committed *failure* -- a malformed request, an upstream
                // outage -- says nothing about the route, so it must not clear
                // a cooldown the ledger is still serving.
                if status < 400 {
                    unified.record_route_success(attempt.route);
                }
                return;
            }
            ProxyOutcome::Declined(verdict) => {
                let state = unified.record_route(attempt.route, verdict);
                // Recomputed after recording, so the route that just declined
                // is skipped on its own merits rather than by position.
                let Some(rung) = unified.next_rung(attempt.spec.as_deref(), attempt.route) else {
                    // Nothing left to try. Re-issue against the same route with
                    // probing off so the client receives the real upstream
                    // status instead of a silent hang.
                    warn_route_exhausted(attempt.route, verdict, state, None);
                    if verdict == RouteVerdict::ProviderFault {
                        write_provider_fault(stream, attempt.route);
                        return;
                    }
                    return finish_without_failover(
                        stream,
                        config,
                        unified,
                        conversations,
                        conversation_key,
                        headers,
                        &payload,
                        attempt.route,
                        shutdown,
                        cache_health,
                        turn.redaction,
                        chat,
                    );
                };
                warn_route_exhausted(attempt.route, verdict, state, Some(&rung));
                attempt = UnifiedAttempt {
                    provider: rung.provider,
                    route: rung.route,
                    model: Some(rung.wire_id.clone()),
                    spec: Some(rung.spec.clone()),
                };
            }
        }
    }
}

/// The launch's one redaction report, held for the life of a unified turn.
fn redaction_launch_report(
    config: &BridgeConfig,
    unified: &UnifiedGatewayConfig,
) -> crate::secret_redaction::status::Report {
    crate::secret_redaction::status::Report::new(
        unified.redaction.as_ref(),
        config
            .status_usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .writer
            .clone(),
    )
}

fn write_provider_fault(stream: &mut TcpStream, route: ConversationRoute) {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": format!(
                "gateway route {} returned a malformed Messages response",
                route.as_str()
            ),
        }
    })
    .to_string();
    let _ = write_response(stream, 502, "application/json", body.as_bytes(), false);
}

/// The catalog row a unified request's model names, plus the legacy
/// `<model>@<effort>` suffix a Codex discovery selection may carry.
struct UnifiedCatalogMatch<'a> {
    entry: Option<provider_catalog::CatalogModel>,
    codex_effort_suffix: Option<&'a str>,
}

fn resolve_unified_catalog(model: &str) -> UnifiedCatalogMatch<'_> {
    let exact_catalog = provider_catalog::model_by_discovery_id(model);
    // A Codex discovery selection may retain the bridge's legacy
    // `<model>@<effort>` override. Resolve the reserved base ID before the
    // translator sees its `claude` substring, then carry the suffix onto the
    // reviewed wire model so `effort_for` remains the sole precedence owner.
    let mut codex_effort_suffix = None;
    let entry = exact_catalog
        .or_else(|| {
            let (base, effort) = provider_catalog::split_effort_suffix(model);
            let effort = effort?;
            let entry = provider_catalog::model_by_discovery_id(base).or_else(|| {
                provider_catalog::non_claude_model_by_any_id(base)
                    .filter(|entry| entry.provider == ModelProvider::Codex)
            })?;
            (entry.provider == ModelProvider::Codex).then(|| {
                codex_effort_suffix = Some(effort);
                entry
            })
        })
        // A persisted or continued session can still name a known provider by
        // wire ID or CLI alias instead of its discovery ID. Resolve those
        // through the shared catalog so they route to their own provider
        // rather than leaking to Anthropic as an "ordinary Claude" model.
        .or_else(|| provider_catalog::non_claude_model_by_any_id(model));
    UnifiedCatalogMatch {
        entry,
        codex_effort_suffix,
    }
}

/// Answer a `clud-claude-*` ID the catalog does not know with 400, quoting
/// the offending ID and the IDs this gateway serves.
fn refuse_unknown_gateway_model(
    stream: &mut TcpStream,
    log: Option<&SharedBridgeLog>,
    unified: &UnifiedGatewayConfig,
    model: &str,
) {
    let ids = unified_catalog_ids(unified).join(", ");
    // #1010 item 2: this route kept #1000's single message after #1009
    // gave the direct route two cases and made it quote the offending ID.
    // Only the quoting carries over.
    //
    // The direct route's second case -- "clud knows this model, but this
    // gateway is not serving it" -- is unreachable here, and adding it
    // would be dead code.
    //
    // `exact_catalog` in `resolve_unified_catalog` resolves any discovery ID
    // with no provider filter, and the fallback resolves anything else by cli
    // id, wire id, or legacy alias for every provider except Claude and
    // OpenRouter. So reaching this branch with an ID clud knows would need a
    // `clud-claude-*` alias on a *Claude or OpenRouter* row that is not
    // that row's discovery ID. No such row exists today -- note that
    // `clud-claude-deepseek-v4-pro` is such an alias but on a DeepSeek
    // row, which the fallback resolves, so it never lands here.
    //
    // `only_a_claude_or_openrouter_alias_would_make_the_unified_second_
    // case_reachable` in provider_catalog fails if one is ever added.
    record_model_rejection(log, 400, "unknown_model", model);
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!("unknown clud gateway model '{model}'; available IDs: {ids}"),
        }
    })
    .to_string();
    let _ = write_response(stream, 400, "application/json", body.as_bytes(), false);
}

/// What one descent step will send. `model` is `None` only for an ordinary
/// Claude ID, which stays byte-for-byte caller owned.
struct UnifiedAttempt {
    provider: ModelProvider,
    route: ConversationRoute,
    model: Option<String>,
    spec: Option<String>,
}

impl UnifiedAttempt {
    fn for_launch(
        unified: &UnifiedGatewayConfig,
        resolved: &UnifiedCatalogMatch<'_>,
        model: &str,
    ) -> Self {
        let Some((provider, wire)) = unified
            .launch_model
            .as_ref()
            .filter(|(_, wire)| model == wire)
        else {
            return Self::initial(resolved, model);
        };
        Self {
            provider: *provider,
            route: crate::route_plan::conversation_route(*provider),
            model: Some(wire.clone()),
            spec: Some(wire.clone()),
        }
    }

    /// The first rung: the route the request's own model names.
    fn initial(resolved: &UnifiedCatalogMatch<'_>, model: &str) -> Self {
        let Some(entry) = resolved.entry else {
            return Self {
                provider: ModelProvider::Claude,
                route: ConversationRoute::Claude,
                model: None,
                spec: None,
            };
        };
        let requested_base = provider_catalog::split_effort_suffix(model).0;
        let target = if entry.provider == ModelProvider::Codex
            && requested_base.eq_ignore_ascii_case(entry.wire_id)
        {
            requested_base.to_string()
        } else {
            crate::codex_runtime::wire_id(entry.cli_id, entry.wire_id)
        };
        Self {
            provider: entry.provider,
            route: crate::route_plan::conversation_route(entry.provider),
            model: Some(
                resolved
                    .codex_effort_suffix
                    .map_or_else(|| target.clone(), |effort| format!("{target}@{effort}")),
            ),
            spec: None,
        }
    }
}

/// The request-scoped state every unified descent step reads.
struct UnifiedTurn<'a> {
    config: &'a BridgeConfig,
    unified: &'a UnifiedGatewayConfig,
    shutdown: &'a AtomicBool,
    conversations: &'a ConversationStore,
    streaming: bool,
    conversation_key: &'a ConversationKey,
    headers: &'a [(String, String)],
    log: Option<&'a SharedBridgeLog>,
    cache_health: &'a SharedCacheHealth,
    redaction: Option<&'a SharedSession>,
    /// `Some` when the client speaks OpenAI Chat Completions, so every
    /// committed response body is translated back before it is written.
    chat: Option<ChatTranslation<'a>>,
}

impl UnifiedTurn<'_> {
    fn retain_redaction(&mut self, attempt: &UnifiedAttempt) {
        self.redaction = self.redaction.or_else(|| {
            self.unified
                .redactor_for(attempt.provider, attempt.model.as_deref())
        });
    }
}

/// Send one descent step to its provider.
fn dispatch_unified_attempt(
    stream: &mut TcpStream,
    turn: &UnifiedTurn<'_>,
    attempt: &UnifiedAttempt,
    payload: &[u8],
    probe: bool,
) -> ProxyOutcome {
    let (config, unified) = (turn.config, turn.unified);
    match attempt.provider {
        ModelProvider::Claude => serve_unified_anthropic_proxy(
            config,
            stream,
            turn.conversations,
            turn.conversation_key,
            ConversationRoute::Claude,
            &unified.anthropic_base_url,
            "/v1/messages",
            payload,
            turn.headers,
            None,
            config.stream_idle_timeout,
            turn.shutdown,
            probe,
            turn.cache_health,
            &config.status_usage,
            turn.redaction,
            turn.chat,
        ),
        provider @ (ModelProvider::DeepSeek | ModelProvider::Kimi | ModelProvider::OpenRouter)
            if unified.route_for(provider).is_some() =>
        {
            let route = unified
                .route_for(provider)
                .expect("guarded by route_for above");
            // #1863: the gateway can honour `--provider-only` itself, because
            // it owns the request path.
            let routed = unified.routed_body(provider, payload);
            serve_unified_anthropic_proxy(
                config,
                stream,
                turn.conversations,
                turn.conversation_key,
                attempt.route,
                &route.base_url,
                "/v1/messages",
                routed.as_deref().unwrap_or(payload),
                turn.headers,
                Some(&route.api_key),
                config.stream_idle_timeout,
                turn.shutdown,
                probe,
                turn.cache_health,
                &config.status_usage,
                turn.redaction,
                turn.chat,
            )
        }
        ModelProvider::Codex if unified.codex_available && turn.chat.is_some() => {
            // Codex's pipeline writes Messages frames straight to the client,
            // so an OpenAI-shaped turn cannot be answered verbatim. Refuse
            // rather than emit a body the client cannot parse; the Anthropic
            // routes stay available for the same model.
            let body = crate::openai_ingress::render_chat_error(
                400,
                "this model is served by the Codex route, which does not translate Anthropic responses back to OpenAI on this gateway",
            );
            let _ = write_response(stream, 400, "application/json", &body, false);
            ProxyOutcome::local(400)
        }
        ModelProvider::Codex if unified.codex_available => {
            // Codex is a valid destination but never a probe source: its
            // pipeline commits through a different path, so a descent that
            // lands here stops here.
            serve_messages(
                stream,
                config,
                turn.shutdown,
                turn.conversations,
                payload,
                turn.streaming,
                turn.conversation_key,
                Some(ConversationRoute::Codex),
                turn.log,
                turn.cache_health,
                turn.redaction,
            );
            ProxyOutcome::local(200)
        }
        // Only a registry provider whose key this launch lacks, or Codex
        // without credentials, reaches here. Defense in depth: such routes
        // are omitted from discovery, but a stale picker must not reach any
        // paid model.
        ModelProvider::DeepSeek
        | ModelProvider::Kimi
        | ModelProvider::OpenRouter
        | ModelProvider::Codex => {
            let body = serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "the selected provider is not configured; run `clud auth status`",
                }
            })
            .to_string();
            let _ = write_response(stream, 400, "application/json", body.as_bytes(), false);
            ProxyOutcome::local(400)
        }
    }
}

/// One sanitized line per transition. Names the route, the reason, and the
/// rung taken -- never a credential, prompt, or response body.
fn warn_route_exhausted(
    route: ConversationRoute,
    verdict: RouteVerdict,
    state: Option<RouteState>,
    taken: Option<&FailoverRung>,
) {
    let clock = match state {
        Some(RouteState::Cooling { remaining, .. }) => {
            format!(" (retry in {}s)", remaining.as_secs())
        }
        _ => String::new(),
    };
    match taken {
        Some(rung) => eprintln!(
            "[clud] route: {} {}{} -> continuing on {}",
            route.as_str(),
            verdict.reason(),
            clock,
            rung.label(),
        ),
        None => eprintln!(
            "[clud] route: {} {}{}; no configured fallback is available",
            route.as_str(),
            verdict.reason(),
            clock,
        ),
    }
}

/// Replay the last attempt with probing disabled so the client sees the real
/// upstream response. Only reached once the ladder is spent.
#[allow(clippy::too_many_arguments)]
fn finish_without_failover(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    unified: &UnifiedGatewayConfig,
    conversations: &ConversationStore,
    conversation_key: &ConversationKey,
    headers: &[(String, String)],
    payload: &[u8],
    route: ConversationRoute,
    shutdown: &AtomicBool,
    cache_health: &SharedCacheHealth,
    redaction: Option<&SharedSession>,
    chat: Option<ChatTranslation<'_>>,
) {
    let direct = match route {
        ConversationRoute::DeepSeek => unified.route_for(ModelProvider::DeepSeek),
        ConversationRoute::Kimi => unified.route_for(ModelProvider::Kimi),
        ConversationRoute::OpenRouter => unified.route_for(ModelProvider::OpenRouter),
        ConversationRoute::Claude | ConversationRoute::Codex => None,
    };
    let (base_url, key) = match direct {
        Some(route) => (route.base_url.as_str(), Some(route.api_key.as_str())),
        None => (unified.anthropic_base_url.as_str(), None),
    };
    let provider = match route {
        ConversationRoute::DeepSeek => ModelProvider::DeepSeek,
        ConversationRoute::Kimi => ModelProvider::Kimi,
        ConversationRoute::OpenRouter => ModelProvider::OpenRouter,
        ConversationRoute::Claude | ConversationRoute::Codex => ModelProvider::Claude,
    };
    let routed = unified.routed_body(provider, payload);
    serve_unified_anthropic_proxy(
        config,
        stream,
        conversations,
        conversation_key,
        route,
        base_url,
        "/v1/messages",
        routed.as_deref().unwrap_or(payload),
        headers,
        key,
        config.stream_idle_timeout,
        shutdown,
        false,
        cache_health,
        &config.status_usage,
        redaction,
        chat,
    );
}

#[allow(clippy::too_many_arguments)]
fn serve_unified_anthropic_proxy(
    config: &BridgeConfig,
    stream: &mut TcpStream,
    conversations: &ConversationStore,
    conversation_key: &ConversationKey,
    route: ConversationRoute,
    base_url: &str,
    path: &str,
    body: &[u8],
    headers: &[(String, String)],
    injected_api_key: Option<&str>,
    idle_timeout: Duration,
    shutdown: &AtomicBool,
    probe: bool,
    cache_health: &SharedCacheHealth,
    status_usage: &SharedStatusUsage,
    redaction: Option<&SharedSession>,
    chat: Option<ChatTranslation<'_>>,
) -> ProxyOutcome {
    let managed_headers = match standalone_claude_headers(config, route, headers) {
        Ok(headers) => headers,
        Err(()) => {
            // A served caller's credential problem is reported in the shape its
            // client speaks; an OpenAI chat caller cannot parse the Anthropic
            // envelope.
            if chat.is_some() {
                return write_chat_error(
                    stream,
                    503,
                    "the served backend has no usable upstream credential",
                    idle_timeout,
                );
            }
            let _ = write_error(stream, 503);
            return ProxyOutcome::local(503);
        }
    };
    let headers = managed_headers.as_deref().unwrap_or(headers);
    let provider = match route {
        ConversationRoute::Claude => AnthropicUsageProvider::Claude,
        ConversationRoute::DeepSeek => AnthropicUsageProvider::DeepSeek,
        ConversationRoute::Kimi => AnthropicUsageProvider::Kimi,
        ConversationRoute::OpenRouter => AnthropicUsageProvider::OpenRouter,
        ConversationRoute::Codex => unreachable!("Codex does not use the Anthropic proxy"),
    };
    let model = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|request| {
            request
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    // #1529: the advisor is an Anthropic server tool that no compat upstream
    // runs. Its two halves travel together or not at all.
    let stripped;
    let (body, headers) = if route == ConversationRoute::Claude {
        (body, headers)
    } else {
        stripped = strip_advisor_capability(body, headers);
        (stripped.0.as_slice(), stripped.1.as_slice())
    };
    let mut outcome = ProxyOutcome::local(500);
    // #2046: compat routes get an out-of-band cache heartbeat while idle.
    let heartbeat = (route != ConversationRoute::Claude && path == "/v1/messages")
        .then(crate::cache_heartbeat::shared)
        .flatten();
    if let Some(beats) = heartbeat {
        beats.begin(&conversation_key.id, Instant::now());
    }
    let routed = conversations.with_history(&conversation_key.id, |history| {
        // Crossing a provider boundary starts a new route epoch, which is
        // exactly what a failover is: a provider switch nobody typed.
        if history.enter_route(route) {
            lock_cache_health(cache_health).mark_boundary(&conversation_key.id);
        }
        let mut usage = StreamingUsage::new(provider);
        outcome = serve_anthropic_proxy(
            stream,
            AnthropicProxyTarget {
                base_url,
                path,
                injected_api_key,
                redaction,
                chat,
                sanitize_errors: config.standalone_client_auth,
                sockets: config
                    .standalone_client_auth
                    .then_some(&config.standalone_upstreams),
            },
            body,
            headers,
            idle_timeout,
            shutdown,
            probe,
            &mut usage,
        );
        if path == "/v1/messages" {
            if let ProxyOutcome::Committed { status } = outcome {
                if status < 400 {
                    if let Some(usage) = usage.finish() {
                        publish_status_usage(status_usage, provider, &model, usage, "unavailable");
                    }
                }
            }
        }
        Ok(())
    });
    if let Some(beats) = heartbeat {
        let succeeded = matches!(outcome, ProxyOutcome::Committed { status } if status < 400);
        let target =
            succeeded.then(|| heartbeat_target(base_url, path, body, headers, injected_api_key));
        beats.finish(&conversation_key.id, target, Instant::now());
    }
    if let Err(error) = routed {
        let failure = PipelineError::Translate(crate::codex_translate::TranslateError::Invalid(
            error.to_string(),
        ));
        let _ = write_pipeline_error(stream, &failure, None);
        return ProxyOutcome::local(500);
    }
    outcome
}

/// #2046: what the cache heartbeat replays: the request exactly as this hop
/// sent it upstream (forwarded headers, injected credential, same body).
fn heartbeat_target(
    base_url: &str,
    path: &str,
    body: &[u8],
    headers: &[(String, String)],
    injected_api_key: Option<&str>,
) -> crate::cache_heartbeat::Target {
    let mut sent: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| proxy_forwards_header(name, injected_api_key.is_some()))
        .cloned()
        .collect();
    if let Some(api_key) = injected_api_key {
        sent.push(("Authorization".to_string(), format!("Bearer {api_key}")));
    }
    crate::cache_heartbeat::Target {
        url: format!("{}{path}", base_url.trim_end_matches('/')),
        headers: sent,
        body: body.to_vec(),
    }
}

/// Tool type of Anthropic's server-side advisor (#1529).
const ADVISOR_TOOL_TYPE_PREFIX: &str = "advisor_";
/// `anthropic-beta` token that pairs with the advisor tool entry (#1529).
const ADVISOR_BETA_PREFIX: &str = "advisor-tool-";

/// Remove the advisor capability from a request bound for an
/// Anthropic-compatible, non-Claude upstream (#1529).
///
/// The capability is paired: an `advisor_*` entry in `tools[]` plus an
/// `advisor-tool-*` value in `anthropic-beta`. Claude Code's gateway contract
/// says the feature only turns off quietly when both halves are absent
/// together; forwarding either half alone to OpenRouter/DeepSeek/Kimi yields a
/// hard 400 and disables `/advisor` for the session. No compat upstream runs
/// the advisor loop, so both halves are always stripped together. A body that
/// does not parse is forwarded untouched (the upstream owns that error), and
/// the header half is still dropped because it can never be honoured here.
fn strip_advisor_capability(
    body: &[u8],
    headers: &[(String, String)],
) -> (Vec<u8>, Vec<(String, String)>) {
    let mut body_out = body.to_vec();
    if let Ok(mut request) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(tools) = request
            .get_mut("tools")
            .and_then(serde_json::Value::as_array_mut)
        {
            let before = tools.len();
            tools.retain(|tool| {
                !tool
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| kind.starts_with(ADVISOR_TOOL_TYPE_PREFIX))
            });
            if tools.len() != before {
                if tools.is_empty() {
                    if let Some(object) = request.as_object_mut() {
                        object.remove("tools");
                        object.remove("tool_choice");
                    }
                }
                body_out = serde_json::to_vec(&request).unwrap_or(body_out);
            }
        }
    }
    let headers_out = headers
        .iter()
        .filter_map(|(name, value)| {
            if !name.eq_ignore_ascii_case("anthropic-beta") {
                return Some((name.clone(), value.clone()));
            }
            let kept: Vec<&str> = value
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty() && !token.starts_with(ADVISOR_BETA_PREFIX))
                .collect();
            (!kept.is_empty()).then(|| (name.clone(), kept.join(",")))
        })
        .collect();
    (body_out, headers_out)
}

/// Unified token counting has one Anthropic-compatible contract: ordinary
/// Claude model IDs proxy upstream, while synthetic provider routes return an
/// explicit 404 so Claude Code falls back to its documented local estimation.
#[allow(clippy::too_many_arguments)]
fn serve_unified_count_tokens(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    shutdown: &AtomicBool,
    conversations: &ConversationStore,
    body: &[u8],
    conversation_key: &ConversationKey,
    headers: &[(String, String)],
    cache_health: &SharedCacheHealth,
) {
    let GatewayMode::Unified(unified) = &config.gateway_mode else {
        unreachable!("unified token-count dispatch is guarded by the caller");
    };
    let request: serde_json::Value = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => {
            let _ = write_error(stream, 400);
            return;
        }
    };
    let Some(model) = request.get("model").and_then(serde_json::Value::as_str) else {
        let _ = write_error(stream, 400);
        return;
    };
    let catalog = provider_catalog::model_by_discovery_id(model)
        .or_else(|| provider_catalog::non_claude_model_by_any_id(model));
    if model.starts_with("clud-claude-") && catalog.is_none() {
        let _ = write_error(stream, 400);
        return;
    }
    if catalog.is_some() {
        let response = br#"{"error":{"type":"not_found_error","message":"token counting is not supported for this unified provider route"}}"#;
        let _ = write_response(stream, 404, "application/json", response, false);
        return;
    }
    // Token counting is never failed over: it is a cheap advisory call whose
    // answer is provider-specific, so a count from a fallback route would be
    // wrong rather than merely late.
    serve_unified_anthropic_proxy(
        config,
        stream,
        conversations,
        conversation_key,
        ConversationRoute::Claude,
        &unified.anthropic_base_url,
        "/v1/messages/count_tokens",
        body,
        headers,
        None,
        config.stream_idle_timeout,
        shutdown,
        false,
        cache_health,
        &config.status_usage,
        unified.redactor_for(ModelProvider::Claude, Some(model)),
        None,
    );
}

fn unified_catalog_ids(config: &UnifiedGatewayConfig) -> Vec<&'static str> {
    provider_catalog::MODELS
        .iter()
        .filter(|entry| config.provider_available(entry.provider))
        .filter_map(|entry| entry.discovery_id)
        .collect()
}

/// Add a freshly resolved native credential only to standalone Claude requests.
fn standalone_claude_headers(
    config: &BridgeConfig,
    route: ConversationRoute,
    headers: &[(String, String)],
) -> Result<Option<Vec<(String, String)>>, ()> {
    if !config.standalone_client_auth || route != ConversationRoute::Claude {
        return Ok(None);
    }
    let credential = config
        .standalone_claude_credential
        .as_ref()
        .and_then(|source| source.credential())
        .ok_or(())?;
    let (name, secret, bearer) = match credential {
        StandaloneClaudeCredential::ApiKey(key) => ("x-api-key", key, false),
        StandaloneClaudeCredential::Bearer(token) => ("Authorization", token, true),
    };
    if secret.is_empty() || !secret.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(());
    }
    let value = if bearer {
        format!("Bearer {secret}")
    } else {
        secret
    };
    let mut managed: Vec<_> = headers
        .iter()
        .filter(|(name, _)| {
            !name.eq_ignore_ascii_case("authorization") && !name.eq_ignore_ascii_case("x-api-key")
        })
        .cloned()
        .collect();
    managed.push((name.to_string(), value));
    Ok(Some(managed))
}

/// Proxy an Anthropic-compatible Messages response without buffering its body.
/// The caller's Claude credential is retained only for native Claude requests;
/// DeepSeek injects its vault credential and never receives caller credentials.
struct AnthropicProxyTarget<'a> {
    base_url: &'a str,
    path: &'a str,
    injected_api_key: Option<&'a str>,
    redaction: Option<&'a SharedSession>,
    /// `Some` when the caller asked for an OpenAI `chat.completion`; the
    /// upstream body is then buffered and translated before it is written.
    chat: Option<ChatTranslation<'a>>,
    sanitize_errors: bool,
    sockets: Option<&'a Arc<crate::serve_transport::UpstreamSockets>>,
}

fn redact_request<'a>(
    body: &'a [u8],
    session: Option<&SharedSession>,
) -> io::Result<std::borrow::Cow<'a, [u8]>> {
    let Some(session) = session else {
        return Ok(std::borrow::Cow::Borrowed(body));
    };
    session
        .lock()
        .map_err(|_| io::Error::other("secret redaction state unavailable"))?
        .redact_body(body)
        .map_err(|_| io::Error::other("invalid request for secret redaction"))
}

/// What one proxy attempt did to the client connection.
///
/// The distinction is the whole basis of failover: until a byte has been
/// written the status is still ours to choose, so a route-terminal failure can
/// be replayed elsewhere and the client never learns a provider declined.
/// Afterwards the status is committed and the only honest move is to finish
/// the turn.
#[derive(Debug)]
enum ProxyOutcome {
    /// Bytes reached the client. Nothing may be retried. Carries the status so
    /// the caller can tell a served turn from a committed failure: only the
    /// former is evidence that the route recovered.
    Committed { status: u16 },
    /// Nothing was written. The route declined with this verdict.
    Declined(RouteVerdict),
}

impl ProxyOutcome {
    /// A status the gateway produced itself, with no upstream verdict behind
    /// it. Treated as a committed failure so it never clears a cooldown.
    const fn local(status: u16) -> Self {
        Self::Committed { status }
    }
}

/// Bounded prefix read from a failing upstream so it can be classified.
///
/// Large enough for any provider error envelope, small enough that a
/// misbehaving upstream cannot make the gateway buffer a response — the
/// success path still streams without buffering.
const FAILURE_PREFIX_BYTES: usize = 8 * 1024;

/// Statuses worth reading a body for before committing. Everything else is
/// either a success or a failure whose meaning the status already fixes, and
/// buffering those would only delay the client.
///
/// `400` is probed despite that rule because a status alone no longer settles a
/// 400: a guardrail rejection and a malformed request share the status and
/// differ completely in whether another route can serve the bytes. Reading the
/// prefix is free when it does not decline — it is re-emitted verbatim — so the
/// only cost is the buffer on rejections that were going to be read anyway.
fn may_be_route_terminal(status: u16) -> bool {
    matches!(status, 400 | 401 | 402 | 403 | 429)
}

/// Whether a `ureq` transport failure was a *timeout* rather than a dead peer.
///
/// `ureq` 2.x reports both as `ErrorKind::Io`, so the distinction has to be
/// recovered from the error chain: its timeout path builds an `io::Error` with
/// `ErrorKind::TimedOut`, normalizing the `WouldBlock` a POSIX socket timeout
/// actually produces. Without this the bridge answered a hung upstream with
/// the same 502 it uses for an unreachable one, and the harness could not tell
/// "retry me" from "this route is broken" (#1263).
fn transport_timed_out(error: &ureq::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            return io.kind() == std::io::ErrorKind::TimedOut;
        }
        source = std::error::Error::source(current);
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn serve_anthropic_proxy(
    stream: &mut TcpStream,
    target: AnthropicProxyTarget<'_>,
    body: &[u8],
    headers: &[(String, String)],
    idle_timeout: Duration,
    shutdown: &AtomicBool,
    probe: bool,
    usage: &mut StreamingUsage,
) -> ProxyOutcome {
    let redacted = match redact_request(body, target.redaction) {
        Ok(body) => body,
        Err(_) => {
            let _ = write_error(stream, 500);
            return ProxyOutcome::local(500);
        }
    };
    let request = build_proxy_request(&target, headers, idle_timeout);
    let response = match send_proxy_request(stream, request, &redacted) {
        Ok(response) => response,
        Err(outcome) => return outcome,
    };
    let status = response.status();
    if target.sanitize_errors && status >= 400 && !probe {
        return write_sanitized_failure(stream, status, target.chat.is_some(), idle_timeout);
    }
    let content_type = response
        .header("content-type")
        .unwrap_or("application/json")
        .to_string();
    let retry_after = response.header("retry-after").map(str::to_string);
    let request_id = response.header("request-id").map(str::to_string);
    // Captured before `into_reader` consumes the response, so a probe can
    // classify without a second round trip.
    let probe_headers: Vec<(String, String)> = if probe && may_be_route_terminal(status) {
        ["retry-after", "x-request-id", "cf-ray"]
            .into_iter()
            .filter_map(|name| {
                response
                    .header(name)
                    .map(|value| (name.to_string(), value.to_string()))
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut reader = response.into_reader();

    // Pre-commit probe. Nothing has been written yet, so if this route is spent
    // the caller can replay the identical body elsewhere and the client never
    // sees that a provider declined. A prefix that does not read as
    // route-terminal falls through and is re-emitted below, so the probe costs
    // the client nothing.
    let mut prefix = if probe && may_be_route_terminal(status) {
        match probe_route_prefix(reader.as_mut(), status, &probe_headers) {
            Ok(prefix) => prefix,
            Err(verdict) => return ProxyOutcome::Declined(verdict),
        }
    } else {
        Vec::new()
    };
    if status == 200 && target.path == "/v1/messages" {
        match inspect_messages_response(reader.as_mut(), &content_type) {
            Ok(checked) => prefix.extend_from_slice(&checked),
            Err(verdict) => return ProxyOutcome::Declined(verdict),
        }
    }

    let head = ProxyResponseHead {
        status,
        content_type: &content_type,
        retry_after,
        request_id,
    };
    if let Some(chat) = target.chat {
        return relay_translated_chat(
            stream,
            chat,
            &head,
            prefix,
            reader.as_mut(),
            target.redaction,
            idle_timeout,
            shutdown,
            usage,
        );
    }
    if let Some(session) = target.redaction {
        if content_type
            .to_ascii_lowercase()
            .contains("text/event-stream")
        {
            if !write_proxy_head(stream, &head, &[], idle_timeout) {
                return ProxyOutcome::Committed { status };
            }
            let source = io::Cursor::new(prefix).chain(reader);
            let mut restoring =
                crate::secret_redaction::stream::RestoringReader::new(source, Arc::clone(session));
            let relayed = relay_proxy_body(stream, &mut restoring, shutdown, usage);
            finish_proxy_body(stream, relayed, &content_type);
            return ProxyOutcome::Committed { status };
        }
        if status == 200 && target.path == "/v1/messages" {
            prefix = match restore_response(&prefix, session) {
                Ok(restored) => restored,
                Err(_) => return ProxyOutcome::Declined(RouteVerdict::ProviderFault),
            };
        }
    }
    if !write_proxy_head(stream, &head, &prefix, idle_timeout) {
        return ProxyOutcome::Committed { status };
    }
    usage.feed(&prefix);
    let relayed = relay_proxy_body(stream, reader.as_mut(), shutdown, usage);
    finish_proxy_body(stream, relayed, &content_type);
    ProxyOutcome::Committed { status }
}

fn restore_response(body: &[u8], session: &SharedSession) -> io::Result<Vec<u8>> {
    session
        .lock()
        .map_err(|_| io::Error::other("secret redaction state unavailable"))?
        .restore_body(body)
        .map(|body| body.into_owned())
        .map_err(|_| io::Error::other("invalid response for secret restoration"))
}

/// Largest upstream body a translated turn will buffer.
///
/// A translated response cannot stream through unchanged -- every chunk has to
/// be re-framed before the client sees it -- so this route buffers by
/// construction. The cap is what stops a runaway upstream from turning that
/// into unbounded memory: past it the client gets an OpenAI error it can retry
/// instead of a stalled turn.
const TRANSLATED_BODY_LIMIT: u64 = 64 * 1024 * 1024;

/// Buffer one upstream Messages response, translate it into the OpenAI shape
/// the client asked for, and write it back.
///
/// Errors are translated too: a status the upstream chose (a guardrail
/// rejection, a rate limit) reaches the client as an OpenAI error envelope with
/// that same status, so the client's own retry logic still keys on it.
#[allow(clippy::too_many_arguments)]
fn relay_translated_chat(
    stream: &mut TcpStream,
    chat: ChatTranslation<'_>,
    head: &ProxyResponseHead<'_>,
    prefix: Vec<u8>,
    reader: &mut dyn Read,
    redaction: Option<&SharedSession>,
    idle_timeout: Duration,
    shutdown: &AtomicBool,
    usage: &mut StreamingUsage,
) -> ProxyOutcome {
    let status = head.status;
    let chat_stream = head
        .content_type
        .to_ascii_lowercase()
        .contains("text/event-stream");
    let mut body = prefix;
    let overrun = read_translated_body(reader, &mut body, shutdown).is_err();
    usage.feed(&body);
    if overrun {
        let message = format!(
            "upstream response exceeded the {TRANSLATED_BODY_LIMIT}-byte translation buffer"
        );
        return write_chat_error(stream, 502, &message, idle_timeout);
    }
    let restored = match redaction {
        Some(session) => match restore_response(&body, session) {
            Ok(restored) => restored,
            Err(_) => {
                return write_chat_error(
                    stream,
                    502,
                    "upstream response could not be restored for the client",
                    idle_timeout,
                );
            }
        },
        None => body,
    };
    if status >= 400 {
        // The upstream's own failure: keep its status, but in the client's
        // shape, with whatever the upstream said as the message.
        let message = upstream_error_message(&restored);
        return write_chat_error(stream, status, &message, idle_timeout);
    }
    let (out_status, out_type, payload) = if chat_stream {
        (
            200,
            "text/event-stream",
            crate::openai_ingress::render_chat_stream(chat.model, &restored),
        )
    } else {
        match crate::openai_ingress::render_chat_body(chat.model, &restored) {
            Ok(payload) => (200, "application/json", payload),
            Err(error) => {
                return write_chat_error(
                    stream,
                    502,
                    &format!("upstream response was not a Messages body: {error}"),
                    idle_timeout,
                );
            }
        }
    };
    let translated = ProxyResponseHead {
        status: out_status,
        content_type: out_type,
        retry_after: head.retry_after.clone(),
        request_id: head.request_id.clone(),
    };
    if !write_proxy_head(stream, &translated, &payload, idle_timeout) {
        return ProxyOutcome::Committed { status: out_status };
    }
    finish_proxy_body(
        stream,
        ProxyRelay {
            forwarded: payload.len(),
            stalled: None,
        },
        out_type,
    );
    ProxyOutcome::Committed { status: out_status }
}

/// Read the rest of a translated turn's upstream body into `body`, stopping at
/// the translation buffer's ceiling, or at shutdown.
fn read_translated_body(
    reader: &mut dyn Read,
    body: &mut Vec<u8>,
    shutdown: &AtomicBool,
) -> Result<(), ()> {
    let mut chunk = [0_u8; 8192];
    while (body.len() as u64) <= TRANSLATED_BODY_LIMIT {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => body.extend_from_slice(&chunk[..count]),
            Err(_) => return Err(()),
        }
    }
    Err(())
}

/// The most useful one-line message an upstream failure body carries.
fn upstream_error_message(body: &[u8]) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let message = parsed.as_ref().and_then(|error| {
        error
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| error.get("message").and_then(serde_json::Value::as_str))
    });
    match message {
        Some(message) => message.to_string(),
        None if body.is_empty() => "the upstream route returned an empty failure body".to_string(),
        None => "the upstream route refused this turn".to_string(),
    }
}

/// Answer a suppressed upstream failure in the shape the caller speaks.
///
/// The standalone surface hides upstream error detail. `write_error` emits the
/// Anthropic envelope, so a chat caller has to be answered here instead: it
/// cannot parse the Anthropic shape, and translating the status but not the
/// body would hand it a payload it cannot read.
fn write_sanitized_failure(
    stream: &mut TcpStream,
    status: u16,
    chat: bool,
    idle_timeout: Duration,
) -> ProxyOutcome {
    if chat {
        return write_chat_error(
            stream,
            status,
            "the upstream route refused this turn",
            idle_timeout,
        );
    }
    let _ = write_error(stream, status);
    ProxyOutcome::local(status)
}

/// Write one OpenAI error envelope and close the connection.
fn write_chat_error(
    stream: &mut TcpStream,
    status: u16,
    message: &str,
    idle_timeout: Duration,
) -> ProxyOutcome {
    let payload = crate::openai_ingress::render_chat_error(status, message);
    let head = ProxyResponseHead {
        status,
        content_type: "application/json",
        retry_after: None,
        request_id: None,
    };
    if !write_proxy_head(stream, &head, &payload, idle_timeout) {
        return ProxyOutcome::Committed { status };
    }
    finish_proxy_body(
        stream,
        ProxyRelay {
            forwarded: payload.len(),
            stalled: None,
        },
        "application/json",
    );
    ProxyOutcome::Committed { status }
}

fn finish_proxy_body(stream: &mut TcpStream, relayed: ProxyRelay, content_type: &str) {
    if let Some(error) = relayed.stalled {
        report_proxy_stall(stream, &error, relayed.forwarded, content_type);
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

const MESSAGE_JSON_LIMIT: u64 = 16 * 1024 * 1024;

/// Verify the response before committing HTTP 200 to Claude Code. Once that
/// status is sent, a malformed provider body can no longer use the ladder.
fn inspect_messages_response(
    reader: &mut dyn Read,
    content_type: &str,
) -> Result<Vec<u8>, RouteVerdict> {
    if content_type
        .to_ascii_lowercase()
        .contains("text/event-stream")
    {
        return inspect_first_stream_event(reader);
    }
    if !content_type.to_ascii_lowercase().contains("json") {
        return Ok(Vec::new());
    }
    let mut body = Vec::new();
    reader
        .take(MESSAGE_JSON_LIMIT + 1)
        .read_to_end(&mut body)
        .map_err(|_| RouteVerdict::ProviderFault)?;
    if body.len() as u64 > MESSAGE_JSON_LIMIT {
        return Err(RouteVerdict::ProviderFault);
    }
    let message: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| RouteVerdict::ProviderFault)?;
    if message.get("type").and_then(serde_json::Value::as_str) != Some("message")
        || message.get("role").and_then(serde_json::Value::as_str) != Some("assistant")
        || !message
            .get("content")
            .is_some_and(serde_json::Value::is_array)
        || !message.get("id").is_some_and(serde_json::Value::is_string)
        || !message
            .get("model")
            .is_some_and(serde_json::Value::is_string)
        || !message
            .get("usage")
            .is_some_and(serde_json::Value::is_object)
    {
        return Err(RouteVerdict::ProviderFault);
    }
    Ok(body)
}

/// A stream with no SSE event has the same provider fault as a non-Message
/// JSON retry. Keep the first event's bytes so forwarding stays lossless.
fn inspect_first_stream_event(reader: &mut dyn Read) -> Result<Vec<u8>, RouteVerdict> {
    let mut prefix = Vec::new();
    let mut chunk = [0_u8; 1024];
    while prefix.len() < FAILURE_PREFIX_BYTES {
        let count = reader
            .read(&mut chunk)
            .map_err(|_| RouteVerdict::ProviderFault)?;
        if count == 0 {
            break;
        }
        prefix.extend_from_slice(&chunk[..count]);
        if has_complete_sse_event(&prefix) {
            return Ok(prefix);
        }
    }
    Err(RouteVerdict::ProviderFault)
}

fn has_complete_sse_event(bytes: &[u8]) -> bool {
    let mut start = 0;
    let mut has_data = false;
    for (end, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let mut line = &bytes[start..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            if has_data {
                return true;
            }
            has_data = false;
        } else if line.starts_with(b"data:") {
            has_data = true;
        }
        start = end + 1;
    }
    false
}

/// The upstream request for one proxy hop: byte-idle timeouts, and only the
/// caller headers the upstream should see.
fn build_proxy_request(
    target: &AnthropicProxyTarget<'_>,
    headers: &[(String, String)],
    idle_timeout: Duration,
) -> ureq::Request {
    // #1263: byte-idle, not a whole-request deadline. `Request::timeout()` is
    // documented as covering "reading the response body" and *takes precedence
    // over* `timeout_read()`, so the budget this hop is handed
    // (`stream_idle_timeout`, DD-028) was really a total one: a healthy stream
    // that simply ran longer than it -- which is ordinary for a model that
    // thinks for minutes and keeps emitting deltas -- was cut off mid-turn,
    // while a socket that had gone quiet was only noticed after the same
    // absolute wall-clock had elapsed. The sibling Codex upstream hop has
    // always read this way (`codex_upstream`); the proxy hop was the outlier,
    // and this is the shape the constant's own doc comment claims.
    let connect_timeout = if target.sockets.is_some() {
        idle_timeout.min(Duration::from_secs(10))
    } else {
        idle_timeout
    };
    let builder = ureq::AgentBuilder::new()
        .timeout_connect(connect_timeout)
        .timeout_read(idle_timeout)
        .timeout_write(idle_timeout);
    let agent = match target.sockets {
        Some(sockets) => crate::serve_transport::tracked_agent(builder, sockets),
        None => builder.build(),
    };
    let mut request = agent
        .post(&format!(
            "{}{}",
            target.base_url.trim_end_matches('/'),
            target.path
        ))
        .set("Content-Type", "application/json");
    for (name, value) in headers {
        if proxy_forwards_header(name, target.injected_api_key.is_some()) {
            request = request.set(name, value);
        }
    }
    if let Some(api_key) = target.injected_api_key {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }
    request
}

/// Whether one caller header is forwarded upstream. A caller credential is
/// dropped when the gateway injects its own.
fn proxy_forwards_header(name: &str, injects_api_key: bool) -> bool {
    let forwarded = name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("x-api-key")
        || name.eq_ignore_ascii_case("anthropic-version")
        || name.to_ascii_lowercase().starts_with("anthropic-")
        || name.eq_ignore_ascii_case("accept");
    let hop_by_hop = name.eq_ignore_ascii_case("host")
        || name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case(UNIFIED_GATEWAY_TOKEN_HEADER);
    let caller_credential =
        name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key");
    forwarded && !hop_by_hop && (!injects_api_key || !caller_credential)
}

/// Send the request. A transport failure is answered here, while nothing has
/// been written, and returned as the outcome.
fn send_proxy_request(
    stream: &mut TcpStream,
    request: ureq::Request,
    body: &[u8],
) -> Result<ureq::Response, ProxyOutcome> {
    match request.send_bytes(body) {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(_, response)) => Ok(response),
        Err(error) if transport_timed_out(&error) => {
            // #1263: a *timeout* is not a broken gateway hop. Answering it 502
            // told the harness "this route is dead" when the honest answer is
            // "retry me": 502 is not retried, 504 `timeout_error` is, and the
            // mapping already existed for every other timeout the bridge
            // raises. Nothing has been written yet, so the status is still
            // ours to choose (DD-029).
            let _ = write_response(
                stream,
                504,
                "application/json",
                br#"{"error":{"type":"timeout_error","message":"upstream request timed out"}}"#,
                false,
            );
            Err(ProxyOutcome::local(504))
        }
        Err(_) => {
            let _ = write_response(
                stream,
                502,
                "application/json",
                br#"{"error":{"type":"api_error","message":"gateway upstream unavailable"}}"#,
                false,
            );
            Err(ProxyOutcome::local(502))
        }
    }
}

/// Read a bounded prefix of a possibly route-terminal response and classify
/// it. `Err` carries the verdict when the route should fail over; `Ok` the
/// consumed prefix, which the caller must re-emit.
fn probe_route_prefix(
    reader: &mut dyn Read,
    status: u16,
    probe_headers: &[(String, String)],
) -> Result<Vec<u8>, RouteVerdict> {
    let mut prefix = Vec::new();
    let mut window = [0_u8; 1024];
    while prefix.len() < FAILURE_PREFIX_BYTES {
        match reader.read(&mut window) {
            Ok(0) | Err(_) => break,
            Ok(count) => prefix.extend_from_slice(&window[..count]),
        }
    }
    let text = String::from_utf8_lossy(&prefix);
    let failure = UpstreamFailure::from_parts(
        status,
        |name| {
            probe_headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        },
        &text,
        crate::route_health::MAX_COOLDOWN,
    );
    let verdict = RouteVerdict::from_failure(&failure);
    if verdict.fails_over() {
        return Err(verdict);
    }
    Ok(prefix)
}

/// The upstream response metadata the proxy relays to the client.
struct ProxyResponseHead<'a> {
    status: u16,
    content_type: &'a str,
    retry_after: Option<String>,
    request_id: Option<String>,
}

/// Commit the response: status line, headers, and any probed prefix as the
/// first chunk. `false` means the client went away.
fn write_proxy_head(
    stream: &mut TcpStream,
    head: &ProxyResponseHead<'_>,
    prefix: &[u8],
    idle_timeout: Duration,
) -> bool {
    let (status, content_type) = (head.status, head.content_type);
    let _ = stream.set_write_timeout(Some(idle_timeout));
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Upstream\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nCache-Control: no-store\r\nConnection: close\r\n"
    );
    if let Some(retry_after) = &head.retry_after {
        let _ = write!(stream, "Retry-After: {retry_after}\r\n");
    }
    if let Some(request_id) = &head.request_id {
        let _ = write!(stream, "request-id: {request_id}\r\n");
    }
    if stream.write_all(b"\r\n").is_err() || stream.flush().is_err() {
        return false;
    }
    // Anything the probe already consumed has to go out first, or the client
    // would receive a truncated error envelope.
    prefix.is_empty()
        || write!(stream, "{:x}\r\n", prefix.len())
            .and_then(|()| stream.write_all(prefix))
            .and_then(|()| stream.write_all(b"\r\n"))
            .and_then(|()| stream.flush())
            .is_ok()
}

/// How the upstream body relay ended.
struct ProxyRelay {
    forwarded: usize,
    stalled: Option<io::Error>,
}

/// Relay the upstream body to the client as chunks until it ends, the client
/// goes away, or the bridge shuts down.
fn relay_proxy_body(
    stream: &mut TcpStream,
    reader: &mut dyn Read,
    shutdown: &AtomicBool,
    usage: &mut StreamingUsage,
) -> ProxyRelay {
    let mut chunk = [0_u8; 8192];
    let mut forwarded = 0_usize;
    let mut stalled: Option<io::Error> = None;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                usage.feed(&chunk[..count]);
                forwarded += count;
                if write!(stream, "{count:x}\r\n")
                    .and_then(|()| stream.write_all(&chunk[..count]))
                    .and_then(|()| stream.write_all(b"\r\n"))
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    break;
                }
            }
            Err(error) => {
                stalled = Some(error);
                break;
            }
        }
    }
    ProxyRelay { forwarded, stalled }
}

/// Report an upstream read failure after the response was committed.
fn report_proxy_stall(
    stream: &mut TcpStream,
    error: &io::Error,
    forwarded: usize,
    content_type: &str,
) {
    // #1263, DD-029: the status line went out with the first frame, so the
    // only channel left for a mid-stream failure is in-band. This arm used
    // to write the terminating chunk and close, which on the wire is
    // byte-identical to a stream that ended because the model finished --
    // a client still waiting on its `message_stop` saw a truncated turn
    // with no reason attached, and nothing anywhere said the upstream had
    // gone quiet. That is the silent stall DD-029 exists to prevent, and
    // it is what a byte-idle read (above) now reports instead of hiding.
    let timed_out = matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    );
    eprintln!(
        "[clud] codex bridge: upstream stream {} after {forwarded} bytes: {error}",
        if timed_out { "stalled" } else { "failed" }
    );
    if content_type.starts_with("text/event-stream") {
        let frame = crate::codex_sse::anthropic_frame(
            "error",
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": if timed_out { "timeout_error" } else { "api_error" },
                    "message": if timed_out {
                        "model request stalled: no upstream bytes arrived before the idle timeout"
                    } else {
                        "upstream stream ended before the model finished"
                    },
                }
            }),
        );
        let _ = write!(stream, "{:x}\r\n", frame.len())
            .and_then(|()| stream.write_all(frame.as_bytes()))
            .and_then(|()| stream.write_all(b"\r\n"))
            .and_then(|()| stream.flush());
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_codex_discovery_messages(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    shutdown: &AtomicBool,
    conversations: &ConversationStore,
    body: &[u8],
    streaming: bool,
    conversation_key: &ConversationKey,
    log: Option<&SharedBridgeLog>,
    cache_health: &SharedCacheHealth,
) {
    let mut request: serde_json::Value = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => {
            let _ = write_error(stream, 400);
            return;
        }
    };
    let Some(model) = request.get("model").and_then(serde_json::Value::as_str) else {
        let _ = write_error(stream, 400);
        return;
    };
    let (base, effort) = provider_catalog::split_effort_suffix(model);
    // #1010 item 3: `codex_translate::resolve_selection` trims and treats an
    // empty selection as "unspecified -> serve the default". Testing the raw
    // string here made `{"model":""}` and `{"model":" claude-opus-5"}` take the
    // refusal instead, the first of which rendered as the nonsense
    // `unknown clud Codex model ''`. Same predicate, same answer.
    let base = base.trim();
    // A persisted or continued session can name a Codex row by wire ID or CLI
    // alias instead of its discovery ID, exactly as on the unified route.
    //
    // Two different questions, deliberately two lookups (#1010 item 1):
    // `discovered` is *routing* -- only a Codex row can be served here.
    // `known_to_clud` is *wording* -- whether clud has heard of the ID at all,
    // which includes the Claude tier aliases and OpenRouter rows that the
    // routing lookup excludes. Using the routing chain for both told a user
    // that `opus` does not exist.
    let discovered = provider_catalog::model_by_discovery_id(base)
        .or_else(|| provider_catalog::non_claude_model_by_any_id(base))
        .filter(|entry| entry.provider == ModelProvider::Codex);
    let known_to_clud = provider_catalog::model_by_any_id(base).is_some();
    // #1022: an ID the user pinned at launch is theirs, even when clud's
    // catalog has no row for it.
    //
    // `ModelSpec::parse` deliberately forwards an unknown *full* id
    // (`codex_model.rs`: "`gpt-5.7-whatever` is how a user reaches a model
    // released after this table was written, and refusing it would make the
    // table a gate we would have to keep updating"). #1005 made the table
    // exactly that gate on the request path, so `--model gpt-5.7-nova`
    // launched fine and then 400d on every turn.
    //
    // Matching on the launch selection rather than restoring blanket
    // passthrough is what keeps #1005's fix intact. The two are distinguished
    // by provenance, not by string shape: an ID the *user* typed is honored,
    // while one the *harness* invented -- Claude Code merges the gateway's
    // rows with its own built-in catalog, so `anthropic.*` rows the user never
    // chose do arrive here -- is still refused (#997). No string test can tell
    // those apart, which is why #1005 could not fix one without breaking the
    // other.
    let pinned_at_launch = config
        .default_model
        .as_ref()
        .is_some_and(|spec| spec.model.trim() == base);
    // Claude Code merges this gateway's rows with its own built-in catalog, so
    // IDs the gateway never advertised do arrive here. Only a `claude*` ID is
    // caller-owned -- `codex_translate::resolve_selection` maps those onto the
    // reviewed default. Anything else the bridge cannot resolve is refused
    // here: forwarding it unrewritten would translate an unknown ID to the
    // Codex upstream, which answers with an error about a model the user never
    // knowingly sent there (#997).
    if discovered.is_none()
        && !pinned_at_launch
        && !base.is_empty()
        && !base.to_ascii_lowercase().starts_with("claude")
    {
        let ids = provider_catalog::models_for_provider(ModelProvider::Codex)
            .filter_map(|entry| entry.discovery_id)
            .collect::<Vec<_>>()
            .join(", ");
        // Two different failures land here and they need different answers
        // (#1000): a model clud has never heard of is the caller's to fix,
        // while a model clud does know but this bridge does not serve is a
        // clud-side limit -- saying so stops the user debugging their own
        // model choice.
        let (message, reason) = if known_to_clud {
            (
                format!(
                    "clud knows the model '{base}', but this Codex gateway is not \
                     serving it; available IDs: {ids}"
                ),
                "model_not_served_here",
            )
        } else {
            (
                format!("unknown clud Codex model '{base}'; available IDs: {ids}"),
                "unknown_model",
            )
        };
        // #999: the terminal names the rejected model and the log did not, so a
        // session wedged by a model selection left no evidence behind. The two
        // cases stay as distinguishable here as they are to the user.
        record_model_rejection(log, 400, reason, base);
        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": message,
            }
        })
        .to_string();
        let _ = write_response(stream, 400, "application/json", body.as_bytes(), false);
        return;
    }
    // #1257: the launch's allowlist governs this route too. Claude-prefixed
    // and haiku ids pass through `allowlist_refusal`'s exemptions, so
    // DD-038's substitution and the harness's side-model calls are untouched.
    if refuse_if_outside_allowlist(stream, log, &config.allowed_models, base) {
        return;
    }
    // #1007: a built-in Anthropic row picked in `/model` arrives here as a
    // `claude*` id and is served by the launch selection -- silently, until
    // now. The picker cannot be constrained (DD-054), so the remedy is to
    // detect and report: one terminal line per session, one log event per
    // request. The substitution itself is unchanged; refusing would break the
    // harness's own side-model calls, which share the prefix.
    if is_anthropic_main_model_pick(base) {
        let served = config.default_model.as_ref().map_or_else(
            || {
                provider_catalog::reviewed_default_model(ModelProvider::Codex).map_or_else(
                    || "the Codex default".to_string(),
                    |entry| crate::codex_runtime::wire_id(entry.cli_id, entry.wire_id),
                )
            },
            ModelSpec::display,
        );
        warn_once_on_anthropic_pick(base, &served);
        record_model_substitution(log, base, &served);
    }
    if let Some(entry) = discovered {
        let target = codex_discovery_wire_model(config, entry, base);
        let wire_model =
            effort.map_or_else(|| target.clone(), |effort| format!("{target}@{effort}"));
        request["model"] = serde_json::Value::String(wire_model);
    }
    let rewritten = serde_json::to_vec(&request).unwrap_or_default();
    serve_messages(
        stream,
        config,
        shutdown,
        conversations,
        &rewritten,
        streaming,
        conversation_key,
        None,
        log,
        cache_health,
        None,
    );
}

fn codex_discovery_wire_model(
    config: &BridgeConfig,
    entry: provider_catalog::CatalogModel,
    requested: &str,
) -> String {
    if config.selected_model_cli_id.as_deref() == Some(entry.cli_id) {
        if let Some(model) = &config.default_model {
            return model.model.clone();
        }
    }
    if requested.eq_ignore_ascii_case(entry.wire_id) {
        return requested.to_string();
    }
    crate::codex_runtime::wire_id(entry.cli_id, entry.wire_id)
}

/// Serve one `POST /v1/messages`.
///
/// The status is chosen only while nothing has been written. Once the writer
/// has emitted a frame the response is committed, so a later failure is
/// reported in-band by the translator's own `error` event (already appended by
/// the pipeline) and the chunked body is simply terminated.
#[allow(clippy::too_many_arguments)]
fn serve_messages(
    stream: &mut TcpStream,
    config: &BridgeConfig,
    shutdown: &AtomicBool,
    conversations: &ConversationStore,
    body: &[u8],
    streaming: bool,
    conversation_key: &ConversationKey,
    unified_route: Option<ConversationRoute>,
    log: Option<&SharedBridgeLog>,
    cache_health: &SharedCacheHealth,
    redaction: Option<&SharedSession>,
) {
    let redacted = match redact_request(body, redaction) {
        Ok(body) => body,
        Err(_) => {
            let _ = write_error(stream, 500);
            return;
        }
    };
    let pipeline = match build_pipeline(config, log, conversation_key, redaction) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            let failure = PipelineError::Upstream(error);
            let _ = write_pipeline_error(stream, &failure, log);
            return;
        }
    };
    let message_id = new_message_id();
    let turn = MessagesTurn {
        config,
        shutdown,
        conversations,
        body: &redacted,
        conversation_key,
        unified_route,
        log,
        cache_health,
        pipeline: &pipeline,
        message_id: &message_id,
        redaction,
    };

    if streaming {
        serve_messages_streaming(stream, &turn);
    } else {
        serve_messages_complete(stream, &turn);
    }
}

/// One `POST /v1/messages` turn, once its pipeline is built.
struct MessagesTurn<'a> {
    config: &'a BridgeConfig,
    shutdown: &'a AtomicBool,
    conversations: &'a ConversationStore,
    body: &'a [u8],
    conversation_key: &'a ConversationKey,
    unified_route: Option<ConversationRoute>,
    log: Option<&'a SharedBridgeLog>,
    cache_health: &'a SharedCacheHealth,
    pipeline: &'a Pipeline<ResolvedCredentials>,
    message_id: &'a str,
    redaction: Option<&'a SharedSession>,
}

/// Enter the turn's unified route, marking a cache boundary on a switch, and
/// report whether the conversation's cache fuse has tripped. Runs inside the
/// conversation lock.
fn cache_fuse_tripped_on_entry(
    history: &mut crate::codex_history::ConversationHistory,
    turn: &MessagesTurn<'_>,
) -> bool {
    let conversation_id = &turn.conversation_key.id;
    if let Some(route) = turn.unified_route {
        if history.enter_route(route) {
            lock_cache_health(turn.cache_health).mark_boundary(conversation_id);
        }
    }
    lock_cache_health(turn.cache_health).health(conversation_id) == CacheHealth::FuseTripped
}

/// Restore translated frames and drain a partial token even on terminal errors.
fn stream_with_redaction(
    turn: &MessagesTurn<'_>,
    writer: &mut EventStreamWriter<'_>,
    history: &mut crate::codex_history::ConversationHistory,
) -> Result<crate::codex_pipeline::StreamSummary, PipelineError> {
    let mut restoring = turn
        .redaction
        .map(|session| crate::secret_redaction::stream::Frames::new(Arc::clone(session)));
    let result = {
        let mut sink = |frame: &str| -> Result<(), UpstreamError> {
            let restored;
            let frame = if let Some(restoring) = &mut restoring {
                restored = restoring
                    .rewrite(frame)
                    .map_err(|_| UpstreamError::Downstream("secret restoration failed"))?;
                &restored
            } else {
                frame
            };
            writer
                .write_frame(frame)
                .map_err(|_| UpstreamError::Downstream("client write failed"))
        };
        turn.pipeline.stream_with_history(
            turn.body,
            turn.message_id,
            turn.shutdown,
            &mut sink,
            history,
        )
    };
    if let Some(restoring) = &mut restoring {
        let tail = restoring.finish().map_err(|_| {
            PipelineError::Upstream(UpstreamError::Downstream("secret restoration failed"))
        })?;
        if !tail.is_empty() {
            writer.write_frame(&tail).map_err(|_| {
                PipelineError::Upstream(UpstreamError::Downstream("client write failed"))
            })?;
        }
    }
    result
}

/// The streaming (`"stream": true`) half of `serve_messages`.
fn serve_messages_streaming(stream: &mut TcpStream, turn: &MessagesTurn<'_>) {
    let (config, log, conversation_key) = (turn.config, turn.log, turn.conversation_key);
    let mut writer = EventStreamWriter::new(stream, config);
    let streamed = turn
        .conversations
        .with_history(&conversation_key.id, |history| {
            if cache_fuse_tripped_on_entry(history, turn) {
                return Ok(None);
            }
            let streamed = stream_with_redaction(turn, &mut writer, history);
            if let Ok(summary) = &streamed {
                if summary.history_append_rejected && writer.started() {
                    let _ = writer.finish();
                    summary.clear_history_after_client_commit(history);
                }
                // This remains inside the conversation lock: a queued sibling
                // must observe a newly armed fuse before it can reach upstream.
                record_cache_usage(
                    turn.cache_health,
                    &config.status_usage,
                    conversation_key,
                    &summary.model,
                    summary.usage,
                    log,
                );
            }
            Ok(Some(streamed))
        });
    match streamed {
        Ok(Some(Ok(summary))) => {
            report_stream_summary(turn, &summary);
            if !summary.history_append_rejected {
                let _ = writer.finish();
            }
        }
        Ok(Some(Err(error))) => {
            log_continuation_invariant(&error, conversation_key, log);
            if writer.started() {
                // Committed: the pipeline has already emitted a sanitized
                // `error` event, so just close the body cleanly.
                log_pipeline_error(&error, log);
                // ...but a drained account still deserves the banner. This
                // is the same failure as the pre-commit case; the only
                // difference is that a frame had already gone out, which
                // changes the status we can send and nothing else.
                warn_once_on_terminal_failure(&error);
                let _ = writer.finish();
            } else {
                let _ = write_pipeline_error(stream, &error, log);
            }
        }
        Err(error) => {
            let failure = PipelineError::Translate(
                crate::codex_translate::TranslateError::Invalid(error.to_string()),
            );
            let _ = write_pipeline_error(stream, &failure, log);
        }
        Ok(None) => write_cache_fuse_refusal(stream, log, conversation_key),
    }
}

/// Log what a completed stream reported: credential rechecks, repaired or
/// recovered outputs, and in-band failures.
fn report_stream_summary(turn: &MessagesTurn<'_>, summary: &crate::codex_pipeline::StreamSummary) {
    let log = turn.log;
    if summary.authentication_failure {
        mark_auth_recheck(&turn.config.auth_recheck, turn.pipeline);
    }
    if summary.orphaned_outputs_repaired > 0 {
        log_orphaned_outputs_repaired(log, summary.orphaned_outputs_repaired);
    }
    if summary.pending_outputs_recovered > 0 {
        log_pending_outputs_recovered(
            log,
            turn.conversation_key,
            summary.pending_outputs_recovered,
        );
    }
    if let Some(failure) = summary.in_band_failure.as_ref() {
        log_in_band_failure(
            log,
            failure,
            request_phase(turn.body),
            summary.request_shape.clone(),
        );
    }
    // A quota failure delivered inside a 200 SSE stream used to
    // produce HTTP 200, no log line even under
    // `CLUD_CODEX_BRIDGE_DEBUG=1`, and an abruptly truncated turn.
    // The status is committed by now, but silence is not forced.
    // The translator also marks authentication failures as
    // account-level, but they are not quota exhaustion: the next
    // turn may recover them under the credential lock.
    if summary.terminal_account_failure && !summary.authentication_failure {
        let error = PipelineError::Provider(ProviderFailure {
            kind: "billing_error".to_string(),
            message: IN_BAND_QUOTA_MESSAGE.to_string(),
            diagnostic: None,
        });
        log_pipeline_error(&error, log);
        warn_once_on_terminal_failure(&error);
    }
}

/// The non-streaming half of `serve_messages`: one complete JSON response.
fn serve_messages_complete(stream: &mut TcpStream, turn: &MessagesTurn<'_>) {
    let (config, log, conversation_key) = (turn.config, turn.log, turn.conversation_key);
    match turn
        .conversations
        .with_history(&conversation_key.id, |history| {
            if cache_fuse_tripped_on_entry(history, turn) {
                return Ok(None);
            }
            let completed = turn
                .pipeline
                .complete_with_history(turn.body, turn.message_id, turn.shutdown, history)
                .and_then(|completion| {
                    if completion.pending_outputs_recovered > 0 {
                        log_pending_outputs_recovered(
                            log,
                            conversation_key,
                            completion.pending_outputs_recovered,
                        );
                    }
                    let rendered = serde_json::to_vec(&completion.message).unwrap_or_default();
                    let rendered = if let Some(session) = turn.redaction {
                        restore_response(&rendered, session).map_err(|_| {
                            PipelineError::Upstream(UpstreamError::Downstream(
                                "secret restoration failed",
                            ))
                        })?
                    } else {
                        rendered
                    };
                    if write_response(stream, 200, "application/json", &rendered, false).is_ok() {
                        completion.clear_history_after_client_commit(history);
                    }
                    // Keep health accounting ordered with the history mutation so a
                    // queued sibling rechecks the fuse after this turn completes.
                    record_cache_usage(
                        turn.cache_health,
                        &config.status_usage,
                        conversation_key,
                        &completion.model,
                        completion.usage,
                        log,
                    );
                    Ok(())
                });
            Ok(Some(completed))
        }) {
        Ok(Some(Ok(()))) => {}
        Ok(Some(Err(error))) => {
            log_continuation_invariant(&error, conversation_key, log);
            if let PipelineError::Provider(failure) = &error {
                if failure.kind == "authentication_error" {
                    mark_auth_recheck(&config.auth_recheck, turn.pipeline);
                }
                if let Some(diagnostic) = &failure.diagnostic {
                    log_in_band_failure(
                        log,
                        &diagnostic.failure,
                        request_phase(turn.body),
                        diagnostic.request_shape.clone(),
                    );
                }
            }
            let _ = write_pipeline_error(stream, &error, log);
        }
        Err(error) => {
            let failure = PipelineError::Translate(
                crate::codex_translate::TranslateError::Invalid(error.to_string()),
            );
            let _ = write_pipeline_error(stream, &failure, log);
        }
        Ok(None) => write_cache_fuse_refusal(stream, log, conversation_key),
    }
}

/// Announce a terminal account failure on stderr, once per process.
///
/// Every other diagnostic the bridge produces is gated behind
/// `CLUD_CODEX_BRIDGE_DEBUG=1` or buried in the forensic log that nothing
/// reads back. A drained account is not a debugging detail: the session cannot
/// continue, no retry will fix it, and the user has to go do something in a
/// browser. It gets the one ungated line in the module.
///
/// Once per process, not per request: a failing turn can produce several of
/// these, and a banner repeated ten times is noise that trains people to
/// ignore it. Follows the `wedge_watchdog` warn-once-per-episode precedent.
fn warn_once_on_terminal_failure(error: &PipelineError) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !error.is_terminal_account_failure() {
        return;
    }
    if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // The leading `\x07` is a terminal bell: this is the failure most likely
    // to be waiting on a user who has looked away from an unattended run.
    eprintln!("\x07[clud] codex bridge: {}", error.client_message());
}

/// Whether a `claude*` id on the direct Codex route names a built-in
/// Anthropic model that something chose *after* launch, as opposed to the
/// harness's own background traffic (#1007).
///
/// Two facts make the prefix alone insufficient and this predicate possible:
///
/// - The cross route launches the harness with `--model <discovery-id>`
///   (`command::builder::gateway_model_selection`), so an ordinary foreground
///   turn arrives as `clud-claude-codex-*`, not `claude*`. A `claude*` main
///   model was therefore selected later: by the user, from the rows Claude
///   Code merges into `/model` behind clud's back (DD-054), or by a subagent
///   whose `model: opus` / `model: sonnet` alias the harness resolves to its
///   built-in id -- `apply_cross_route_overlay` deliberately does not pin the
///   `ANTHROPIC_DEFAULT_*_MODEL` tiers, the hijacking DD-054 rejects. Either
///   way the request is served by the launch selection and the user should
///   hear which.
/// - The harness's side-model calls (titles, summaries, the small fast model)
///   keep their built-in `claude-*-haiku*` ids regardless of `--model`. Those
///   are routine and are excluded by name.
fn is_anthropic_main_model_pick(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.starts_with("claude") && !lower.contains("haiku")
}

/// The one line a user gets when their `/model` pick is not what runs (#1007).
///
/// Once per process, like [`warn_once_on_terminal_failure`]: every turn after
/// the pick would repeat it, and the useful information -- which model is
/// actually being billed, and how to change it -- does not change.
fn warn_once_on_anthropic_pick(requested: &str, served: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let requested = requested
        .chars()
        .take(MAX_LOGGED_MODEL_CHARS)
        .collect::<String>();
    eprintln!(
        "[clud] codex bridge: '{requested}' is a built-in Anthropic model this Codex session \
         cannot serve; requests naming it (a /model pick, or a subagent's `model:` alias) run \
         on {served} instead. Use a \"Codex\" row in /model to change models."
    );
}

/// The forensic half of #1007: which id the harness asked for and which one
/// the bridge served in its place. Ambient, not notable -- the terminal line
/// already carries the user's attention, and a session that keeps working on
/// the substitute should not end with a "problems were recorded" hint.
fn record_model_substitution(log: Option<&SharedBridgeLog>, requested: &str, served: &str) {
    if let Some(log) = log {
        let requested = requested
            .chars()
            .take(MAX_LOGGED_MODEL_CHARS)
            .collect::<String>();
        lock_log(log).record_ambient(serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "model_substituted",
            "requested": requested,
            "served": served,
        }));
    }
}

/// Opt-in diagnostics. The bridge answers the harness with a sanitized error,
/// which is correct but leaves an operator with no way to tell a translation
/// bug from an upstream outage. This prints the classification only -- never an
/// upstream body, which can carry account identifiers.
fn log_pipeline_error(error: &PipelineError, log: Option<&SharedBridgeLog>) {
    if let Some(log) = log {
        let mut event = serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "pipeline_failure",
            "downstream_status": error.http_status(),
            "kind": pipeline_error_kind(error),
        });
        if let PipelineError::Upstream(UpstreamError::Status(failure)) = error {
            add_failure_fields(&mut event, failure);
        }
        lock_log(log).record(event);
    }
    if std::env::var_os("CLUD_CODEX_BRIDGE_DEBUG").is_some_and(|value| value == "1") {
        eprintln!(
            "[clud] codex bridge: {} -> HTTP {}",
            error,
            error.http_status()
        );
        // The classification, the correlation ids, and the scrubbed reason.
        // Without this an operator can see *that* a 502 happened but not
        // whether it came from the edge, the provider, or clud itself (#764).
        if let Some(diagnostic) = error.upstream_diagnostic() {
            eprintln!("[clud] codex bridge: upstream {diagnostic}");
        }
    }
}

fn log_continuation_invariant(
    error: &PipelineError,
    conversation_key: &ConversationKey,
    log: Option<&SharedBridgeLog>,
) {
    let PipelineError::ContinuationInvariant(failure) = error else {
        return;
    };
    let event = serde_json::json!({
        "ts_ms": unix_ms(),
        "event": "continuation_invariant_failure",
        "downstream_status": 400,
        "conversation_scope": conversation_key.scope(),
        "input_count": failure.input_count,
        "input_kinds": failure.input_kinds,
        "function_call_count": failure.function_call_count,
        "function_call_output_count": failure.function_call_output_count,
        "unmatched_call_count": failure.unmatched_call_count,
        "source": failure.source,
    });
    if let Some(log) = log {
        lock_log(log).record(event.clone());
    }
    if std::env::var_os("CLUD_CODEX_BRIDGE_DEBUG").is_some_and(|value| value == "1") {
        eprintln!("[clud] codex bridge: {event}");
    }
}

/// Record that a turn rewrote orphaned tool results to keep the Responses
/// API from rejecting it outright. Counts only — no call ids, no outputs.
///
/// Worth logging because the untreated form of this is a *permanent* 400
/// that wedges a session until the process restarts, and the bridge log is
/// what identified it.
fn log_orphaned_outputs_repaired(log: Option<&SharedBridgeLog>, repaired: usize) {
    let event = serde_json::json!({
        "ts_ms": unix_ms(),
        "event": "orphaned_outputs_repaired",
        "repaired_count": repaired,
        "reason": "history_compaction_elided_the_originating_call",
    });
    if let Some(log) = log {
        lock_log(log).record(event.clone());
    }
    if std::env::var_os("CLUD_CODEX_BRIDGE_DEBUG").is_some_and(|value| value == "1") {
        eprintln!("[clud] codex bridge: {event}");
    }
}

/// Record that canonical continuation assembly found real, previously omitted
/// tool outputs in Claude's full replay. Counts and scope only: the identifiers
/// and output payloads remain request-local.
fn log_pending_outputs_recovered(
    log: Option<&SharedBridgeLog>,
    conversation_key: &ConversationKey,
    recovered: usize,
) {
    let event = serde_json::json!({
        "ts_ms": unix_ms(),
        "event": "pending_outputs_recovered",
        "conversation_scope": conversation_key.scope(),
        "recovered_count": recovered,
    });
    if let Some(log) = log {
        lock_log(log).record(event.clone());
    }
    if std::env::var_os("CLUD_CODEX_BRIDGE_DEBUG").is_some_and(|value| value == "1") {
        eprintln!("[clud] codex bridge: {event}");
    }
}

fn log_in_band_failure(
    log: Option<&SharedBridgeLog>,
    failure: &InBandFailure,
    phase: &'static str,
    request_shape: serde_json::Value,
) {
    let event = serde_json::json!({
        "ts_ms": unix_ms(),
        "event": "in_band_upstream_failure",
        "upstream_status": 400,
        "category": failure.category,
        "code": failure.code,
        "request_id": failure.request_id,
        "phase": phase,
        "request_shape": request_shape,
    });
    if let Some(log) = log {
        lock_log(log).record(event.clone());
    }
    if std::env::var_os("CLUD_CODEX_BRIDGE_DEBUG").is_some_and(|value| value == "1") {
        eprintln!("[clud] codex bridge: {}", event);
    }
}

fn request_phase(body: &[u8]) -> &'static str {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return "initial";
    };
    let messages = value
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if messages
        .iter()
        .any(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("assistant"))
    {
        "continuation"
    } else {
        "initial"
    }
}

fn write_pipeline_error(
    stream: &mut TcpStream,
    error: &PipelineError,
    log: Option<&SharedBridgeLog>,
) -> io::Result<()> {
    log_pipeline_error(error, log);
    warn_once_on_terminal_failure(error);
    let status = error.http_status();
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type_for(error),
            "message": error.client_message(),
        },
    });
    write_response_with(
        stream,
        status,
        "application/json",
        body.to_string().as_bytes(),
        false,
        &retry_after_header(error),
    )
}

/// The Anthropic error type, derived from the *classification* where one
/// exists and only falling back to the status otherwise.
///
/// Re-deriving it from the number alone is what kept `billing_error` from ever
/// reaching the client: a drained account and an ordinary throttle are both
/// 429, and the status has already lost the distinction the pipeline computed.
fn error_type_for(error: &PipelineError) -> &'static str {
    match error.failure_class() {
        Some(FailureClass::Exhausted) => "billing_error",
        _ => anthropic_error_type(error.http_status()),
    }
}

/// `Retry-After`, echoed to the client on a throttle or an exhaustion.
///
/// The bridge previously emitted exactly four hardcoded headers and never this
/// one, on any status -- so a client showing a reset time was reporting its own
/// separate accounting, about a different limit than the one that broke the
/// turn.
fn retry_after_header(error: &PipelineError) -> Vec<(String, String)> {
    match error.retry_after_seconds() {
        Some(seconds) => vec![("Retry-After".to_string(), seconds.to_string())],
        None => Vec::new(),
    }
}

fn lock_log(log: &SharedBridgeLog) -> std::sync::MutexGuard<'_, BridgeLog> {
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_cache_health(health: &SharedCacheHealth) -> std::sync::MutexGuard<'_, CacheHealthTracker> {
    health
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Feed the request's `output_config.effort` to the status line (#1464).
/// The gateway is the only layer that sees subagents' actual effort.
fn record_request_effort(status_usage: &SharedStatusUsage, json: &serde_json::Value, agent: bool) {
    let Some(effort) = json
        .pointer("/output_config/effort")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let writer = status_usage
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .writer
        .clone();
    if let Some(writer) = writer {
        writer.record_effort(effort, agent);
    }
}

/// `/clear` starts a new conversation, so the status line drops the previous
/// one's totals instead of carrying them for the rest of the launch (#2045).
fn reset_status_usage(status_usage: &SharedStatusUsage) {
    let reset = {
        let mut status = status_usage.lock().unwrap_or_else(|e| e.into_inner());
        status.totals = UsageTotals::default();
        status.generation = status.generation.saturating_add(1);
        let generation = status.generation;
        status.writer.clone().map(|writer| (writer, generation))
    };
    if let Some((writer, generation)) = reset {
        writer.reset_usage(generation);
    }
}

/// Publish exact terminal usage for an Anthropic-compatible upstream without
/// feeding a provider-specific cache-health state machine. The wire bytes have
/// already been forwarded unchanged; this is an independent display ledger
/// for the current conversation.
fn publish_status_usage(
    status_usage: &SharedStatusUsage,
    provider: AnthropicUsageProvider,
    model: &str,
    usage: TokenUsage,
    cache_health: &str,
) {
    let provider = match provider {
        AnthropicUsageProvider::Claude => "claude",
        AnthropicUsageProvider::DeepSeek => "deepseek",
        AnthropicUsageProvider::Kimi => "kimi",
        AnthropicUsageProvider::OpenRouter => "openrouter",
    };
    let published = {
        let mut status = status_usage.lock().unwrap_or_else(|e| e.into_inner());
        status.totals.request_count = status.totals.request_count.saturating_add(1);
        status.totals.input_tokens = status
            .totals
            .input_tokens
            .saturating_add(usage.input_tokens);
        status.totals.cached_input_tokens = status
            .totals
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        status.totals.output_tokens = status
            .totals
            .output_tokens
            .saturating_add(usage.output_tokens);
        let totals = status.totals;
        status.writer.clone().map(|writer| {
            (
                writer,
                crate::toast::statusline::StatusUsage {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    request_count: totals.request_count,
                    cached_input_tokens: totals.cached_input_tokens,
                    uncached_input_tokens: totals
                        .input_tokens
                        .saturating_sub(totals.cached_input_tokens),
                    output_tokens: totals.output_tokens,
                    cache_health: cache_health.to_string(),
                    generation: status.generation,
                },
            )
        })
    };
    if let Some((writer, status)) = published {
        writer.publish_usage(status);
    }
}

fn record_cache_usage(
    health: &SharedCacheHealth,
    status_usage: &SharedStatusUsage,
    conversation_key: &ConversationKey,
    model: &str,
    usage: Option<TokenUsage>,
    log: Option<&SharedBridgeLog>,
) {
    let Some(usage) = usage else {
        return;
    };
    let mut health = lock_cache_health(health);
    let before = health.health(&conversation_key.id);
    let after = health.record(&conversation_key.id, usage);
    let totals = health.totals(&conversation_key.id);
    drop(health);
    let published = {
        let mut status = status_usage.lock().unwrap_or_else(|e| e.into_inner());
        status.totals.request_count = status.totals.request_count.saturating_add(1);
        status.totals.input_tokens = status
            .totals
            .input_tokens
            .saturating_add(usage.input_tokens);
        status.totals.cached_input_tokens = status
            .totals
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        status.totals.output_tokens = status
            .totals
            .output_tokens
            .saturating_add(usage.output_tokens);
        let totals = status.totals;
        status.writer.clone().map(|writer| {
            (
                writer,
                crate::toast::statusline::StatusUsage {
                    provider: "codex".to_string(),
                    model: model.to_string(),
                    request_count: totals.request_count,
                    cached_input_tokens: totals.cached_input_tokens,
                    uncached_input_tokens: totals
                        .input_tokens
                        .saturating_sub(totals.cached_input_tokens),
                    output_tokens: totals.output_tokens,
                    cache_health: cache_health_name(after).to_string(),
                    generation: status.generation,
                },
            )
        })
    };
    if let Some((writer, status)) = published {
        writer.publish_usage(status);
    }
    if after == before {
        return;
    }
    if let Some(log) = log {
        lock_log(log).record(serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "cache_health",
            "conversation_scope": conversation_key.scope(),
            "state": cache_health_name(after),
            "input_tokens": usage.input_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "uncached_input_tokens": usage.uncached_input_tokens(),
            "output_tokens": usage.output_tokens,
            "request_count": totals.request_count,
            "total_input_tokens": totals.input_tokens,
            "total_cached_input_tokens": totals.cached_input_tokens,
            "total_output_tokens": totals.output_tokens,
        }));
    }
    match after {
        CacheHealth::Degraded => eprintln!(
            "\x07[clud] codex bridge: repeated large requests have little cache credit; the next misses may be stopped before more plan usage is consumed"
        ),
        CacheHealth::FuseTripped => eprintln!(
            "\x07[clud] codex bridge: cache-health fuse armed; the next Codex request will be stopped before another large uncached replay"
        ),
        CacheHealth::Cold | CacheHealth::Healthy => {}
    }
}

fn record_cache_fuse_refusal(log: Option<&SharedBridgeLog>, conversation_key: &ConversationKey) {
    if let Some(log) = log {
        lock_log(log).record(serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "cache_health_fuse_refused",
            "conversation_scope": conversation_key.scope(),
        }));
    }
}

/// Not retryable by the client (see [`write_cache_fuse_refusal`]).
const CACHE_FUSE_REFUSAL_STATUS: u16 = 400;
const CACHE_FUSE_REFUSAL_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"clud cache-health fuse: this conversation sent repeated large uncached requests and was stopped before another one. This is not retryable and is not a provider rate limit. The likely cause is a wrong or oversized diff range (for example a stale @{upstream} after a rebase): check the range, then run /clear or restart to resume, or switch to --harness codex"}}"#;

fn write_cache_fuse_refusal(
    stream: &mut TcpStream,
    log: Option<&SharedBridgeLog>,
    conversation_key: &ConversationKey,
) {
    record_cache_fuse_refusal(log, conversation_key);
    // A 400 `invalid_request_error`, not a 429 `overloaded_error`: Claude Code
    // retries a 429 on its own, which is the loop this refusal exists to stop
    // (#1301). The likely cause is a wrong diff range, so the message says so.
    let _ = write_response(
        stream,
        CACHE_FUSE_REFUSAL_STATUS,
        "application/json",
        CACHE_FUSE_REFUSAL_BODY.as_bytes(),
        false,
    );
}

fn cache_health_name(health: CacheHealth) -> &'static str {
    match health {
        CacheHealth::Cold => "cold",
        CacheHealth::Healthy => "healthy",
        CacheHealth::Degraded => "degraded",
        CacheHealth::FuseTripped => "fuse_tripped",
    }
}

fn record_rejection(log: Option<&SharedBridgeLog>, status: u16, reason: &'static str) {
    if let Some(log) = log {
        lock_log(log).record(serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "request_rejected",
            "downstream_status": status,
            "reason": reason,
        }));
    }
}

/// Record one contiguous interval in which new sockets remain in the kernel
/// listener backlog. No request-derived field enters this event.
fn record_admission_queued(log: Option<&SharedBridgeLog>) {
    if let Some(log) = log {
        lock_log(log).record_ambient(serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "admission_queued",
        }));
    }
}

/// The matching admission event is deliberately distinct from
/// `upstream_attempt`: queue time is local scheduling, not a retried provider
/// request. `wait_ms` is an aggregate backlog interval and contains no client
/// identity, header, token, or request content.
fn record_admission_acquired(log: Option<&SharedBridgeLog>, wait: Duration) {
    if let Some(log) = log {
        lock_log(log).record_ambient(serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "admission_acquired",
            "wait_ms": wait.as_millis() as u64,
        }));
    }
}

/// Longest model ID persisted. The field comes from a request body capped only
/// at `DEFAULT_MAX_BODY_BYTES`, and a single ~1 MiB value would exhaust the
/// log's budget and silence every failure after it -- the incident the log
/// exists for. No real ID is close to this.
const MAX_LOGGED_MODEL_CHARS: usize = 128;

/// A rejection whose cause is the model the caller asked for, recorded with
/// that ID (#999). `reason` alone cannot say *which* selection wedged the
/// session, and the model ID is the one request-body field worth persisting.
fn record_model_rejection(
    log: Option<&SharedBridgeLog>,
    status: u16,
    reason: &'static str,
    model: &str,
) {
    if let Some(log) = log {
        let model = model
            .chars()
            .take(MAX_LOGGED_MODEL_CHARS)
            .collect::<String>();
        lock_log(log).record(serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "request_rejected",
            "downstream_status": status,
            "reason": reason,
            "model": model,
        }));
    }
}

/// Why `model` may not be served on a pinned launch (#1257), or `None` when
/// it may. An empty allowlist means unconstrained -- byte-for-byte the
/// pre-#1257 behavior.
///
/// Two exemptions, both because the id that reaches an upstream is never
/// billed outside the pin (DD-077):
/// - a `haiku`-named id is the harness's side-model machinery -- the same
///   substring `is_anthropic_main_model_pick` keys on;
/// - a `claude*` id is served *as the launch selection* on the Codex route
///   (DD-038's substitution) and proxied on the caller's own Claude
///   credential on the unified route. Neither touches the pinned provider's
///   key, and refusing would break the harness's own side-model calls, which
///   share the prefix (#1007).
fn allowlist_refusal(allowed: &[String], model: &str) -> Option<String> {
    if allowed.is_empty() {
        return None;
    }
    let lower = model.trim().to_ascii_lowercase();
    if lower.contains("haiku") || lower.starts_with("claude") {
        return None;
    }
    if provider_catalog::model_allowlist_allows(allowed, model) {
        return None;
    }
    Some(provider_catalog::model_allowlist_refusal(allowed, model))
}

/// Write the 400 every pinned route answers with (#1257): an
/// `invalid_request_error` whose message names the allowlist, recorded like
/// every other model rejection so a wedged session still leaves evidence.
/// Returns `true` when a response was written and the caller must return.
fn refuse_if_outside_allowlist(
    stream: &mut TcpStream,
    log: Option<&SharedBridgeLog>,
    allowed: &[String],
    model: &str,
) -> bool {
    let Some(message) = allowlist_refusal(allowed, model) else {
        return false;
    };
    record_model_rejection(log, 400, "model_not_allowed", model);
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": message,
        }
    })
    .to_string();
    let _ = write_response(stream, 400, "application/json", body.as_bytes(), false);
    true
}

/// The model IDs a `GET /v1/models` advertised (#999).
///
/// Not a failure, so it widens the log's contract deliberately: when a model
/// selection wedges a session this is often the only surviving evidence of
/// what the bridge offered, and a refusal is only readable against it. Recorded
/// as ambient context so it does not, on its own, claim an operator's attention
/// through the shutdown hint.
fn record_catalog_advertised(log: Option<&SharedBridgeLog>, model_ids: &[&str]) {
    if let Some(log) = log {
        lock_log(log).record_ambient(serde_json::json!({
            "ts_ms": unix_ms(),
            "event": "catalog_advertised",
            "model_ids": model_ids,
        }));
    }
}

fn record_retry(
    log: &SharedBridgeLog,
    error: &UpstreamError,
    attempt: u32,
    budget: u32,
    backoff: Option<Duration>,
) {
    let mut event = serde_json::json!({
        "ts_ms": unix_ms(),
        "event": "upstream_attempt",
        "kind": upstream_error_kind(error),
        "attempt": attempt,
        "budget": budget,
        "decision": if backoff.is_some() { "retry" } else { "stop" },
        "backoff_ms": backoff.map(|value| value.as_millis() as u64),
    });
    if let UpstreamError::Status(failure) = error {
        add_failure_fields(&mut event, failure);
    }
    lock_log(log).record(event);
}

fn add_failure_fields(event: &mut serde_json::Value, failure: &UpstreamFailure) {
    let Some(fields) = event.as_object_mut() else {
        return;
    };
    fields.insert("upstream_status".into(), failure.status().into());
    fields.insert(
        "class".into(),
        format!("{:?}", failure.class()).to_ascii_lowercase().into(),
    );
    fields.insert("cf_ray".into(), failure.cf_ray().into());
    fields.insert("x_request_id".into(), failure.request_id().into());
    fields.insert(
        "retry_after_ms".into(),
        failure
            .retry_after()
            .map(|value| value.as_millis() as u64)
            .into(),
    );
    fields.insert(
        "resets_in_ms".into(),
        failure
            .resets_in()
            .map(|value| value.as_millis() as u64)
            .into(),
    );
    fields.insert("detail".into(), failure.detail().into());
}

fn upstream_error_kind(error: &UpstreamError) -> &'static str {
    match error {
        UpstreamError::Credentials(_) => "credentials",
        UpstreamError::Transport(_) => "transport",
        UpstreamError::CompactMalformed => "compact_malformed",
        UpstreamError::Status(_) => "status",
        UpstreamError::Timeout => "timeout",
        UpstreamError::TooLarge => "too_large",
        UpstreamError::Cancelled => "cancelled",
        UpstreamError::Downstream(_) => "downstream",
    }
}

fn pipeline_error_kind(error: &PipelineError) -> &'static str {
    match error {
        PipelineError::Translate(_) => "translate",
        PipelineError::Compaction(_) => "compaction",
        PipelineError::ContinuationInvariant(_) => "continuation_invariant",
        PipelineError::Provider(_) => "provider_in_band",
        PipelineError::Upstream(error) => upstream_error_kind(error),
    }
}

fn default_bridge_log_path() -> Option<std::path::PathBuf> {
    let state_dir = crate::daemon::default_state_dir().ok()?;
    Some(crate::bridge_log::session_bridge_log_path(
        &state_dir,
        std::process::id(),
        crate::process_identity::self_start_time(),
    ))
}

fn anthropic_error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        422 => "invalid_request_error",
        429 => "rate_limit_error",
        // #764 stopped folding these into 502; they need types of their own.
        499 => "request_cancelled",
        504 => "timeout_error",
        _ => "api_error",
    }
}

/// Per-response identifier. Anthropic clients treat this as opaque; it only has
/// to be unique enough to correlate one turn.
fn new_message_id() -> String {
    let mut bytes = [0_u8; 12];
    if getrandom::fill(&mut bytes).is_err() {
        return "msg_clud_bridge".to_string();
    }
    format!(
        "msg_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

struct ParsedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    authorization: Option<String>,
    session_id: Option<String>,
    agent_id: Option<String>,
    content_length: usize,
    body_prefix: Vec<u8>,
}

impl ParsedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn read_headers(
    stream: &mut TcpStream,
    limit: usize,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> Result<ParsedRequest, u16> {
    let mut buffer = Vec::with_capacity(limit.min(4096));
    let header_end = loop {
        if let Some(index) = find_header_end(&buffer) {
            if index + 4 > limit {
                return Err(431);
            }
            break index + 4;
        }
        if buffer.len() >= limit {
            return Err(431);
        }
        set_remaining_read_timeout(stream, deadline, shutdown)?;
        let mut chunk = [0_u8; 1024];
        match stream.read(&mut chunk) {
            Ok(0) => return Err(400),
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                match read_expired(deadline, shutdown) {
                    Some(status) => return Err(status),
                    None => continue,
                }
            }
            Err(_) => return Err(400),
        }
    };

    let header_text = std::str::from_utf8(&buffer[..header_end - 4]).map_err(|_| 400_u16)?;
    let mut lines = header_text.split("\r\n");
    let mut request_line = lines.next().ok_or(400_u16)?.split_whitespace();
    let method = request_line.next().ok_or(400_u16)?.to_string();
    let path = request_line.next().ok_or(400_u16)?.to_string();
    if request_line.next().ok_or(400_u16)? != "HTTP/1.1" || request_line.next().is_some() {
        return Err(400);
    }
    let mut headers = Vec::new();
    let mut authorization = None;
    let mut session_id = None;
    let mut agent_id = None;
    let mut content_length = 0_usize;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(400_u16)?;
        let value = value.trim();
        if value.contains(['\r', '\n']) {
            return Err(400);
        }
        headers.push((name.to_string(), value.to_string()));
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-claude-code-session-id") {
            session_id = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-claude-code-agent-id") {
            agent_id = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-claude-code-parent-agent-id") {
            // The parent is provenance, not the child's history identity.
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().map_err(|_| 400_u16)?;
        }
    }
    Ok(ParsedRequest {
        method,
        path,
        headers,
        authorization,
        session_id,
        agent_id,
        content_length,
        body_prefix: buffer[header_end..].to_vec(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_body(
    stream: &mut TcpStream,
    mut body: Vec<u8>,
    content_length: usize,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> Result<Vec<u8>, u16> {
    body.truncate(content_length);
    while body.len() < content_length {
        set_remaining_read_timeout(stream, deadline, shutdown)?;
        let remaining = content_length - body.len();
        let mut chunk = [0_u8; 4096];
        match stream.read(&mut chunk[..remaining.min(4096)]) {
            Ok(0) => return Err(400),
            Ok(count) => body.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                match read_expired(deadline, shutdown) {
                    Some(status) => return Err(status),
                    None => continue,
                }
            }
            Err(_) => return Err(400),
        }
    }
    Ok(body)
}

/// Arm the next read. The timeout is the smaller of the remaining phase budget
/// and [`READ_POLL`], so a caller that sees a timeout must consult the deadline
/// itself before deciding the peer is late — see [`read_expired`].
fn set_remaining_read_timeout(
    stream: &TcpStream,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> Result<(), u16> {
    if shutdown.load(Ordering::Acquire) {
        return Err(ABANDON);
    }
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(408_u16)?;
    stream
        .set_read_timeout(Some(remaining.min(READ_POLL)))
        .map_err(|_| 400)
}

/// Classify a read timeout: a real deadline breach is 408, a poll-interval
/// expiry is just "nothing yet, look again".
fn read_expired(deadline: Instant, shutdown: &AtomicBool) -> Option<u16> {
    if shutdown.load(Ordering::Acquire) {
        return Some(ABANDON);
    }
    (Instant::now() >= deadline).then_some(408)
}

fn write_error(stream: &mut TcpStream, status: u16) -> io::Result<()> {
    let (error_type, message) = match status {
        400 => ("invalid_request_error", "invalid request"),
        401 => ("authentication_error", "unauthorized"),
        404 => ("not_found_error", "not found"),
        408 => ("timeout_error", "request timeout"),
        413 => ("invalid_request_error", "request body too large"),
        431 => ("invalid_request_error", "request headers too large"),
        // No 502 arm: an upstream failure is written by `write_pipeline_error`,
        // which carries the classified message. This path only serves statuses
        // the connection layer itself chooses.
        _ => ("api_error", "bridge error"),
    };
    let body = format!(r#"{{"error":{{"type":"{error_type}","message":"{message}"}}}}"#);
    write_response(stream, status, "application/json", body.as_bytes(), false)
}

/// Incremental SSE writer.
///
/// `write_response` cannot serve this path: it derives `Content-Length` from a
/// fully materialised body and writes once, so a caller could only ever send a
/// stream that had already finished. Chunked transfer plus a flush per frame is
/// what lets Claude render tokens as they arrive rather than at end-of-turn.
///
/// Headers are deferred until the first frame. That is deliberate: a failure
/// before any output can still choose a status code, and only once a frame is
/// on the wire does the response become committed.
///
/// The write timeout is re-armed per frame, making it an idle timeout: total
/// response duration is unbounded, but a peer that stops reading is still cut
/// off.
struct EventStreamWriter<'a> {
    stream: &'a mut TcpStream,
    idle_timeout: Duration,
    started: bool,
    #[cfg(test)]
    frame_hold: Duration,
}

impl<'a> EventStreamWriter<'a> {
    fn new(stream: &'a mut TcpStream, config: &BridgeConfig) -> Self {
        let idle_timeout = if config.stream_idle_timeout.is_zero() {
            DEFAULT_STREAM_IDLE_TIMEOUT
        } else {
            config.stream_idle_timeout
        };
        Self {
            stream,
            idle_timeout,
            started: false,
            #[cfg(test)]
            frame_hold: config.frame_hold,
        }
    }

    fn started(&self) -> bool {
        self.started
    }

    fn write_frame(&mut self, frame: &str) -> io::Result<()> {
        // Re-arm per frame so the budget is "silence between frames", not
        // "total time to produce the response".
        self.stream.set_write_timeout(Some(self.idle_timeout))?;
        if !self.started {
            self.stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            )?;
            self.stream.flush()?;
            self.started = true;
        }
        // Chunked framing is CRLF-delimited by specification; an LF-only
        // variant is not merely untidy, clients fail to parse it.
        write!(self.stream, "{:x}\r\n", frame.len())?;
        self.stream.write_all(frame.as_bytes())?;
        self.stream.write_all(b"\r\n")?;
        self.stream.flush()?;
        #[cfg(test)]
        if !self.frame_hold.is_zero() {
            thread::sleep(self.frame_hold);
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if !self.started {
            return Ok(());
        }
        self.stream.write_all(b"0\r\n\r\n")?;
        self.stream.flush()?;
        let _ = self.stream.shutdown(Shutdown::Both);
        Ok(())
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> io::Result<()> {
    write_response_with(stream, status, content_type, body, head_only, &[])
}

fn write_response_with(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
    extra_headers: &[(String, String)],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        499 => "Client Closed Request",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        body.len()
    )?;
    // Header names here are ours, never echoed from upstream, so no folding or
    // injection check is needed beyond that invariant.
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn test_upstream_override_from_process() -> Option<String> {
    let integration_enabled =
        std::env::var_os("CLUD_INTEGRATION_TESTS").is_some_and(|value| value == "1");
    let value = std::env::var("CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL").ok();
    resolve_test_upstream_override(cfg!(debug_assertions), integration_enabled, value)
}

/// The three non-Codex provider-shaped fakes used by the foreground matrix.
/// Codex continues to use `CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL` because its
/// wire protocol is Responses-shaped and is already independently seamed.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UnifiedIntegrationUpstreams {
    codex_base_url: String,
    anthropic_base_url: String,
    deepseek_base_url: String,
    openrouter_base_url: String,
    /// Optional, unlike the rest: suites written before Kimi joined the
    /// gateway set only four URLs, and leaving this out just drops Kimi's
    /// route for that launch.
    kimi_base_url: Option<String>,
}

impl UnifiedIntegrationUpstreams {
    pub(crate) fn has_kimi(&self) -> bool {
        self.kimi_base_url.is_some()
    }
}

impl std::fmt::Debug for UnifiedIntegrationUpstreams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnifiedIntegrationUpstreams")
            .field("codex_base_url", &"[redacted]")
            .field("anthropic_base_url", &"[redacted]")
            .field("deepseek_base_url", &"[redacted]")
            .field("openrouter_base_url", &"[redacted]")
            .field(
                "kimi_base_url",
                &self.kimi_base_url.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

/// Read an all-or-nothing test-only unified upstream override.  Requiring all
/// three URLs means a stray environment variable cannot partially alter a
/// launch, and the existing debug plus explicit-integration gate makes this
/// unavailable from release builds.
pub(crate) fn unified_integration_upstreams_from_process() -> Option<UnifiedIntegrationUpstreams> {
    let enabled = cfg!(debug_assertions)
        && std::env::var_os("CLUD_INTEGRATION_TESTS").is_some_and(|value| value == "1");
    resolve_unified_integration_upstreams(
        enabled,
        std::env::var("CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL").ok(),
        std::env::var("CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL").ok(),
        std::env::var("CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL").ok(),
        std::env::var("CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL").ok(),
    )
    .map(|mut upstreams| {
        upstreams.kimi_base_url = std::env::var("CLUD_TEST_UNIFIED_KIMI_UPSTREAM_URL")
            .ok()
            .filter(|url| !url.trim().is_empty());
        upstreams
    })
}

fn resolve_unified_integration_upstreams(
    enabled: bool,
    codex_base_url: Option<String>,
    anthropic_base_url: Option<String>,
    deepseek_base_url: Option<String>,
    openrouter_base_url: Option<String>,
) -> Option<UnifiedIntegrationUpstreams> {
    if !enabled {
        return None;
    }
    let usable = |url: Option<String>| url.filter(|url| !url.trim().is_empty());
    Some(UnifiedIntegrationUpstreams {
        codex_base_url: usable(codex_base_url)?,
        anthropic_base_url: usable(anthropic_base_url)?,
        deepseek_base_url: usable(deepseek_base_url)?,
        openrouter_base_url: usable(openrouter_base_url)?,
        kimi_base_url: None,
    })
}

fn resolve_test_upstream_override(
    debug_assertions: bool,
    integration_enabled: bool,
    value: Option<String>,
) -> Option<String> {
    if debug_assertions && integration_enabled {
        value.filter(|url| !url.trim().is_empty())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    mod redaction_http;

    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::thread;
    use std::time::Duration;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    #[test]
    fn unified_gateway_injects_the_complete_openrouter_privacy_policy() {
        let policy = crate::route_plan::ProviderRouting {
            only: Vec::new(),
            allow_fallbacks: true,
            zdr: true,
            data_collection: Some("deny".to_string()),
        };
        let gateway =
            UnifiedGatewayConfig::new(None, false).with_upstream_routing(Some(policy.clone()));
        let request = br#"{"messages":[],"provider":{"zdr":false},"top_k":5}"#;
        let bytes = gateway
            .routed_body(ModelProvider::OpenRouter, request)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["provider"], serde_json::to_value(policy).unwrap());
        assert_eq!(body["top_k"], 5);
        assert!(gateway
            .routed_body(ModelProvider::Claude, request)
            .is_none());
    }

    /// Read timeout for the test client.
    ///
    /// A backstop against a wedged bridge, not an assertion about latency, so
    /// it has to sit above the longest *legitimate* response. A transient
    /// upstream failure is retried with exponential backoff, and on a loaded CI
    /// runner that ladder overruns a tight budget -- which surfaces as EAGAIN
    /// from `read_to_string` and reads like a hang rather than the timing
    /// artefact it is. 10s sat under the ladder; this sits over it.
    const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(60);

    fn request(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect to bridge");
        stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("bridge response within the client read timeout");
        response
    }

    #[test]
    fn standalone_listener_requires_a_private_client_token() {
        assert!(BridgeConfig::default()
            .with_standalone_listener(0, "short".to_string())
            .is_err());
        assert!(BridgeConfig::default()
            .with_standalone_listener(0, "a".repeat(31) + "\n")
            .is_err());
        let token = "local-client-token-0123456789abcdef";
        let config = BridgeConfig::default()
            .with_unified_gateway(UnifiedGatewayConfig::new(None, false))
            .with_standalone_listener(0, token.to_string())
            .unwrap();
        assert!(!format!("{config:?}").contains(token));
        let bridge = BridgeHandle::start(config).unwrap();
        assert_eq!(bridge.bearer_token(), token);
        assert!(bridge.socket_addr().ip().is_loopback());
        assert!(!format!("{bridge:?}").contains(token));
        let response = request(
            bridge.socket_addr(),
            &authorized("GET", "/unknown", token, ""),
        );
        assert_eq!(status(&response), 404, "{response}");
    }

    #[test]
    fn standalone_client_auth_accepts_one_key_and_rejects_ambiguous_headers() {
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(UnifiedGatewayConfig::new(None, false))
                .with_standalone_client_auth(),
        )
        .expect("start bridge");
        let token = bridge.bearer_token();
        for (headers, expected) in [
            (format!("x-api-key: {token}\r\n"), "404"),
            (format!("Authorization: Bearer {token}\r\n"), "404"),
            (String::new(), "401"),
            ("x-api-key: wrong\r\n".to_string(), "401"),
            (format!("x-api-key: {token}\r\nx-api-key: wrong\r\n"), "401"),
            (
                format!("x-api-key: {token}\r\nAuthorization: Bearer {token}\r\n"),
                "401",
            ),
        ] {
            let wire = format!("GET /unknown HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n");
            let response = request(bridge.socket_addr(), &wire);
            assert!(
                response.starts_with(&format!("HTTP/1.1 {expected}")),
                "{response}"
            );
        }
    }

    struct FixedStandaloneClaudeCredential;

    impl StandaloneClaudeCredentialSource for FixedStandaloneClaudeCredential {
        fn credential(&self) -> Option<StandaloneClaudeCredential> {
            Some(StandaloneClaudeCredential::ApiKey(
                "upstream-claude-canary".to_string(),
            ))
        }
    }

    #[test]
    fn standalone_codex_catalog_requires_no_claude_credential() {
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(UnifiedGatewayConfig::new(None, true))
                .with_standalone_client_auth(),
        )
        .unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models", bridge.bearer_token(), ""),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("clud-claude-codex-sol"));
        assert!(!response.contains("clud-claude-openrouter"));
    }

    #[test]
    fn anonymous_standalone_requests_do_not_exhaust_conversation_capacity() {
        let upstream = FakeResponses::start();
        let limits = HistoryLimits {
            max_conversations: 1,
            ..HistoryLimits::default()
        };
        let bridge = BridgeHandle::start(
            bridged_config(&upstream)
                .with_standalone_client_auth()
                .with_history_limits(limits),
        )
        .unwrap();
        for _ in 0..40 {
            let response = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
            );
            assert_eq!(status(&response), 200, "{response}");
        }
        assert_eq!(upstream.requests().len(), 40);
    }

    #[test]
    fn standalone_shutdown_interrupts_codex_tls_handshake() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut config = BridgeConfig::default()
            .with_test_upstream_url(Some(format!("https://{}", listener.local_addr().unwrap())))
            .with_standalone_client_auth();
        config.stream_idle_timeout = Duration::from_secs(5);
        let mut bridge = BridgeHandle::start(config).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_string();
        let client = thread::spawn(move || {
            request(
                addr,
                &authorized("POST", "/v1/messages", &token, PROBE_BODY),
            )
        });
        let (mut upstream, _) = listener.accept().unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut byte = [0];
        upstream.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], 0x16, "TLS handshake reached the tracked socket");
        let start = Instant::now();
        bridge.shutdown().unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        client.join().unwrap();
    }

    #[test]
    fn standalone_unauthorized_post_returns_complete_error_for_a_split_body() {
        let upstream = FakeResponses::start();
        let bridge =
            BridgeHandle::start(bridged_config(&upstream).with_standalone_client_auth()).unwrap();
        let mut client = TcpStream::connect(bridge.socket_addr()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.set_nodelay(true).unwrap();
        let body = PROBE_BODY;
        write!(client, "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nx-api-key: wrong\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        client.write_all(body.as_bytes()).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert_eq!(status(&response), 401, "{response}");
        let json: serde_json::Value =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(json["error"]["type"], "authentication_error");
        assert!(upstream.requests().is_empty());
    }

    #[test]
    fn standalone_catalog_omits_openrouter_without_its_credential() {
        let upstream = FakeResponses::start_with_response(Some(raw_response(
            200,
            "OK",
            r#"{"data":[{"id":"claude-test","type":"model"}],"has_more":false}"#,
        )));
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(FixedStandaloneClaudeCredential)),
        )
        .unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models", bridge.bearer_token(), ""),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("claude-test"));
        assert!(!response.contains("clud-claude-openrouter"));
        let body = r#"{"model":"clud-claude-openrouter-sonnet","messages":[{"role":"user","content":"hello"}]}"#;
        let refused = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), body),
        );
        assert_eq!(status(&refused), 400, "{refused}");
        assert_eq!(upstream.requests().len(), 1, "no fallback to Claude");
    }

    #[test]
    fn standalone_catalog_combines_claude_and_configured_openrouter_models() {
        let upstream = FakeResponses::start_with_response(Some(raw_response(
            200,
            "OK",
            r#"{"data":[{"id":"claude-test","type":"model"}],"has_more":false}"#,
        )));
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_openrouter(Some("openrouter-canary".to_string()))
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(FixedStandaloneClaudeCredential)),
        )
        .unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models", bridge.bearer_token(), ""),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("claude-test"));
        assert!(response.contains("clud-claude-openrouter-sonnet"));
        assert!(!response.contains("upstream-claude-canary"));
        let sent = upstream.requests();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("upstream-claude-canary"));
        assert!(!sent[0].contains(bridge.bearer_token()));
        assert!(!sent[0].contains("openrouter-canary"));
    }

    #[test]
    fn standalone_catalog_fetches_all_native_pages() {
        let upstream = FakeResponses::start_with_responses(vec![
            Some(raw_response(
                200,
                "OK",
                r#"{"data":[{"id":"first"}],"has_more":true,"last_id":"first"}"#,
            )),
            Some(raw_response(
                200,
                "OK",
                r#"{"data":[{"id":"second"}],"has_more":false}"#,
            )),
        ]);
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(FixedStandaloneClaudeCredential)),
        )
        .unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models", bridge.bearer_token(), ""),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("first") && response.contains("second"));
        let sent = upstream.requests();
        assert_eq!(sent.len(), 2);
        assert!(sent[1].contains("after_id=first"));
        assert!(sent
            .iter()
            .all(|wire| !wire.contains(bridge.bearer_token())));
    }

    #[test]
    fn standalone_catalog_rejects_a_repeated_upstream_cursor() {
        let page = raw_response(
            200,
            "OK",
            r#"{"data":[{"id":"first"}],"has_more":true,"last_id":"first"}"#,
        );
        let upstream = FakeResponses::start_with_response(Some(page));
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(FixedStandaloneClaudeCredential)),
        )
        .unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models", bridge.bearer_token(), ""),
        );
        assert_eq!(status(&response), 502, "{response}");
        assert!(!response.contains("upstream-claude-canary"));
        assert_eq!(upstream.requests().len(), 2);
    }

    #[test]
    fn standalone_client_token_is_never_forwarded_to_claude() {
        let upstream = FakeResponses::start_with_response(Some(raw_response(
            200,
            "OK",
            r#"{"id":"msg_test","type":"message","role":"assistant","model":"claude-test","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#,
        )));
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(FixedStandaloneClaudeCredential)),
        )
        .unwrap();
        let body = r#"{"model":"claude-test","messages":[{"role":"user","content":"hello"}]}"#;
        let token = bridge.bearer_token();
        let api_key_request = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nx-api-key: {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        for wire in [
            authorized("POST", "/v1/messages", token, body),
            api_key_request,
        ] {
            let response = request(bridge.socket_addr(), &wire);
            assert_eq!(status(&response), 200, "{response}");
        }
        let requests = upstream.requests();
        assert_eq!(requests.len(), 2);
        for sent in requests {
            assert!(!sent.contains(token));
            assert!(!sent.to_ascii_lowercase().contains("authorization:"));
            assert!(sent.contains("upstream-claude-canary"));
        }
    }

    struct RotatingStandaloneClaudeCredential(AtomicUsize);

    impl StandaloneClaudeCredentialSource for RotatingStandaloneClaudeCredential {
        fn credential(&self) -> Option<StandaloneClaudeCredential> {
            let next = self.0.fetch_add(1, Ordering::Relaxed);
            Some(StandaloneClaudeCredential::Bearer(format!(
                "upstream-token-{next}"
            )))
        }
    }

    #[test]
    fn standalone_resolves_claude_credentials_for_every_request() {
        let upstream = FakeResponses::start_with_response(Some(raw_response(
            200,
            "OK",
            r#"{"id":"msg_test","type":"message","role":"assistant","model":"claude-test","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#,
        )));
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(RotatingStandaloneClaudeCredential(
                    AtomicUsize::new(0),
                ))),
        )
        .unwrap();
        let body = r#"{"model":"claude-test","messages":[{"role":"user","content":"hello"}]}"#;
        for _ in 0..2 {
            let response = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", bridge.bearer_token(), body),
            );
            assert_eq!(status(&response), 200, "{response}");
        }
        let requests = upstream.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("Bearer upstream-token-0"));
        assert!(requests[1].contains("Bearer upstream-token-1"));
        assert!(requests
            .iter()
            .all(|sent| !sent.contains(bridge.bearer_token())));
    }

    struct MalformedStandaloneClaudeCredential;

    impl StandaloneClaudeCredentialSource for MalformedStandaloneClaudeCredential {
        fn credential(&self) -> Option<StandaloneClaudeCredential> {
            Some(StandaloneClaudeCredential::Bearer(
                "secret\r\nx-extra: injected".to_string(),
            ))
        }
    }

    #[test]
    fn standalone_rejects_malformed_upstream_credentials_without_a_request() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(MalformedStandaloneClaudeCredential)),
        )
        .unwrap();
        let body = r#"{"model":"claude-test","messages":[{"role":"user","content":"hello"}]}"#;
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), body),
        );
        assert_eq!(status(&response), 503, "{response}");
        assert!(!response.contains("injected"));
        assert!(upstream.requests().is_empty());
    }

    #[test]
    fn standalone_never_follows_upstream_redirects_with_credentials() {
        let destination = FakeResponses::start();
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            destination.base_url
        );
        let upstream = FakeResponses::start_with_response(Some(redirect.into_bytes()));
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(FixedStandaloneClaudeCredential)),
        )
        .unwrap();
        let body = r#"{"model":"claude-test","messages":[{"role":"user","content":"hello"}]}"#;
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), body),
        );
        assert_eq!(status(&response), 302, "{response}");
        assert!(!response.contains("upstream-claude-canary"));
        assert!(destination.requests().is_empty());
    }

    #[test]
    fn standalone_sanitizes_upstream_error_bodies() {
        let upstream = FakeResponses::start_with_response(Some(raw_response(
            401,
            "Unauthorized",
            r#"{"error":{"message":"upstream-claude-canary"}}"#,
        )));
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth()
                .with_standalone_claude_credential(Arc::new(FixedStandaloneClaudeCredential)),
        )
        .unwrap();
        let body = r#"{"model":"claude-test","messages":[{"role":"user","content":"hello"}]}"#;
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), body),
        );
        assert_eq!(status(&response), 401, "{response}");
        assert!(!response.contains("upstream-claude-canary"));
        assert!(!response.contains(bridge.bearer_token()));
    }

    #[test]
    fn standalone_refuses_claude_requests_without_upstream_credentials() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_unified_gateway(
                    UnifiedGatewayConfig::new(None, false)
                        .with_upstreams(upstream.base_url.clone(), upstream.base_url.clone()),
                )
                .with_standalone_client_auth(),
        )
        .unwrap();
        let body = r#"{"model":"claude-test","messages":[{"role":"user","content":"hello"}]}"#;
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), body),
        );
        assert_eq!(status(&response), 503, "{response}");
        assert!(upstream.requests().is_empty());
    }

    fn authorized(method: &str, path: &str, token: &str, body: &str) -> String {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn authorized_with_headers(
        method: &str,
        path: &str,
        token: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> String {
        let extra = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// A Responses-shaped fake. Phase 2's seam passed an Anthropic body
    /// straight through, so the end-to-end tests proved transport and auth but
    /// nothing about translation; this one speaks the upstream protocol and
    /// records what it was actually sent.
    struct FakeResponses {
        base_url: String,
        requests: std::sync::Arc<Mutex<Vec<String>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl FakeResponses {
        fn start() -> Self {
            Self::start_with_response(None)
        }

        fn start_with_response(scripted: Option<Vec<u8>>) -> Self {
            Self::start_with_scripted_responses(vec![scripted.clone()], scripted)
        }

        fn start_with_responses(scripted: Vec<Option<Vec<u8>>>) -> Self {
            Self::start_with_scripted_responses(scripted, None)
        }

        #[expect(
            clippy::too_many_lines,
            reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
        )]
        fn start_with_scripted_responses(
            scripted: Vec<Option<Vec<u8>>>,
            fallback_response: Option<Vec<u8>>,
        ) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
            let scripted = std::sync::Arc::new(Mutex::new(
                scripted
                    .into_iter()
                    .collect::<std::collections::VecDeque<Option<Vec<u8>>>>(),
            ));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread_requests = std::sync::Arc::clone(&requests);
            let thread_scripted = std::sync::Arc::clone(&scripted);
            let thread_fallback_response = fallback_response;
            let thread_shutdown = Arc::clone(&shutdown);
            let handle = thread::spawn(move || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    let (mut upstream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    upstream.set_nonblocking(false).unwrap();
                    upstream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut raw = Vec::new();
                    let mut byte = [0_u8; 1];
                    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                        match upstream.read(&mut byte) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => raw.push(byte[0]),
                        }
                    }
                    let head = String::from_utf8_lossy(&raw).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    let mut request_body = vec![0_u8; length];
                    if length > 0 {
                        let _ = upstream.read_exact(&mut request_body);
                    }
                    thread_requests
                        .lock()
                        .unwrap()
                        .push(format!("{head}{}", String::from_utf8_lossy(&request_body)));

                    let scripted_reply = thread_scripted
                        .lock()
                        .unwrap()
                        .pop_front()
                        .flatten()
                        .or_else(|| thread_fallback_response.clone());
                    if let Some(reply) = scripted_reply {
                        let _ = upstream.write_all(&reply);
                        let _ = upstream.flush();
                        let _ = upstream.shutdown(Shutdown::Both);
                        continue;
                    }
                    let events = concat!(
                        "event: response.created
data: {\"type\":\"response.created\"}

",
                        "event: response.output_text.delta
data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"bridged \"}

",
                        "event: response.output_text.delta
data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"reply\"}

",
                        "event: response.output_text.done
data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"content_index\":0}

",
                        "event: response.completed
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}

",
                    );
                    let reply = format!(
                        "HTTP/1.1 200 OK
Content-Type: text/event-stream
Content-Length: {}
Connection: close

{events}",
                        events.len()
                    );
                    let _ = upstream.write_all(reply.as_bytes());
                    let _ = upstream.flush();
                    let _ = upstream.shutdown(Shutdown::Both);
                }
            });
            Self {
                base_url: format!("http://{addr}"),
                requests,
                shutdown,
                handle: Some(handle),
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for FakeResponses {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn response_with_events(events: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
            events.len()
        )
        .into_bytes()
    }

    fn anthropic_usage_response(start_usage: &str, terminal_usage: &str) -> Vec<u8> {
        response_with_events(&format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{start_usage}}}}}\n\n\
             event: message_delta\ndata: {{\"type\":\"message_delta\",\"usage\":{terminal_usage}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        ))
    }

    fn large_uncached_response() -> Vec<u8> {
        response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"ok\"}\n\n\
             event: response.output_text.done\ndata: {\"type\":\"response.output_text.done\",\"output_index\":0,\"content_index\":0}\n\n\
             event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100000,\"output_tokens\":1,\"input_tokens_details\":{\"cached_tokens\":0}}}}\n\n",
        )
    }

    fn context_length_failure_response() -> Vec<u8> {
        response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"invalid_request\",\"code\":\"context_length_exceeded\"}}}\n\n",
        )
    }

    fn context_length_status_response() -> Vec<u8> {
        let body = r#"{"error":{"type":"invalid_request","code":"context_length_exceeded","message":"too large"}}"#;
        format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn not_found_response() -> Vec<u8> {
        let body = r#"{"error":{"type":"not_found_error","message":"not found"}}"#;
        format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn incomplete_response() -> Vec<u8> {
        response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"truncated\"}\n\n\
             event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        )
    }

    fn recovery_success_response() -> Vec<u8> {
        response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"recovered\"}\n\n\
             event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg_recovered\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"recovered\"}]}}\n\n\
             event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        )
    }

    fn function_call_response() -> Vec<u8> {
        response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_parent\",\"name\":\"Workflow\"}}\n\n\
             event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{}\"}\n\n\
             event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_parent\",\"name\":\"Workflow\",\"arguments\":\"{}\"}}\n\n\
             event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        )
    }

    fn visible_transport_failure_response() -> Vec<u8> {
        let events = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
                      event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"partial\"}\n\n";
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
            events.len() + 1024
        )
        .into_bytes()
    }

    fn failed_turn_replay_body(mut suffix: Vec<serde_json::Value>) -> String {
        let mut messages = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "call_parent",
                    "name": "Workflow",
                    "input": {},
                }],
            }),
            serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_parent",
                    "content": "result-parent",
                }],
            }),
        ];
        messages.append(&mut suffix);
        serde_json::json!({
            "model": "claude-x",
            "messages": messages,
            "stream": true,
        })
        .to_string()
    }

    fn parallel_function_call_response(count: usize) -> Vec<u8> {
        let mut events =
            String::from("event: response.created\ndata: {\"type\":\"response.created\"}\n\n");
        for index in 0..count {
            events.push_str(&format!(
                "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":{index},\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_{index}\",\"name\":\"Bash\"}}}}\n\n\
                 event: response.function_call_arguments.delta\ndata: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":{index},\"delta\":\"{{}}\"}}\n\n\
                 event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":{index},\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_{index}\",\"name\":\"Bash\",\"arguments\":\"{{}}\"}}}}\n\n"
            ));
        }
        events.push_str(
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":8}}}\n\n",
        );
        response_with_events(&events)
    }

    fn split_parallel_result_body(count: usize, include_every_result: bool) -> String {
        let calls = (0..count)
            .map(|index| {
                serde_json::json!({
                    "type": "tool_use",
                    "id": format!("call_{index}"),
                    "name": "Bash",
                    "input": {},
                })
            })
            .collect::<Vec<_>>();
        let mut messages = vec![
            serde_json::json!({"role": "user", "content": "inspect"}),
            serde_json::json!({"role": "assistant", "content": calls}),
        ];
        if include_every_result {
            messages.extend((0..count).map(|index| {
                serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": format!("call_{index}"),
                        "content": format!("result-{index}"),
                    }],
                })
            }));
        } else {
            messages.push(serde_json::json!({"role": "user", "content": "missing results"}));
        }
        serde_json::json!({
            "model": "claude-x",
            "messages": messages,
            "stream": false,
        })
        .to_string()
    }

    fn compact_success_response() -> Vec<u8> {
        let body = r#"{"output":[{"type":"compaction","id":"cmp_recovered","encrypted_content":"opaque-summary"}]}"#;
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// Config wired to a fake upstream, so POST /v1/messages exercises the real
    /// translate -> upstream -> translate pipeline.
    fn bridged_config(upstream: &FakeResponses) -> BridgeConfig {
        BridgeConfig::default().with_test_upstream_url(Some(upstream.base_url.clone()))
    }

    fn unified_config(
        anthropic: &FakeResponses,
        codex: &FakeResponses,
        deepseek: &FakeResponses,
    ) -> BridgeConfig {
        BridgeConfig::default()
            .with_test_upstream_url(Some(codex.base_url.clone()))
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-route-canary".to_string()), true)
                    .with_upstreams(anthropic.base_url.clone(), deepseek.base_url.clone()),
            )
    }

    fn unified_request(
        bridge: &BridgeHandle,
        body: &str,
        session: &str,
        agent: Option<&str>,
    ) -> String {
        let mut headers = vec![
            (UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token()),
            ("x-api-key", "claude-api-key-canary"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "oauth-2025-04-20,fixture-capability"),
            ("x-claude-code-session-id", session),
        ];
        if let Some(agent) = agent {
            headers.push(("x-claude-code-agent-id", agent));
        }
        request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages?beta=true",
                "claude-oauth-canary",
                body,
                &headers,
            ),
        )
    }

    fn captured_body(raw_request: &str) -> &str {
        raw_request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .expect("captured HTTP body")
    }

    fn captured_header<'a>(raw_request: &'a str, name: &str) -> &'a str {
        raw_request
            .split("\r\n")
            .find_map(|line| {
                let (candidate, value) = line.split_once(':')?;
                candidate.eq_ignore_ascii_case(name).then_some(value.trim())
            })
            .unwrap_or_else(|| panic!("captured upstream header {name:?}: {raw_request}"))
    }

    /// The smallest request the translator accepts.
    const PROBE_BODY: &str =
        r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
    const PROBE_STREAM_BODY: &str =
        r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}],"stream":true}"#;

    #[test]
    fn cache_health_fuse_stops_the_next_large_uncached_bridge_turn() {
        let upstream = FakeResponses::start_with_response(Some(large_uncached_response()));
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("bridge.jsonl");
        let mut bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_test_upstream_url(Some(upstream.base_url.clone()))
                .with_log_path(log_path.clone()),
        )
        .unwrap();
        let headers = [("X-Claude-Code-Session-Id", "raw-session-must-not-log")];
        for _ in 0..4 {
            let response = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    bridge.bearer_token(),
                    PROBE_BODY,
                    &headers,
                ),
            );
            assert_eq!(status(&response), 200, "{response}");
        }
        let refused = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_BODY,
                &headers,
            ),
        );
        assert_eq!(status(&refused), 400, "{refused}");
        assert!(refused.contains("invalid_request_error"), "{refused}");
        assert!(!refused.contains("overloaded_error"), "{refused}");
        assert!(refused.contains("diff range"), "{refused}");
        assert!(refused.contains("not retryable"), "{refused}");
        assert_eq!(
            upstream.requests().len(),
            4,
            "the fuse blocks before upstream"
        );
        bridge.shutdown().unwrap();
        let log = std::fs::read_to_string(log_path).unwrap();
        assert!(log.contains("cache_health_fuse_refused"));
        assert!(!log.contains("raw-session-must-not-log"));
        assert!(!log.contains("\"content\":\"hi\""));
    }

    #[test]
    fn terminal_codex_usage_is_published_to_the_session_status_writer() {
        let upstream = FakeResponses::start_with_response(Some(large_uncached_response()));
        let directory = tempfile::tempdir().unwrap();
        let writer = std::sync::Arc::new(crate::toast::statusline::StatusStateWriter::new(
            crate::toast::statusline::state_path(directory.path(), 77),
        ));
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        bridge.set_status_usage_writer(std::sync::Arc::clone(&writer));
        for agent in [None, Some("status-agent"), None] {
            let mut headers = vec![("X-Claude-Code-Session-Id", "status-session")];
            if let Some(agent) = agent {
                headers.push(("x-claude-code-agent-id", agent));
            }
            let response = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    bridge.bearer_token(),
                    PROBE_BODY,
                    &headers,
                ),
            );
            assert_eq!(status(&response), 200, "{response}");
        }
        let usage = wait_for_status_usage(&writer, 3);
        assert_eq!(usage.request_count, 3);
        assert_eq!(usage.uncached_input_tokens, 300_000);
        let cleared = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/_clud/context/clear",
                bridge.bearer_token(),
                r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"status-session"}"#,
            ),
        );
        assert_eq!(status(&cleared), 204, "{cleared}");
        // `/clear` drops the previous conversation's totals at once (#2045).
        assert_eq!(writer.usage_snapshot(), None);
        let response = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_BODY,
                &[("X-Claude-Code-Session-Id", "status-session")],
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        let usage = wait_for_status_usage(&writer, 1);
        assert_eq!(usage.provider, "codex");
        assert_eq!(usage.model, "gpt-6-sol");
        assert_eq!(usage.generation, 1);
        assert_eq!(usage.request_count, 1);
        assert_eq!(usage.cached_input_tokens, 0);
        assert_eq!(usage.uncached_input_tokens, 100_000);
        assert_eq!(usage.output_tokens, 1);
        // The first post-clear turn is cold.
        assert_eq!(usage.cache_health, "cold");
    }

    /// The client can finish reading the HTTP response a few instructions
    /// before the worker records its terminal telemetry. Wait only for the
    /// local state write, never for a provider or a new request.
    fn wait_for_status_usage(
        writer: &crate::toast::statusline::StatusStateWriter,
        request_count: u64,
    ) -> crate::toast::statusline::StatusUsage {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(usage) = crate::toast::statusline::read_live_usage(
                writer.path(),
                crate::toast::statusline::now_ms(),
            ) {
                if usage.request_count == request_count {
                    return usage;
                }
            }
            assert!(
                Instant::now() < deadline,
                "usage update {request_count} was not published"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn subagent_high_effort_request_is_counted_and_shown_on_the_statusline() {
        let upstream = FakeResponses::start_with_response(Some(large_uncached_response()));
        let directory = tempfile::tempdir().unwrap();
        let writer = std::sync::Arc::new(crate::toast::statusline::StatusStateWriter::new(
            crate::toast::statusline::state_path(directory.path(), 79),
        ));
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        bridge.set_status_usage_writer(std::sync::Arc::clone(&writer));
        for (agent, effort) in [(None, "medium"), (Some("effort-agent"), "high")] {
            let body = serde_json::json!({
                "model": "gpt-6-sol",
                "messages": [{"role": "user", "content": "effort fixture"}],
                "output_config": {"effort": effort},
                "stream": false,
            })
            .to_string();
            let mut headers = vec![("X-Claude-Code-Session-Id", "effort-session")];
            if let Some(agent) = agent {
                headers.push(("x-claude-code-agent-id", agent));
            }
            let _ = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    bridge.bearer_token(),
                    &body,
                    &headers,
                ),
            );
        }
        let args = crate::toast::statusline::RunArgs {
            session_pid: 79,
            state_dir: directory.path().to_path_buf(),
            chain_b64: None,
        };
        let mut out = Vec::new();
        crate::toast::statusline::render_into_with_effort_env(
            &mut out,
            &args,
            b"{}",
            crate::toast::statusline::now_ms(),
            None,
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("effort: medium"), "{text}");
        assert!(text.contains("agents: 1\u{d7}high"), "{text}");
    }

    #[test]
    fn unified_anthropic_routes_publish_exact_launch_wide_usage_to_statusline() {
        let claude = FakeResponses::start_with_response(Some(anthropic_usage_response(
            r#"{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30}"#,
            r#"{"output_tokens":4}"#,
        )));
        let codex = FakeResponses::start();
        let deepseek = FakeResponses::start_with_response(Some(anthropic_usage_response(
            r#"{"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20}"#,
            r#"{"completion_tokens":3}"#,
        )));
        let openrouter = FakeResponses::start_with_response(Some(anthropic_usage_response(
            r#"{"input_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":35}"#,
            r#"{"output_tokens":2}"#,
        )));
        let config = BridgeConfig::default()
            .with_test_upstream_url(Some(codex.base_url.clone()))
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-status-key".to_string()), false)
                    .with_openrouter(Some("openrouter-status-key".to_string()))
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                    .with_openrouter_upstream(openrouter.base_url.clone()),
            );
        let directory = tempfile::tempdir().unwrap();
        let writer = std::sync::Arc::new(crate::toast::statusline::StatusStateWriter::new(
            crate::toast::statusline::state_path(directory.path(), 78),
        ));
        let bridge = BridgeHandle::start(config).unwrap();
        bridge.set_status_usage_writer(std::sync::Arc::clone(&writer));

        for (model, session) in [
            ("claude-opus-4-1", "native-status"),
            ("clud-claude-deepseek-v4-pro-0813", "deepseek-status"),
            ("clud-claude-openrouter-sonnet", "openrouter-status"),
        ] {
            let body = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "status fixture"}],
                "stream": true,
            })
            .to_string();
            let response = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    "claude-oauth-status",
                    &body,
                    &[
                        (UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token()),
                        ("X-Claude-Code-Session-Id", session),
                        ("Anthropic-Version", "2023-06-01"),
                    ],
                ),
            );
            assert_eq!(status(&response), 200, "{model}: {response}");
            assert!(response.contains("message_start"), "{model}: {response}");
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        let usage = loop {
            if let Some(usage) = crate::toast::statusline::read_live_usage(
                writer.path(),
                crate::toast::statusline::now_ms(),
            ) {
                if usage.request_count == 3 {
                    break usage;
                }
            }
            assert!(
                Instant::now() < deadline,
                "unified usage update was not published"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(usage.provider, "openrouter");
        assert_eq!(usage.model, "~anthropic/claude-sonnet-latest");
        assert_eq!(usage.request_count, 3);
        assert_eq!(usage.cached_input_tokens, 145);
        assert_eq!(usage.uncached_input_tokens, 65);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.cache_health, "unavailable");
    }

    #[test]
    fn concurrent_turns_recheck_the_fuse_after_the_prior_turn_commits_usage() {
        let upstream = FakeResponses::start_with_response(Some(large_uncached_response()));
        let mut bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_test_upstream_url(Some(upstream.base_url.clone()))
                // Align request handling so the old request-time preflight
                // would admit all five before any terminal usage arrived.
                .with_request_hold(Duration::from_millis(100)),
        )
        .unwrap();
        let session = [("X-Claude-Code-Session-Id", "concurrent-cache-health")];
        let address = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let mut statuses = thread::scope(|scope| {
            let mut turns = Vec::new();
            for _ in 0..5 {
                let barrier = Arc::clone(&barrier);
                let token = token.clone();
                turns.push(scope.spawn(move || {
                    barrier.wait();
                    let response = request(
                        address,
                        &authorized_with_headers(
                            "POST",
                            "/v1/messages",
                            &token,
                            PROBE_BODY,
                            &session,
                        ),
                    );
                    status(&response)
                }));
            }
            turns
                .into_iter()
                .map(|turn| turn.join().unwrap())
                .collect::<Vec<_>>()
        });
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 200, 200, 200, 400]);
        assert_eq!(upstream.requests().len(), 4, "the fifth turn stays local");
        bridge.shutdown().unwrap();
    }

    /// One buffered Anthropic Messages body, as the upstream sends it.
    fn chat_messages_response() -> Vec<u8> {
        let body = r#"{"id":"msg_chat_1","type":"message","role":"assistant","model":"claude-x","content":[{"type":"text","text":"hello from messages"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":7}}"#;
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// The same turn as an Anthropic SSE stream.
    fn chat_messages_stream() -> Vec<u8> {
        response_with_events(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_chat_2\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-x\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n\
             event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )
    }

    fn chat_request(bridge: &BridgeHandle, body: &str, session: &str) -> String {
        request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/chat/completions",
                bridge.bearer_token(),
                body,
                &[
                    (UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token()),
                    ("x-claude-code-session-id", session),
                    ("content-type", "application/json"),
                ],
            ),
        )
    }

    /// `POST /v1/chat/completions` is translated in, routed by the ladder the
    /// gateway already owns, and translated back out.
    #[test]
    fn chat_completions_answers_in_the_openai_shape() {
        let anthropic = FakeResponses::start_with_response(Some(chat_messages_response()));
        let codex = FakeResponses::start_with_response(None);
        let deepseek = FakeResponses::start_with_response(None);
        let mut bridge =
            BridgeHandle::start(unified_config(&anthropic, &codex, &deepseek)).unwrap();
        let body =
            r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
        let response = chat_request(&bridge, body, "chat-shaped-session");
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("chat.completion"), "{response}");
        assert!(response.contains("hello from messages"), "{response}");
        assert!(
            response.contains("\"finish_reason\":\"stop\""),
            "{response}"
        );
        // The request that reached the upstream was a Messages request.
        let sent = anthropic.requests();
        assert_eq!(sent.len(), 1, "{}", sent.len());
        assert!(sent[0].contains("\"messages\""), "{}", sent[0]);
        assert!(
            !sent[0].contains("\"max_completion_tokens\""),
            "{}",
            sent[0]
        );
        bridge.shutdown().unwrap();
    }

    /// The standalone (served) surface suppresses upstream error detail. A chat
    /// caller must still receive an OpenAI envelope: the suppression path used
    /// to answer with the Anthropic shape, which an OpenAI client cannot parse.
    #[test]
    fn standalone_chat_failure_keeps_the_openai_error_shape() {
        let failure = {
            let body = r#"{"type":"error","error":{"type":"api_error","message":"upstream secret detail"}}"#;
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        };
        let anthropic = FakeResponses::start_with_response(Some(failure));
        let codex = FakeResponses::start_with_response(None);
        let deepseek = FakeResponses::start_with_response(None);
        let mut bridge = BridgeHandle::start(
            unified_config(&anthropic, &codex, &deepseek)
                .with_standalone_client_auth()
                // Without an upstream credential the route refuses before it
                // ever reaches the fake; the failure under test is the
                // upstream's, so supply one.
                .with_standalone_claude_credential(Arc::new(FixedStandaloneClaudeCredential)),
        )
        .unwrap();
        let body =
            r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
        let response = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/chat/completions",
                bridge.bearer_token(),
                body,
                &[("content-type", "application/json")],
            ),
        );
        assert_eq!(status(&response), 500, "{response}");
        let expected = String::from_utf8(crate::openai_ingress::render_chat_error(
            500,
            "the upstream route refused this turn",
        ))
        .unwrap();
        assert!(
            response.contains(&expected),
            "expected the OpenAI envelope {expected}, got {response}"
        );
        assert!(
            !response.contains("bridge error"),
            "the Anthropic sanitized body leaked to a chat client: {response}"
        );
        assert!(
            !response.contains("upstream secret detail"),
            "sanitization leaked the upstream message: {response}"
        );
        bridge.shutdown().unwrap();
    }

    /// A served chat caller with no configured upstream credential is refused in
    /// the OpenAI shape rather than the Anthropic one.
    #[test]
    fn standalone_chat_without_a_credential_is_refused_in_the_openai_shape() {
        let anthropic = FakeResponses::start_with_response(None);
        let codex = FakeResponses::start_with_response(None);
        let deepseek = FakeResponses::start_with_response(None);
        let mut bridge = BridgeHandle::start(
            unified_config(&anthropic, &codex, &deepseek).with_standalone_client_auth(),
        )
        .unwrap();
        let body =
            r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
        let response = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/chat/completions",
                bridge.bearer_token(),
                body,
                &[("content-type", "application/json")],
            ),
        );
        assert_eq!(status(&response), 503, "{response}");
        assert!(
            response.contains("the served backend has no usable upstream credential"),
            "{response}"
        );
        assert!(
            !response.contains("bridge error"),
            "the Anthropic sanitized body leaked to a chat client: {response}"
        );
        bridge.shutdown().unwrap();
    }

    /// The streaming half keeps its framing: OpenAI chunks, then `[DONE]`.
    #[test]
    fn chat_completions_streams_openai_chunks_then_done() {
        let anthropic = FakeResponses::start_with_response(Some(chat_messages_stream()));
        let codex = FakeResponses::start_with_response(None);
        let deepseek = FakeResponses::start_with_response(None);
        let mut bridge =
            BridgeHandle::start(unified_config(&anthropic, &codex, &deepseek)).unwrap();
        let body =
            r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
        let response = chat_request(&bridge, body, "chat-stream-session");
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("text/event-stream"), "{response}");
        assert!(response.contains("chat.completion.chunk"), "{response}");
        assert!(response.contains("hello "), "{response}");
        assert!(response.contains("data: [DONE]"), "{response}");
        bridge.shutdown().unwrap();
    }

    /// A part clud has no Anthropic spelling for is refused, not dropped, and
    /// the refusal is an OpenAI error envelope.
    #[test]
    fn chat_completions_refuses_an_untranslatable_part() {
        let anthropic = FakeResponses::start_with_response(Some(chat_messages_response()));
        let codex = FakeResponses::start_with_response(None);
        let deepseek = FakeResponses::start_with_response(None);
        let mut bridge =
            BridgeHandle::start(unified_config(&anthropic, &codex, &deepseek)).unwrap();
        let body = r#"{"model":"claude-x","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AAA","format":"wav"}}]}],"stream":false}"#;
        let response = chat_request(&bridge, body, "chat-refused-session");
        assert_eq!(status(&response), 400, "{response}");
        assert!(response.contains("invalid_request_error"), "{response}");
        assert!(response.contains("input_audio"), "{response}");
        assert!(
            anthropic.requests().is_empty(),
            "a refused turn must not reach the upstream"
        );
        bridge.shutdown().unwrap();
    }

    /// Codex writes Messages frames directly, so an OpenAI-shaped turn for a
    /// Codex model is refused in the client's shape instead of half-translated.
    #[test]
    fn chat_completions_refuses_a_codex_model() {
        let anthropic = FakeResponses::start_with_response(Some(chat_messages_response()));
        let codex = FakeResponses::start_with_response(None);
        let deepseek = FakeResponses::start_with_response(None);
        let mut bridge =
            BridgeHandle::start(unified_config(&anthropic, &codex, &deepseek)).unwrap();
        let body =
            r#"{"model":"codex-sol","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
        let response = chat_request(&bridge, body, "chat-codex-session");
        assert_eq!(status(&response), 400, "{response}");
        assert!(
            response.contains("api_error") || response.contains("invalid_request_error"),
            "{response}"
        );
        assert!(response.contains("Codex route"), "{response}");
        bridge.shutdown().unwrap();
    }

    /// A launch with no unified gateway has no OpenAI surface at all, and says
    /// so in the shape its client speaks.
    #[test]
    fn chat_completions_is_refused_without_a_unified_gateway() {
        let upstream = FakeResponses::start_with_response(Some(chat_messages_response()));
        let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let body =
            r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
        let response = chat_request(&bridge, body, "chat-no-gateway-session");
        assert_eq!(status(&response), 501, "{response}");
        assert!(response.contains("service_unavailable_error"), "{response}");
        assert!(upstream.requests().is_empty(), "{response}");
        bridge.shutdown().unwrap();
    }

    fn status(response: &str) -> u16 {
        response
            .split_whitespace()
            .nth(1)
            .expect("HTTP status")
            .parse()
            .unwrap()
    }

    /// Best-effort RSS snapshot for the opt-in bridge benchmark. It is a
    /// reported observation, not a test threshold: host allocators and test
    /// harnesses make an absolute memory limit flaky across target lanes.
    fn current_rss_bytes() -> u64 {
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        system.process(pid).map_or(0, |process| process.memory())
    }

    #[test]
    fn default_502_is_persisted_with_safe_diagnostics() {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789SECRET";
        let body = format!(r#"{{"error":{{"message":"bad gateway {secret}"}}}}"#);
        let reply = format!(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\ncf-ray: ray_772\r\nx-request-id: req_772\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let upstream = FakeResponses::start_with_response(Some(reply));
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let config = bridged_config(&upstream).with_log_path(log_path.clone());
        let mut bridge = BridgeHandle::start(config).unwrap();
        let bearer = bridge.bearer_token().to_string();
        let base_url = upstream.base_url.clone();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );
        assert_eq!(status(&response), 502);
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(log_path).unwrap();
        assert!(text.contains(r#""upstream_status":502"#), "{text}");
        assert!(text.contains(r#""class":"transient""#), "{text}");
        assert!(text.contains("ray_772"), "{text}");
        assert!(text.contains("req_772"), "{text}");
        for forbidden in [secret, bearer.as_str(), base_url.as_str(), "Authorization"] {
            assert!(!text.contains(forbidden), "leaked {forbidden:?}: {text}");
        }
    }

    /// A turn on the model the session was launched with has nothing to
    /// record. (A turn on a *substituted* model does -- #1007 -- which is why
    /// this uses the discovery id the harness really sends rather than
    /// `PROBE_BODY`'s `claude-x` placeholder.)
    #[test]
    fn successful_turn_creates_no_forensic_log() {
        let upstream = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge =
            BridgeHandle::start(bridged_config(&upstream).with_log_path(log_path.clone())).unwrap();
        let body = PROBE_BODY.replace("claude-x", "clud-claude-codex-terra");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
        );
        assert_eq!(status(&response), 200);
        bridge.shutdown().unwrap();
        assert!(!log_path.exists());
    }

    #[test]
    fn bearer_rejection_log_never_contains_the_bearer() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge =
            BridgeHandle::start(BridgeConfig::default().with_log_path(log_path.clone())).unwrap();
        let bearer = bridge.bearer_token().to_string();
        let response = request(
            bridge.socket_addr(),
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(status(&response), 401);
        bridge.shutdown().unwrap();
        let text = std::fs::read_to_string(log_path).unwrap();
        assert!(text.contains(r#""reason":"bearer_mismatch""#));
        assert!(!text.contains(&bearer));
    }

    #[test]
    fn bridge_log_cap_is_visible_through_the_real_bridge() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge = BridgeHandle::start(
            BridgeConfig::default()
                .with_log_path(log_path.clone())
                .with_log_max_bytes(150),
        )
        .unwrap();
        for _ in 0..10 {
            let _ = request(
                bridge.socket_addr(),
                "POST /nope HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            );
        }
        bridge.shutdown().unwrap();
        let text = std::fs::read_to_string(log_path).unwrap();
        assert!(text.contains(r#""event":"truncated""#), "{text}");
        for line in text.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }

    #[test]
    fn binds_ephemeral_loopback_ports_without_collision() {
        let first = BridgeHandle::start(BridgeConfig::default()).unwrap();
        let second = BridgeHandle::start(BridgeConfig::default()).unwrap();
        assert!(first.socket_addr().ip().is_loopback());
        assert!(second.socket_addr().ip().is_loopback());
        assert_ne!(first.socket_addr().port(), 0);
        assert_ne!(first.socket_addr(), second.socket_addr());
    }

    #[test]
    fn bearer_and_route_method_matrix_is_exact() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();

        let missing = request(
            addr,
            "HEAD /v1/messages HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status(&missing), 401);
        let wrong = request(addr, &authorized("HEAD", "/v1/messages", "wrong", ""));
        assert_eq!(status(&wrong), 401);

        let head = request(addr, &authorized("HEAD", "/v1/messages", &token, ""));
        assert_eq!(status(&head), 200);
        assert!(head.ends_with("\r\n\r\n"), "HEAD must not return a body");

        let non_stream = request(
            addr,
            &authorized("POST", "/v1/messages", &token, PROBE_BODY),
        );
        assert_eq!(status(&non_stream), 200);
        assert!(non_stream.contains("application/json"));
        assert!(non_stream.contains("\"type\":\"message\""));
        assert!(non_stream.contains("\"text\":\"bridged reply\""));

        let stream = request(
            addr,
            &authorized("POST", "/v1/messages", &token, PROBE_STREAM_BODY),
        );
        assert_eq!(status(&stream), 200);
        assert!(stream.contains("text/event-stream"));
        assert!(stream.contains("event: message_start\ndata:"));
        assert!(stream.contains("event: message_stop"));
        assert!(!stream.contains(r#"message_start\ndata:"#));

        let count = request(
            addr,
            &authorized("POST", "/v1/messages/count_tokens", &token, "{}"),
        );
        assert_eq!(status(&count), 404);
        assert!(count.contains("not supported"));
        for (method, path) in [
            ("GET", "/v1/messages"),
            ("POST", "/unknown"),
            ("HEAD", "/unknown"),
        ] {
            let response = request(addr, &authorized(method, path, &token, ""));
            assert_eq!(status(&response), 404, "{method} {path}");
        }
    }

    /// Issue #955: a future Codex row must become both discoverable and
    /// routable without adding another bridge-specific model list.
    #[test]
    fn direct_codex_catalog_advertises_and_routes_every_registered_row() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let token = bridge.bearer_token().to_owned();
        let catalog_response = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models?limit=1000", &token, ""),
        );
        assert_eq!(status(&catalog_response), 200, "{catalog_response}");
        let catalog: serde_json::Value = serde_json::from_str(
            catalog_response
                .split("\r\n\r\n")
                .nth(1)
                .expect("catalog response body"),
        )
        .unwrap();
        let ids = catalog["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        let registered = provider_catalog::models_for_provider(ModelProvider::Codex)
            .map(|entry| {
                (
                    entry.discovery_id.expect("Codex discovery id"),
                    entry.wire_id,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            registered
                .iter()
                .map(|(discovery_id, _)| *discovery_id)
                .collect::<Vec<_>>()
        );

        for (discovery_id, wire_id) in &registered {
            let body = PROBE_BODY.replace("claude-x", &format!("{discovery_id}@high"));
            let response = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", &token, &body),
            );
            assert_eq!(status(&response), 200, "{discovery_id}: {response}");
            let requests = upstream.requests();
            let sent: serde_json::Value =
                serde_json::from_str(captured_body(requests.last().unwrap())).unwrap();
            assert_eq!(sent["model"], *wire_id);
            assert_eq!(sent["reasoning"]["effort"], "high");
        }

        let unknown = PROBE_BODY.replace("claude-x", "clud-claude-codex-nova");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", &token, &unknown),
        );
        assert_eq!(status(&response), 400, "{response}");
        assert_eq!(upstream.requests().len(), registered.len());
    }

    /// Issue #997: only a `claude*` ID is caller-owned on this route. Any
    /// other ID the bridge cannot resolve must be refused here rather than
    /// translated to the Codex upstream, which would answer with an error
    /// about a model the user never knowingly sent there.
    ///
    /// Issue #1000: the two refusals must not read alike. A model clud has
    /// never heard of is the caller's to fix; a model clud does know but this
    /// gateway does not serve is a clud-side limit and must say so.
    #[test]
    fn an_unresolvable_non_claude_model_is_refused_before_the_translator() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let body = PROBE_BODY.replace("claude-x", "gpt-6-nonexistent");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
        );
        assert_eq!(status(&response), 400, "{response}");
        assert!(response.contains("invalid_request_error"), "{response}");
        assert!(
            response.contains("unknown clud Codex model 'gpt-6-nonexistent'"),
            "{response}"
        );
        assert!(!response.contains("clud knows the model"), "{response}");

        // Case 2: a real catalog row, served by another provider's route.
        let known = PROBE_BODY.replace("claude-x", "clud-claude-kimi-k3");
        let refused = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &known),
        );
        assert_eq!(status(&refused), 400, "{refused}");
        assert!(
            refused.contains("clud knows the model 'clud-claude-kimi-k3'"),
            "{refused}"
        );
        assert!(!refused.contains("unknown clud Codex model"), "{refused}");
        // Both refusals still say what this gateway can serve.
        for body in [&response, &refused] {
            assert!(body.contains("clud-claude-codex-terra"), "{body}");
        }
        assert!(
            upstream.requests().is_empty(),
            "a refused model must not reach the Codex upstream"
        );
    }

    #[test]
    fn sol_6_1_picker_and_wire_ids_reach_codex_without_a_launch_pin() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let catalog = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models", bridge.bearer_token(), ""),
        );
        assert!(catalog.contains("clud-claude-codex-sol-6-1"), "{catalog}");
        for selector in ["gpt-6.1-sol", "codex-sol-6.1", "clud-claude-codex-sol-6-1"] {
            let body = PROBE_BODY.replace("claude-x", &format!("{selector}@high"));
            let response = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
            );
            assert_eq!(status(&response), 200, "{selector}: {response}");
            let captured = upstream.requests().pop().unwrap();
            let sent: serde_json::Value = serde_json::from_str(captured_body(&captured)).unwrap();
            assert_eq!(sent["model"], "gpt-6.1-sol");
            assert_eq!(sent["reasoning"]["effort"], "high");
        }
        assert_eq!(upstream.requests().len(), 3);
    }

    /// Issue #1022: an uncataloged full ID the user pinned at launch reaches
    /// upstream verbatim.
    ///
    /// `--model gpt-5.7-nova` launched fine and then 400d on every turn after
    /// #1005 made the catalog a gate on the request path. `ModelSpec::parse`
    /// forwards unknown *full* ids on purpose -- that is how a user reaches a
    /// model released after clud's table was written -- and this is the other
    /// half of that contract.
    #[test]
    fn a_launch_pinned_uncataloged_model_reaches_upstream_unchanged() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(
            bridged_config(&upstream)
                .with_default_model(Some(ModelSpec::parse("gpt-5.7-nova@medium").unwrap())),
        )
        .unwrap();
        let body = PROBE_BODY.replace("claude-x", "gpt-5.7-nova");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
        );
        assert_eq!(status(&response), 200, "{response}");

        // Verbatim: the point of the escape hatch is that clud does not need a
        // row for the model to get out of the way.
        let sent = upstream.requests().remove(0);
        let sent_body = sent.split("\r\n\r\n").nth(1).expect("upstream body");
        let json: serde_json::Value = serde_json::from_str(sent_body).expect("JSON body");
        assert_eq!(json["model"], "gpt-5.7-nova", "{sent_body}");
    }

    /// #1007: a built-in Anthropic row picked from `/model` still runs on the
    /// launch selection (the picker cannot be constrained, DD-054), but the
    /// substitution is now recorded instead of silent.
    #[test]
    fn an_anthropic_pick_on_the_codex_route_is_served_by_the_default_and_recorded() {
        let upstream = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge = BridgeHandle::start(
            bridged_config(&upstream)
                .with_default_model(Some(ModelSpec::parse("sol@xhigh").unwrap()))
                .with_log_path(log_path.clone()),
        )
        .unwrap();
        let body = PROBE_BODY.replace("claude-x", "claude-opus-5");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
        );
        assert_eq!(status(&response), 200, "{response}");
        let sent = upstream.requests().remove(0);
        let sent_body = sent.split("\r\n\r\n").nth(1).expect("upstream body");
        let json: serde_json::Value = serde_json::from_str(sent_body).expect("JSON body");
        assert_eq!(json["model"], "gpt-6-sol", "{sent_body}");
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(text.contains(r#""event":"model_substituted""#), "{text}");
        assert!(text.contains(r#""requested":"claude-opus-5""#), "{text}");
        assert!(text.contains(r#""served":"gpt-6-sol@xhigh""#), "{text}");
        // Ambient: the session kept working, so it must not end on a
        // "problems were recorded" hint the terminal line already covered.
        let log = bridge.log.as_ref().expect("configured log");
        assert!(!lock_log(log).has_notable_records(), "{text}");
    }

    /// The harness's own side-model calls share the `claude` prefix and must
    /// not be reported as a pick, or every session would warn on its first
    /// title generation.
    #[test]
    fn a_haiku_side_call_is_not_reported_as_an_anthropic_pick() {
        let upstream = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge =
            BridgeHandle::start(bridged_config(&upstream).with_log_path(log_path.clone())).unwrap();
        let body = PROBE_BODY.replace("claude-x", "claude-haiku-4-5-20251001");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
        );
        assert_eq!(status(&response), 200, "{response}");
        bridge.shutdown().unwrap();

        // A log with nothing to say is never created, which is the point.
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(!text.contains("model_substituted"), "{text}");
    }

    #[test]
    fn anthropic_pick_predicate_keys_on_prefix_and_excludes_haiku() {
        assert!(is_anthropic_main_model_pick("claude-opus-5"));
        assert!(is_anthropic_main_model_pick("Claude-Sonnet-4-5[1m]"));
        assert!(!is_anthropic_main_model_pick("claude-haiku-4-5"));
        assert!(!is_anthropic_main_model_pick("claude-3-5-haiku-latest"));
        assert!(!is_anthropic_main_model_pick("clud-claude-codex-sol"));
        assert!(!is_anthropic_main_model_pick("gpt-5.6-sol"));
    }

    /// The narrowness is the whole design, so it gets its own test.
    ///
    /// #1005 exists because Claude Code merges this gateway's rows with its
    /// own built-in catalog, so IDs the *harness* invented arrive here looking
    /// exactly like IDs the *user* typed. Provenance is the only thing that
    /// separates them. If the check degraded into "any default is set", an
    /// invented ID would sail through on a session that merely happened to
    /// pin something -- which is #997 all over again.
    #[test]
    fn an_uncataloged_model_is_still_refused_when_it_is_not_the_pinned_one() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(
            bridged_config(&upstream)
                .with_default_model(Some(ModelSpec::parse("gpt-5.7-nova@medium").unwrap())),
        )
        .unwrap();
        // A pin *is* in effect -- just not for this ID.
        let body = PROBE_BODY.replace("claude-x", "gpt-9-invented-by-the-harness");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
        );
        assert_eq!(status(&response), 400, "{response}");
        assert!(
            response.contains("unknown clud Codex model 'gpt-9-invented-by-the-harness'"),
            "{response}"
        );
        assert!(
            upstream.requests().is_empty(),
            "a refused model must not reach the Codex upstream"
        );
    }

    /// The caveat #1022 names, pinned so it is a known limit rather than a
    /// surprise: the escape hatch is scoped to the launch selection, so
    /// `/model <newer-id>` mid-session still refuses. Widening it to any
    /// id-shaped string is remedy (a), which reopens what #1005 closed.
    #[test]
    fn switching_to_a_different_uncataloged_model_mid_session_still_refuses() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        // No launch pin at all: nothing is the user's own selection here.
        let body = PROBE_BODY.replace("claude-x", "gpt-5.7-nova");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
        );
        assert_eq!(status(&response), 400, "{response}");
        assert!(
            upstream.requests().is_empty(),
            "a refused model must not reach the Codex upstream"
        );
    }

    /// Issue #999: a session wedged by a model selection left nothing in the
    /// bridge log. The advertised set is the evidence that survives when
    /// nothing else does.
    #[test]
    fn a_catalog_fetch_records_the_advertised_model_ids() {
        let upstream = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge =
            BridgeHandle::start(bridged_config(&upstream).with_log_path(log_path.clone())).unwrap();
        let bearer = bridge.bearer_token().to_string();
        let response = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models?limit=1000", &bearer, ""),
        );
        assert_eq!(status(&response), 200, "{response}");
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(log_path).unwrap();
        assert!(text.contains(r#""event":"catalog_advertised""#), "{text}");
        assert!(text.contains("clud-claude-codex-terra"), "{text}");
        assert!(!text.contains(&bearer), "{text}");
        // Ambient context only: the shutdown hint stays a signal that this
        // launch left something worth reading, not a footer on every session.
        let log = bridge.log.as_ref().expect("configured log");
        assert!(!lock_log(log).has_notable_records());
    }

    /// Issue #1010 item 2: the unified route kept #1000's message after #1009
    /// gave the direct route a quoted ID, so a `--unified` user could not see
    /// what the gateway actually received.
    ///
    /// Only the quoting carries over. #1009's *second* case is unreachable
    /// here — see the comment at the refusal and
    /// `a_clud_claude_alias_would_need_the_unified_second_case`.
    #[test]
    fn the_unified_refusal_quotes_the_offending_id() {
        let fake = FakeResponses::start();
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                .with_upstreams(fake.base_url.clone(), fake.base_url.clone()),
        );
        let mut bridge = BridgeHandle::start(config).unwrap();
        let token = bridge.bearer_token().to_string();
        let body = PROBE_BODY.replace("claude-x", "clud-claude-nonexistent");
        let refused = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                "native-claude-credential",
                &body,
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, &token)],
            ),
        );
        assert_eq!(status(&refused), 400, "{refused}");
        assert!(
            refused.contains("unknown clud gateway model 'clud-claude-nonexistent'"),
            "{refused}"
        );
        // Still says what this gateway can serve.
        assert!(refused.contains("clud-claude-codex-terra"), "{refused}");
        bridge.shutdown().unwrap();
    }

    /// Issue #1010 item 1: a model clud *does* know was reported as unknown.
    ///
    /// The case discriminator was the Codex-route resolution chain, and
    /// `non_claude_model_by_any_id` excludes Claude **and** OpenRouter rows.
    /// So the tier aliases (`opus`/`sonnet`/`haiku`, whose wire ids carry no
    /// `claude` prefix and therefore reach this path) and
    /// `openrouter-claude-sonnet` were told they do not exist — the worst
    /// possible answer for a user in a Codex session trying to get back to
    /// Claude.
    #[test]
    fn a_model_clud_knows_is_never_reported_as_unknown() {
        let upstream = FakeResponses::start();
        let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let bearer = bridge.bearer_token().to_string();
        for model in ["opus", "sonnet", "haiku", "openrouter-claude-sonnet"] {
            let body = PROBE_BODY.replace("claude-x", model);
            let refused = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", &bearer, &body),
            );
            assert_eq!(status(&refused), 400, "{model}: {refused}");
            assert!(
                refused.contains("clud knows the model"),
                "{model} is in clud's catalog and must not be called unknown: {refused}"
            );
            assert!(
                !refused.contains("unknown clud Codex model"),
                "{model}: {refused}"
            );
        }
        // A genuinely unknown id still gets the case-1 answer, so the split
        // has not simply collapsed the other way.
        let body = PROBE_BODY.replace("claude-x", "gpt-6-nonexistent");
        let refused = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", &bearer, &body),
        );
        assert!(refused.contains("unknown clud Codex model"), "{refused}");
        bridge.shutdown().unwrap();
    }

    /// Issue #1010 item 3: the guard tested the raw `model`, while
    /// `resolve_selection` trims and treats empty as "serve the default".
    ///
    /// So `{"model":""}` was refused as `unknown clud Codex model ''` — a
    /// confusing thing to show anyone — and a leading space on an otherwise
    /// ordinary Claude id took the refusal too.
    #[test]
    fn an_empty_or_padded_model_serves_the_default_rather_than_refusing() {
        let upstream = FakeResponses::start();
        let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let bearer = bridge.bearer_token().to_string();
        for model in ["", " claude-opus-5", "claude-opus-5 "] {
            let body = PROBE_BODY.replace("claude-x", model);
            let response = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", &bearer, &body),
            );
            assert_ne!(
                status(&response),
                400,
                "{model:?} must serve the default, not refuse: {response}"
            );
            assert!(!response.contains("unknown clud Codex model"), "{response}");
        }
        bridge.shutdown().unwrap();
    }

    /// Issue #1021: #999's catalog record landed on the direct Codex route
    /// only, so a `--unified` launch wrote no `catalog_advertised` at all.
    ///
    /// It matters more here than there. The direct route advertises a fixed
    /// three rows, so the log adds little; the unified set varies with which
    /// provider credentials happen to be stored, and "what did the gateway
    /// actually offer this session?" is otherwise unanswerable afterwards.
    #[test]
    fn a_unified_catalog_fetch_records_the_advertised_model_ids() {
        let fake = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let config = BridgeConfig::default()
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                    .with_upstreams(fake.base_url.clone(), fake.base_url.clone()),
            )
            .with_log_path(log_path.clone());
        let mut bridge = BridgeHandle::start(config).unwrap();
        let token = bridge.bearer_token().to_string();
        let response = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "GET",
                "/v1/models?limit=1000",
                "native-claude-credential",
                "",
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, &token)],
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(text.contains(r#""event":"catalog_advertised""#), "{text}");
        // The recorded set must be the one actually served, not the whole
        // catalog: DeepSeek and Codex are credentialed here, Claude never is.
        assert!(text.contains("clud-claude-codex-terra"), "{text}");
        assert!(text.contains("clud-claude-deepseek-flash"), "{text}");
        // Claude rows are never advertised through the unified catalog, so
        // their absence is part of the contract this record captures.
        assert!(!text.contains("clud-claude-openrouter"), "{text}");
        assert!(
            !text.contains(&token),
            "the gateway token must never be logged"
        );
        // Ambient, exactly as on the direct route: this is context to read a
        // failure against, not a reason to look on its own.
        let log = bridge.log.as_ref().expect("configured log");
        assert!(!lock_log(log).has_notable_records());
    }

    /// Issue #1021: the direct route has recorded its refusals since #1000;
    /// the unified route's own `unknown clud gateway model` refusal recorded
    /// nothing, so a unified session wedged on a picker row left no evidence.
    #[test]
    fn a_unified_refused_model_is_recorded_with_its_id() {
        let fake = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let config = BridgeConfig::default()
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                    .with_upstreams(fake.base_url.clone(), fake.base_url.clone()),
            )
            .with_log_path(log_path.clone());
        let mut bridge = BridgeHandle::start(config).unwrap();
        let token = bridge.bearer_token().to_string();
        let body = PROBE_BODY.replace("claude-x", "clud-claude-nonexistent");
        let refused = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                "native-claude-credential",
                &body,
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, &token)],
            ),
        );
        assert_eq!(status(&refused), 400, "{refused}");
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(text.contains(r#""event":"request_rejected""#), "{text}");
        assert!(text.contains(r#""reason":"unknown_model""#), "{text}");
        assert!(text.contains("clud-claude-nonexistent"), "{text}");
        assert!(
            !text.contains(&token),
            "the gateway token must never be logged"
        );
        // A refusal *is* worth an operator's attention, unlike the catalog.
        let log = bridge.log.as_ref().expect("configured log");
        assert!(lock_log(log).has_notable_records());
    }

    /// Issue #999: the refusal added by #1005 named the model to the user but
    /// not to the log, and #1009's two cases must stay distinguishable there.
    #[test]
    fn a_refused_model_is_recorded_with_its_id_and_case() {
        let upstream = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge =
            BridgeHandle::start(bridged_config(&upstream).with_log_path(log_path.clone())).unwrap();
        let bearer = bridge.bearer_token().to_string();
        let oversized = "x".repeat(5000);
        for model in ["gpt-6-nonexistent", "clud-claude-kimi-k3", &oversized] {
            let body = PROBE_BODY.replace("claude-x", model);
            let refused = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", &bearer, &body),
            );
            assert_eq!(status(&refused), 400, "{refused}");
        }
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(log_path).unwrap();
        // Serialized field order is serde_json's (sorted): model, then reason.
        assert!(
            text.contains(r#""model":"gpt-6-nonexistent","reason":"unknown_model""#),
            "{text}"
        );
        assert!(
            text.contains(r#""model":"clud-claude-kimi-k3","reason":"model_not_served_here""#),
            "{text}"
        );
        // The model field comes from a 32 MiB-capped body: one oversized value
        // would otherwise exhaust the 1 MiB budget and silence every later
        // failure, so it is truncated before it reaches the log.
        assert!(text.contains(&"x".repeat(MAX_LOGGED_MODEL_CHARS)), "{text}");
        assert!(
            !text.contains(&"x".repeat(MAX_LOGGED_MODEL_CHARS + 1)),
            "{text}"
        );
        assert!(!text.contains(&bearer), "{text}");
    }

    // -----------------------------------------------------------------
    // #1257: the launch allowlist at the bridge.
    // -----------------------------------------------------------------

    #[test]
    fn a_model_outside_the_launch_allowlist_is_refused_with_the_boundary_named() {
        let upstream = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let config = bridged_config(&upstream)
            .with_allowed_models(vec!["codex-terra".to_string()])
            .with_log_path(log_path.clone());
        let mut bridge = BridgeHandle::start(config).unwrap();
        let bearer = bridge.bearer_token().to_string();
        let body = PROBE_BODY.replace("claude-x", "gpt-6-sol");
        let refused = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", &bearer, &body),
        );
        assert_eq!(status(&refused), 400, "{refused}");
        let payload = captured_body(&refused);
        assert!(payload.contains("gpt-6-sol"), "{payload}");
        assert!(payload.contains("codex-terra"), "{payload}");
        assert!(payload.contains("not allowed for this launch"), "{payload}");
        assert!(
            upstream.requests().is_empty(),
            "a refused model must not reach the upstream"
        );
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            text.contains(r#""model":"gpt-6-sol","reason":"model_not_allowed""#),
            "{text}"
        );
        assert!(!text.contains(&bearer), "{text}");
    }

    #[test]
    fn an_in_allowlist_model_and_the_harness_side_models_flow_normally() {
        let upstream = FakeResponses::start();
        let config = bridged_config(&upstream).with_allowed_models(vec!["codex-terra".to_string()]);
        let mut bridge = BridgeHandle::start(config).unwrap();
        let bearer = bridge.bearer_token().to_string();
        // The pinned row itself, addressed by wire id: forwarded, 200.
        let pinned = PROBE_BODY.replace("claude-x", "gpt-5.6-terra");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", &bearer, &pinned),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert_eq!(
            upstream.requests().len(),
            1,
            "the in-allowlist model is forwarded"
        );
        // Exemption 1: the harness's side-model machinery shares the haiku
        // substring and is served regardless of the boundary.
        let haiku = PROBE_BODY.replace("claude-x", "claude-haiku-4-5-20251001");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", &bearer, &haiku),
        );
        assert_eq!(status(&response), 200, "{response}");
        // Exemption 2: a claude* id is the DD-038 / #1007 substitution this
        // route performs with the caller's own Claude credential -- refusing
        // it would refuse clud's own translation.
        let anthropic_pick = PROBE_BODY.replace("claude-x", "claude-opus-5");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", &bearer, &anthropic_pick),
        );
        assert_eq!(status(&response), 200, "{response}");
        bridge.shutdown().unwrap();
    }

    #[test]
    fn a_pinned_codex_catalog_only_advertises_rows_the_launch_would_serve() {
        let upstream = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let config = bridged_config(&upstream)
            .with_allowed_models(vec!["codex-terra".to_string()])
            .with_log_path(log_path.clone());
        let mut bridge = BridgeHandle::start(config).unwrap();
        let bearer = bridge.bearer_token().to_string();
        let response = request(
            bridge.socket_addr(),
            &authorized("GET", "/v1/models?limit=1000", &bearer, ""),
        );
        assert_eq!(status(&response), 200, "{response}");
        let payload: serde_json::Value = serde_json::from_str(captured_body(&response)).unwrap();
        let ids = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["id"].as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"clud-claude-codex-terra"), "{ids:?}");
        assert!(!ids.contains(&"clud-claude-codex-sol"), "{ids:?}");
        assert!(!ids.contains(&"clud-claude-codex-luna"), "{ids:?}");
        for id in &ids {
            assert!(
                provider_catalog::model_allowlist_allows(&["codex-terra".to_string()], id),
                "advertised {id} is outside the launch allowlist"
            );
        }
        bridge.shutdown().unwrap();
    }

    #[test]
    fn a_unified_launch_refuses_and_logs_a_model_outside_its_allowlist() {
        let fake = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let config = BridgeConfig::default()
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                    .with_upstreams(fake.base_url.clone(), fake.base_url.clone()),
            )
            .with_allowed_models(vec!["deepseek-v4-pro".to_string()])
            .with_log_path(log_path.clone());
        let mut bridge = BridgeHandle::start(config).unwrap();
        // Outside the boundary: refused before any upstream is touched.
        let outside = PROBE_BODY.replace("claude-x", "deepseek-flash");
        let refused = unified_request(&bridge, &outside, "pin-refused", None);
        assert_eq!(status(&refused), 400, "{refused}");
        let payload = captured_body(&refused);
        assert!(payload.contains("deepseek-flash"), "{payload}");
        assert!(payload.contains("deepseek-v4-pro"), "{payload}");
        assert!(payload.contains("not allowed for this launch"), "{payload}");
        assert!(
            fake.requests().is_empty(),
            "a refused model must not reach any upstream"
        );
        // Inside the boundary: served normally.
        let inside = PROBE_BODY.replace("claude-x", "deepseek-v4-pro");
        let served = unified_request(&bridge, &inside, "pin-served", None);
        assert_eq!(status(&served), 200, "{served}");
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            text.contains(r#""model":"deepseek-flash","reason":"model_not_allowed""#),
            "{text}"
        );
    }

    #[test]
    fn a_pinned_unified_catalog_only_advertises_rows_the_launch_would_serve() {
        let fake = FakeResponses::start();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let config = BridgeConfig::default()
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                    .with_upstreams(fake.base_url.clone(), fake.base_url.clone()),
            )
            .with_allowed_models(vec!["deepseek-flash".to_string()])
            .with_log_path(log_path.clone());
        let mut bridge = BridgeHandle::start(config).unwrap();
        let token = bridge.bearer_token().to_string();
        let response = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "GET",
                "/v1/models?limit=1000",
                "native-claude-credential",
                "",
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, token.as_str())],
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        let payload: serde_json::Value = serde_json::from_str(captured_body(&response)).unwrap();
        let ids = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["clud-claude-deepseek-flash"], "{ids:?}");
        bridge.shutdown().unwrap();

        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(text.contains(r#""event":"catalog_advertised""#), "{text}");
        assert!(text.contains("clud-claude-deepseek-flash"), "{text}");
        assert!(!text.contains("clud-claude-codex-terra"), "{text}");
    }

    #[test]
    fn unified_catalog_requires_its_custom_token_and_omits_unavailable_routes() {
        let fake = FakeResponses::start();
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                .with_upstreams(fake.base_url.clone(), fake.base_url.clone()),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let addr = bridge.socket_addr();
        let missing = request(
            addr,
            &authorized("GET", "/v1/models?limit=1000", bridge.bearer_token(), ""),
        );
        assert_eq!(status(&missing), 401, "bearer auth is never unified auth");
        let response = request(
            addr,
            &authorized_with_headers(
                "GET",
                "/v1/models?limit=1000",
                "native-claude-credential",
                "",
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token())],
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        let catalog: serde_json::Value = serde_json::from_str(
            response
                .split("\r\n\r\n")
                .nth(1)
                .expect("catalog response body"),
        )
        .unwrap();
        let rows = catalog["data"].as_array().unwrap();
        let ids = rows
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "clud-claude-codex-sol",
                "clud-claude-codex-terra",
                "clud-claude-codex-luna",
                "clud-claude-codex-sol-6-1",
                "clud-claude-codex-terra-6-1",
                "clud-claude-deepseek-v4-pro-0813",
                "clud-claude-deepseek-flash",
            ],
            "catalog ordering is part of the picker contract"
        );
        assert_eq!(
            rows.iter()
                .map(|row| row["display_name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "Codex Sol (OpenAI)",
                "Codex Terra (OpenAI)",
                "Codex Luna (OpenAI)",
                "Codex Sol 6.1 (OpenAI)",
                "Codex Terra 6.1 (OpenAI)",
                "DeepSeek V4 Pro 0813",
                "DeepSeek V4.1 Flash",
            ]
        );
        for secret in [bridge.bearer_token(), "deepseek-test-secret"] {
            assert!(!format!("{bridge:?}").contains(secret));
            assert!(!response.contains(secret));
        }

        let unavailable = BridgeHandle::start(
            BridgeConfig::default().with_unified_gateway(UnifiedGatewayConfig::new(None, false)),
        )
        .unwrap();
        let response = request(
            unavailable.socket_addr(),
            &authorized_with_headers(
                "GET",
                "/v1/models?limit=1000",
                "native-claude-credential",
                "",
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, unavailable.bearer_token())],
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains(r#""data":[]"#), "{response}");
    }

    fn unified_message_request(
        bridge: &BridgeHandle,
        model: &str,
        session_id: &str,
        caller_credential: &str,
    ) -> String {
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": format!("route {model}")}],
            "stream": false,
        })
        .to_string();
        authorized_with_headers(
            "POST",
            "/v1/messages",
            caller_credential,
            &body,
            &[
                (UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token()),
                ("X-Claude-Code-Session-Id", session_id),
                ("X-Api-Key", "native-claude-api-key"),
                ("Anthropic-Version", "2023-06-01"),
                ("Anthropic-Beta", "context-1m-2025-08-07"),
            ],
        )
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    fn unified_routes_all_six_ids_with_provider_credential_isolation() {
        let codex = FakeResponses::start();
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let config = BridgeConfig::default()
            .with_test_upstream_url(Some(codex.base_url.clone()))
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), true)
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone()),
            );
        let bridge = BridgeHandle::start(config).unwrap();

        let native = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "claude-opus-4-1",
                "unified-native",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(status(&native), 200, "{native}");

        for model in [
            "clud-claude-codex-sol",
            "clud-claude-codex-terra",
            "clud-claude-codex-luna",
            "clud-claude-codex-sol-6-1",
            "clud-claude-deepseek-v4-pro-0813",
            "clud-claude-deepseek-flash",
        ] {
            let response = request(
                bridge.socket_addr(),
                &unified_message_request(
                    &bridge,
                    model,
                    "unified-synthetic",
                    "native-claude-oauth-canary",
                ),
            );
            assert_eq!(status(&response), 200, "{model}: {response}");
        }

        let claude_request = claude.requests().pop().unwrap();
        assert!(claude_request.starts_with("POST /v1/messages "));
        assert!(claude_request.contains("claude-opus-4-1"));
        assert!(claude_request.contains("Bearer native-claude-oauth-canary"));
        assert!(claude_request.contains("native-claude-api-key"));
        assert!(claude_request.contains("Anthropic-Beta: context-1m-2025-08-07"));
        assert!(!claude_request.contains(bridge.bearer_token()));
        assert!(!claude_request.contains("deepseek-vault-canary"));
        assert!(!claude_request.contains("clud-test-upstream-key"));

        let codex_requests = codex.requests();
        assert_eq!(codex_requests.len(), 4);
        for (raw, wire_id) in
            codex_requests
                .iter()
                .zip(["gpt-6-sol", "gpt-5.6-terra", "gpt-6-luna", "gpt-6.1-sol"])
        {
            assert!(raw.starts_with("POST /v1/responses "));
            assert!(raw.contains(wire_id), "{raw}");
            assert!(raw.contains("Bearer clud-test-upstream-key"));
            for forbidden in [
                bridge.bearer_token(),
                "native-claude-oauth-canary",
                "native-claude-api-key",
                "deepseek-vault-canary",
            ] {
                assert!(!raw.contains(forbidden), "Codex leaked {forbidden}: {raw}");
            }
        }

        let deepseek_requests = deepseek.requests();
        assert_eq!(deepseek_requests.len(), 2);
        for (raw, wire_id) in deepseek_requests
            .iter()
            .zip(["deepseek-v4-pro[1m]", "deepseek-flash[1m]"])
        {
            assert!(raw.starts_with("POST /v1/messages "));
            assert!(raw.contains(wire_id), "{raw}");
            assert!(raw.contains("Bearer deepseek-vault-canary"));
            assert!(raw.contains("Anthropic-Version: 2023-06-01"));
            assert!(raw.contains("Anthropic-Beta: context-1m-2025-08-07"));
            for forbidden in [
                bridge.bearer_token(),
                "native-claude-oauth-canary",
                "native-claude-api-key",
                "clud-test-upstream-key",
            ] {
                assert!(
                    !raw.contains(forbidden),
                    "DeepSeek leaked {forbidden}: {raw}"
                );
            }
        }

        let before = codex.requests().len() + claude.requests().len() + deepseek.requests().len();
        let unknown = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-unknown",
                "unified-unknown",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(status(&unknown), 400, "{unknown}");
        assert_eq!(
            codex.requests().len() + claude.requests().len() + deepseek.requests().len(),
            before,
            "reserved unknown IDs must fail before any upstream request"
        );
    }

    fn unified_catalog_ids_of(bridge: &BridgeHandle) -> Vec<String> {
        let response = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "GET",
                "/v1/models?limit=1000",
                "native-claude-credential",
                "",
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token())],
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        let catalog: serde_json::Value =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        catalog["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                format!(
                    "{} | {}",
                    row["id"].as_str().unwrap(),
                    row["display_name"].as_str().unwrap()
                )
            })
            .collect()
    }

    /// #937 Phase 4: Kimi is a unified route like DeepSeek -- advertised only
    /// with its own key, served with only that key, and structurally unable
    /// to see another provider's credential or have its key seen by one.
    #[test]
    fn unified_kimi_route_is_key_gated_and_credential_isolated() {
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let kimi = FakeResponses::start();

        let without = BridgeHandle::start(
            BridgeConfig::default().with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                    .with_route_upstream(ModelProvider::Kimi, kimi.base_url.clone()),
            ),
        )
        .unwrap();
        assert!(
            !unified_catalog_ids_of(&without)
                .iter()
                .any(|row| row.contains("kimi")),
            "no Kimi key, no Kimi row"
        );
        let refused = request(
            without.socket_addr(),
            &unified_message_request(&without, "clud-claude-kimi-k3", "no-kimi", "native-oauth"),
        );
        assert_eq!(status(&refused), 400, "{refused}");
        assert!(
            kimi.requests().is_empty(),
            "a keyless route must never be dialed"
        );

        // The key is added after the upstream override on purpose: the
        // override must survive, or a test key would dial the real endpoint.
        let bridge = BridgeHandle::start(
            BridgeConfig::default().with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                    .with_route_upstream(ModelProvider::Kimi, kimi.base_url.clone())
                    .with_route(ModelProvider::Kimi, Some("kimi-vault-canary".to_string())),
            ),
        )
        .unwrap();
        assert_eq!(
            unified_catalog_ids_of(&bridge),
            vec![
                "clud-claude-deepseek-v4-pro-0813 | DeepSeek V4 Pro 0813",
                "clud-claude-deepseek-flash | DeepSeek V4.1 Flash",
                "clud-claude-kimi-k3 | Kimi K3",
            ]
        );
        for model in ["clud-claude-kimi-k3", "kimi-k3", "kimi-k3[1m]"] {
            let response = request(
                bridge.socket_addr(),
                &unified_message_request(&bridge, model, "kimi-session", "native-oauth-canary"),
            );
            assert_eq!(status(&response), 200, "{model}: {response}");
        }
        let deepseek_turn = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "kimi-session",
                "native-oauth-canary",
            ),
        );
        assert_eq!(status(&deepseek_turn), 200, "{deepseek_turn}");

        let kimi_requests = kimi.requests();
        assert_eq!(kimi_requests.len(), 3);
        for raw in &kimi_requests {
            assert!(raw.starts_with("POST /v1/messages "), "{raw}");
            assert!(raw.contains("kimi-k3[1m]"), "{raw}");
            assert!(raw.contains("Bearer kimi-vault-canary"), "{raw}");
            for forbidden in [
                bridge.bearer_token(),
                "native-oauth-canary",
                "native-claude-api-key",
                "deepseek-vault-canary",
            ] {
                assert!(!raw.contains(forbidden), "Kimi leaked {forbidden}: {raw}");
            }
        }
        let deepseek_requests = deepseek.requests();
        assert_eq!(deepseek_requests.len(), 1);
        assert!(deepseek_requests[0].contains("Bearer deepseek-vault-canary"));
        assert!(!deepseek_requests[0].contains("kimi-vault-canary"));
        assert!(
            claude.requests().is_empty(),
            "no Kimi or DeepSeek ID may fall through to Anthropic"
        );
        assert!(!format!("{bridge:?}").contains("kimi-vault-canary"));

        // Token counting is provider-specific, so a Kimi ID gets the local
        // 404 that makes Claude Code estimate instead of asking Anthropic.
        let count = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages/count_tokens",
                "native-oauth-canary",
                r#"{"model":"clud-claude-kimi-k3","messages":[{"role":"user","content":"x"}]}"#,
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token())],
            ),
        );
        assert_eq!(status(&count), 404, "{count}");
        assert_eq!(kimi.requests().len(), 3, "count_tokens never reaches Kimi");
        assert!(claude.requests().is_empty(), "or Anthropic");
    }

    /// Build a raw upstream HTTP response for a canary to replay verbatim.
    fn raw_response(status: u16, reason: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn non_message_json_from_a_provider_fails_over_before_http_200_is_committed() {
        let valid = r#"{"id":"msg_ok","type":"message","role":"assistant","model":"claude-opus-4-1","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let claude = FakeResponses::start_with_response(Some(raw_response(200, "OK", valid)));
        let deepseek = FakeResponses::start_with_response(Some(raw_response(
            200,
            "OK",
            r#"{"error":{"message":"provider gateway fault"}}"#,
        )));
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                .with_failover(FailoverLadder::parse("claude-opus-4-1", true).unwrap()),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "malformed-json-session",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("msg_ok"), "{response}");
        assert!(!response.contains("provider gateway fault"), "{response}");
        assert_eq!(deepseek.requests().len(), 1);
        assert_eq!(claude.requests().len(), 1);
    }

    #[test]
    fn non_message_json_without_a_fallback_is_a_named_gateway_error() {
        let claude = FakeResponses::start();
        let deepseek =
            FakeResponses::start_with_response(Some(raw_response(200, "OK", r#"{"choices":[]}"#)));
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone()),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "malformed-no-fallback",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(status(&response), 502, "{response}");
        assert!(response.contains("gateway route deepseek"), "{response}");
        assert_eq!(deepseek.requests().len(), 1, "do not reissue malformed 200");
        assert!(claude.requests().is_empty());
    }

    #[test]
    fn zero_complete_sse_events_use_the_same_fault_as_non_message_json() {
        for body in ["", "event: message_start\n", ": data: fake event\n\n"] {
            let claude = FakeResponses::start();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes();
            let deepseek = FakeResponses::start_with_response(Some(reply));
            let config = BridgeConfig::default().with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone()),
            );
            let bridge = BridgeHandle::start(config).unwrap();
            let response = request(
                bridge.socket_addr(),
                &unified_message_request(
                    &bridge,
                    "clud-claude-deepseek-flash",
                    "empty-stream-session",
                    "native-claude-oauth-canary",
                ),
            );
            assert_eq!(status(&response), 502, "{body:?}: {response}");
            assert!(
                response.contains("gateway route deepseek"),
                "{body:?}: {response}"
            );
        }
    }

    #[test]
    fn empty_sse_stream_descends_to_a_healthy_route() {
        let valid = r#"{"id":"msg_ok","type":"message","role":"assistant","model":"claude-opus-4-1","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let claude = FakeResponses::start_with_response(Some(raw_response(200, "OK", valid)));
        let empty = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let deepseek = FakeResponses::start_with_response(Some(empty));
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                .with_failover(FailoverLadder::parse("claude-opus-4-1", true).unwrap()),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "empty-stream-failover",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("msg_ok"), "{response}");
        assert_eq!(deepseek.requests().len(), 1);
        assert_eq!(claude.requests().len(), 1);
    }

    #[test]
    fn text_plain_response_keeps_the_existing_proxy_behavior() {
        let claude = FakeResponses::start();
        let body = "legacy plain response";
        let plain = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let deepseek = FakeResponses::start_with_response(Some(plain));
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone()),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "plain-session",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains(body), "{response}");
        assert!(claude.requests().is_empty());
    }

    /// The whole point of #968: a spent route is replayed onto the next rung
    /// *before* a byte reaches the client, so the client sees one ordinary 200
    /// and never learns a provider declined.
    #[test]
    fn an_exhausted_route_is_replayed_onto_the_next_rung_before_the_client_sees_anything() {
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start_with_responses(vec![Some(raw_response(
            429,
            "Too Many Requests",
            r#"{"error":{"message":"Rate limit exceeded: free-models-per-day-stealth"}}"#,
        ))]);
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                .with_failover(FailoverLadder::parse("claude-opus-4-1", true).unwrap()),
        );
        let bridge = BridgeHandle::start(config).unwrap();

        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "failover-session",
                "native-claude-oauth-canary",
            ),
        );

        assert_eq!(
            status(&response),
            200,
            "the client must never see the exhausted route's 429: {response}"
        );
        assert!(
            !response.contains("free-models-per-day"),
            "no part of the declined response may reach the client: {response}"
        );

        let deepseek_requests = deepseek.requests();
        assert_eq!(
            deepseek_requests.len(),
            1,
            "the spent route must be tried exactly once"
        );

        let replayed = claude.requests().pop().expect("the rung was taken");
        assert!(
            replayed.contains("claude-opus-4-1"),
            "the replay must carry the rung's model: {replayed}"
        );
        assert!(
            replayed.contains("route clud-claude-deepseek-flash"),
            "the replay must carry the caller's original transcript: {replayed}"
        );
        assert!(
            !replayed.contains("deepseek-vault-canary"),
            "credentials must not cross a provider boundary: {replayed}"
        );
    }

    /// The reported wedge, reproduced and fixed (#968).
    ///
    /// A session on OpenRouter's free daily tier exhausted its quota and had no
    /// in-session escape: `/model` moves the model ID, not the upstream. Routed
    /// through the gateway, the same exhaustion lands on the Claude rung and
    /// the turn completes.
    #[test]
    fn an_exhausted_openrouter_route_continues_on_claude_without_the_client_noticing() {
        let claude = FakeResponses::start();
        let openrouter = FakeResponses::start_with_responses(vec![Some(raw_response(
            429,
            "Too Many Requests",
            r#"{"error":{"message":"Rate limit exceeded: free-models-per-day-stealth"}}"#,
        ))]);
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(None, false)
                .with_openrouter(Some("openrouter-vault-canary".to_string()))
                .with_upstreams(claude.base_url.clone(), String::new())
                .with_openrouter_upstream(openrouter.base_url.clone())
                .with_failover(FailoverLadder::parse("claude-opus-4-1", true).unwrap()),
        );
        let bridge = BridgeHandle::start(config).unwrap();

        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-openrouter-sonnet",
                "openrouter-session",
                "native-claude-oauth-canary",
            ),
        );

        assert_eq!(status(&response), 200, "{response}");
        let sent = openrouter.requests().pop().expect("openrouter was tried");
        assert!(
            sent.contains("~anthropic/claude-sonnet-latest"),
            "the reviewed OpenRouter wire ID must be used: {sent}"
        );
        assert!(
            sent.contains("Bearer openrouter-vault-canary"),
            "OpenRouter must receive its own credential: {sent}"
        );

        let rescued = claude.requests().pop().expect("the Claude rung was taken");
        assert!(rescued.contains("claude-opus-4-1"), "{rescued}");
        assert!(
            !rescued.contains("openrouter-vault-canary"),
            "credentials must not cross a provider boundary: {rescued}"
        );
    }

    /// The operator surface: what the ladder is, what each route's health is,
    /// and the escape hatch for a clock-less failure.
    #[test]
    fn route_status_reports_health_and_clear_restores_a_drained_route() {
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start_with_responses(vec![Some(raw_response(
            402,
            "Payment Required",
            r#"{"error":{"message":"This request requires more credits"}}"#,
        ))]);
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                .with_failover(
                    FailoverLadder::parse("claude-opus-4-1,deepseek-flash", false).unwrap(),
                ),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();

        let status_request = || {
            request(
                addr,
                &authorized_with_headers(
                    "GET",
                    "/_clud/route/status",
                    "native-claude-oauth-canary",
                    "",
                    &[(UNIFIED_GATEWAY_TOKEN_HEADER, &token)],
                ),
            )
        };

        let before = status_request();
        assert_eq!(status(&before), 200, "{before}");
        assert!(
            before.contains("\"cost\":\"subscription\""),
            "the ladder must name who pays: {before}"
        );
        assert!(
            before.contains("\"withheld_for_consent\":true"),
            "a metered rung without consent must say so: {before}"
        );

        // Drain DeepSeek, then confirm the ledger reports it with no clock.
        let drained = request(
            addr,
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "status-session",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(
            status(&drained),
            200,
            "the Claude rung serves it: {drained}"
        );

        let after = status_request();
        assert!(
            after.contains("\"status\":\"down\"") && after.contains("\"reason\":\"drained\""),
            "a spent balance must be reported as down, not cooling: {after}"
        );

        let cleared = request(
            addr,
            &authorized_with_headers(
                "POST",
                "/_clud/route/clear",
                "native-claude-oauth-canary",
                r#"{"route":"deepseek"}"#,
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, &token)],
            ),
        );
        assert_eq!(status(&cleared), 200, "{cleared}");
        let restored = status_request();
        assert!(
            !restored.contains("\"reason\":\"drained\""),
            "clearing must restore the route a top-up fixed: {restored}"
        );
    }

    /// `route_health` asserts this at the unit level; this pins the gateway
    /// wiring to the same contract. A committed *failure* says nothing about
    /// the route, so it must not clear a cooldown the ledger is still serving.
    #[test]
    fn a_committed_failure_does_not_clear_a_cooling_route() {
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start_with_responses(vec![
            // First: drains the route and is failed over.
            Some(raw_response(
                402,
                "Payment Required",
                r#"{"error":{"message":"This request requires more credits"}}"#,
            )),
            // Second: a malformed request, committed straight through.
            Some(raw_response(
                400,
                "Bad Request",
                r#"{"error":{"type":"invalid_request_error","message":"bad shape"}}"#,
            )),
        ]);
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                .with_failover(FailoverLadder::parse("claude-opus-4-1", true).unwrap()),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();

        for session in ["drain", "fatal"] {
            let _ = request(
                addr,
                &unified_message_request(
                    &bridge,
                    "clud-claude-deepseek-flash",
                    session,
                    "native-claude-oauth-canary",
                ),
            );
        }

        let status_body = request(
            addr,
            &authorized_with_headers(
                "GET",
                "/_clud/route/status",
                "native-claude-oauth-canary",
                "",
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, &token)],
            ),
        );
        assert!(
            status_body.contains("\"reason\":\"drained\""),
            "the 400 must not have resurrected the drained route: {status_body}"
        );
    }

    /// The control surface is gateway-token gated exactly like `/v1/messages`.
    #[test]
    fn route_status_requires_the_gateway_token() {
        let claude = FakeResponses::start();
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(None, false)
                .with_upstreams(claude.base_url.clone(), String::new()),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let denied = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "GET",
                "/_clud/route/status",
                "native-claude-oauth-canary",
                "",
                &[(UNIFIED_GATEWAY_TOKEN_HEADER, "wrong-token")],
            ),
        );
        assert_eq!(status(&denied), 401, "{denied}");
    }

    /// A malformed request fails identically everywhere, so replaying it would
    /// only spend a second account to reproduce the same error.
    #[test]
    fn a_request_fatal_failure_never_descends_the_ladder() {
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start_with_responses(vec![Some(raw_response(
            400,
            "Bad Request",
            r#"{"error":{"type":"invalid_request_error","message":"bad shape"}}"#,
        ))]);
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                .with_failover(FailoverLadder::parse("claude-opus-4-1", true).unwrap()),
        );
        let bridge = BridgeHandle::start(config).unwrap();

        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "fatal-session",
                "native-claude-oauth-canary",
            ),
        );

        assert_eq!(status(&response), 400, "{response}");
        assert!(
            claude.requests().is_empty(),
            "a 400 must not be replayed onto a second account"
        );
    }

    /// The one 400 that *does* descend the ladder. A guardrail exclusion leaves
    /// the request well-formed and only the route's own configuration
    /// unwilling, so replaying the identical bytes onto the next rung is safe
    /// and is the difference between a relaunch and a re-route.
    #[test]
    fn a_policy_rejected_route_is_replayed_onto_the_next_rung() {
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start_with_responses(vec![Some(raw_response(
            400,
            "Bad Request",
            "API Error: 400 0 endpoints out of 1 requested are available \
             matching your guardrail restrictions and data policy. \
             ZDR violation (guardrail)",
        ))]);
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                .with_failover(FailoverLadder::parse("claude-opus-4-1", true).unwrap()),
        );
        let bridge = BridgeHandle::start(config).unwrap();

        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "policy-rejected-session",
                "native-claude-oauth-canary",
            ),
        );

        // The client sees one 200 and never learns the first rung declined —
        // the pre-commit replay is what keeps a policy rejection invisible.
        assert_eq!(status(&response), 200, "{response}");
        assert_eq!(
            claude.requests().len(),
            1,
            "the identical bytes must be replayed onto the fallback rung"
        );
    }

    /// With no ladder configured the gateway takes exactly today's path: the
    /// upstream status reaches the client unchanged and nothing is buffered.
    #[test]
    fn without_a_ladder_the_upstream_status_reaches_the_client_unchanged() {
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start_with_responses(vec![Some(raw_response(
            429,
            "Too Many Requests",
            r#"{"error":{"message":"Rate limit exceeded: free-models-per-day-stealth"}}"#,
        ))]);
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), false)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone()),
        );
        let bridge = BridgeHandle::start(config).unwrap();

        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "clud-claude-deepseek-flash",
                "no-ladder-session",
                "native-claude-oauth-canary",
            ),
        );

        assert_eq!(status(&response), 429, "{response}");
        assert!(
            claude.requests().is_empty(),
            "an unconfigured ladder must never reach another provider"
        );
    }

    #[test]
    fn unified_wire_ids_route_to_their_own_provider_not_anthropic() {
        let codex = FakeResponses::start();
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let config = BridgeConfig::default()
            .with_test_upstream_url(Some(codex.base_url.clone()))
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-vault-canary".to_string()), true)
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone()),
            );
        let bridge = BridgeHandle::start(config).unwrap();

        // A persisted or continued session can carry provider wire IDs
        // instead of discovery IDs. Each must reach its own upstream.
        let deepseek_wire = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "deepseek-v4-pro[1m]",
                "unified-wire-deepseek",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(status(&deepseek_wire), 200, "{deepseek_wire}");
        let deepseek_request = deepseek.requests().pop().unwrap();
        assert!(deepseek_request.starts_with("POST /v1/messages "));
        assert!(deepseek_request.contains("deepseek-v4-pro[1m]"));
        assert!(deepseek_request.contains("Bearer deepseek-vault-canary"));
        assert!(!deepseek_request.contains("native-claude-oauth-canary"));

        let codex_wire = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "gpt-5.6-terra",
                "unified-wire-codex",
                "native-claude-oauth-canary",
            ),
        );
        assert_eq!(status(&codex_wire), 200, "{codex_wire}");
        let codex_request = codex.requests().pop().unwrap();
        assert!(codex_request.starts_with("POST /v1/responses "));
        assert!(codex_request.contains("gpt-5.6-terra"));
        assert!(codex_request.contains("Bearer clud-test-upstream-key"));
        assert!(!codex_request.contains("native-claude-oauth-canary"));

        assert!(
            claude.requests().is_empty(),
            "a known provider wire ID must never reach Anthropic"
        );
    }

    #[test]
    fn unified_native_count_tokens_is_proxied_with_claude_auth() {
        let body = r#"{"input_tokens":7}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let claude = FakeResponses::start_with_response(Some(reply));
        let other = FakeResponses::start();
        let config = BridgeConfig::default().with_unified_gateway(
            UnifiedGatewayConfig::new(None, false)
                .with_upstreams(claude.base_url.clone(), other.base_url.clone()),
        );
        let bridge = BridgeHandle::start(config).unwrap();
        let request_body =
            r#"{"model":"claude-opus-4-1","messages":[{"role":"user","content":"measure"}]}"#;
        let response = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages/count_tokens",
                "native-count-token-canary",
                request_body,
                &[
                    (UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token()),
                    ("Anthropic-Version", "2023-06-01"),
                ],
            ),
        );
        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains(r#"{"input_tokens":7}"#));
        let upstream = claude.requests().pop().unwrap();
        assert!(upstream.starts_with("POST /v1/messages/count_tokens "));
        assert!(upstream.contains("Bearer native-count-token-canary"));
        assert!(!upstream.contains(bridge.bearer_token()));

        for synthetic in [
            serde_json::json!({
                "model": "clud-claude-codex-sol",
                "messages": [{"role": "user", "content": "measure"}],
            })
            .to_string(),
            // A persisted session may name the provider wire ID instead of
            // its discovery ID; it must not be proxied to Anthropic.
            serde_json::json!({
                "model": "deepseek-v4-pro[1m]",
                "messages": [{"role": "user", "content": "measure"}],
            })
            .to_string(),
        ] {
            let unsupported = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages/count_tokens",
                    "native-count-token-canary",
                    &synthetic,
                    &[(UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token())],
                ),
            );
            assert_eq!(status(&unsupported), 404, "{unsupported}");
        }
        assert_eq!(
            claude.requests().len(),
            1,
            "only the native Claude count request may reach Anthropic"
        );
    }

    #[test]
    fn unified_provider_switches_reseed_codex_without_private_output_items() {
        let codex =
            FakeResponses::start_with_responses(vec![Some(recovery_success_response()), None]);
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let config = BridgeConfig::default()
            .with_test_upstream_url(Some(codex.base_url.clone()))
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-route-key".to_string()), true)
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone()),
            );
        let bridge = BridgeHandle::start(config).unwrap();
        let session = "provider-switch-session";
        let turn = |model: &str, messages: serde_json::Value| {
            let body = serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
            })
            .to_string();
            request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    "native-route-credential",
                    &body,
                    &[
                        (UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token()),
                        ("X-Claude-Code-Session-Id", session),
                        ("Anthropic-Version", "2023-06-01"),
                    ],
                ),
            )
        };

        let first_claude = turn(
            "claude-opus-4-1",
            serde_json::json!([{"role": "user", "content": "first claude prompt"}]),
        );
        assert_eq!(status(&first_claude), 200, "{first_claude}");
        let first_codex = turn(
            "clud-claude-codex-sol",
            serde_json::json!([
                {"role": "user", "content": "first claude prompt"},
                {"role": "assistant", "content": "first claude visible answer"},
                {"role": "user", "content": "first codex prompt"}
            ]),
        );
        assert_eq!(status(&first_codex), 200, "{first_codex}");
        let switched_deepseek = turn(
            "clud-claude-deepseek-flash",
            serde_json::json!([
                {"role": "user", "content": "first claude prompt"},
                {"role": "assistant", "content": "first claude visible answer"},
                {"role": "user", "content": "first codex prompt"},
                {"role": "assistant", "content": "recovered"},
                {"role": "user", "content": "deepseek prompt"}
            ]),
        );
        assert_eq!(status(&switched_deepseek), 200, "{switched_deepseek}");
        let switched_claude = turn(
            "claude-opus-4-1",
            serde_json::json!([
                {"role": "user", "content": "first claude prompt"},
                {"role": "assistant", "content": "first claude visible answer"},
                {"role": "user", "content": "first codex prompt"},
                {"role": "assistant", "content": "recovered"},
                {"role": "user", "content": "deepseek prompt"},
                {"role": "assistant", "content": "deepseek visible answer"},
                {"role": "user", "content": "second claude prompt"}
            ]),
        );
        assert_eq!(status(&switched_claude), 200, "{switched_claude}");
        let final_codex = turn(
            "clud-claude-codex-terra",
            serde_json::json!([
                {"role": "user", "content": "first claude prompt"},
                {"role": "assistant", "content": "first claude visible answer"},
                {"role": "user", "content": "first codex prompt"},
                {"role": "assistant", "content": "recovered"},
                {"role": "user", "content": "deepseek prompt"},
                {"role": "assistant", "content": "deepseek visible answer"},
                {"role": "user", "content": "second claude prompt"},
                {"role": "assistant", "content": "second claude visible answer"},
                {"role": "user", "content": "final codex prompt"}
            ]),
        );
        assert_eq!(status(&final_codex), 200, "{final_codex}");

        let requests = codex.requests();
        assert_eq!(requests.len(), 2);
        let final_request = &requests[1];
        assert!(final_request.contains("first claude prompt"));
        assert!(final_request.contains("deepseek visible answer"));
        assert!(final_request.contains("second claude visible answer"));
        assert!(final_request.contains("final codex prompt"));
        assert!(
            !final_request.contains("msg_recovered"),
            "provider-private Codex output IDs crossed a route epoch: {final_request}"
        );
    }

    /// The epoch cycle with every route: Claude -> Codex -> DeepSeek -> Kimi
    /// -> Claude -> Codex. Kimi is its own epoch, so Codex's private output
    /// items still cannot survive the trip.
    #[test]
    fn unified_route_epoch_cycle_includes_kimi() {
        let codex =
            FakeResponses::start_with_responses(vec![Some(recovery_success_response()), None]);
        let claude = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let kimi = FakeResponses::start();
        let config = BridgeConfig::default()
            .with_test_upstream_url(Some(codex.base_url.clone()))
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("deepseek-route-key".to_string()), true)
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                    .with_route(ModelProvider::Kimi, Some("kimi-route-key".to_string()))
                    .with_route_upstream(ModelProvider::Kimi, kimi.base_url.clone()),
            );
        let bridge = BridgeHandle::start(config).unwrap();
        let mut history = Vec::new();
        let mut turn = |model: &str, prompt: &str, answer: Option<&str>| {
            history.push(serde_json::json!({"role": "user", "content": prompt}));
            let body = serde_json::json!({
                "model": model,
                "messages": history,
                "stream": false,
            })
            .to_string();
            let response = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    "native-route-credential",
                    &body,
                    &[
                        (UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token()),
                        ("X-Claude-Code-Session-Id", "kimi-epoch-session"),
                        ("Anthropic-Version", "2023-06-01"),
                    ],
                ),
            );
            assert_eq!(status(&response), 200, "{model}: {response}");
            if let Some(answer) = answer {
                history.push(serde_json::json!({"role": "assistant", "content": answer}));
            }
        };
        turn("claude-opus-4-1", "claude prompt", Some("claude answer"));
        turn("clud-claude-codex-sol", "codex prompt", Some("recovered"));
        turn(
            "clud-claude-deepseek-flash",
            "deepseek prompt",
            Some("deepseek answer"),
        );
        turn("clud-claude-kimi-k3", "kimi prompt", Some("kimi answer"));
        turn(
            "claude-opus-4-1",
            "second claude prompt",
            Some("second claude answer"),
        );
        turn("clud-claude-codex-terra", "final codex prompt", None);

        assert_eq!(kimi.requests().len(), 1);
        assert!(kimi.requests()[0].contains("Bearer kimi-route-key"));
        let requests = codex.requests();
        assert_eq!(requests.len(), 2);
        let final_request = &requests[1];
        assert!(final_request.contains("kimi answer"));
        assert!(final_request.contains("final codex prompt"));
        assert!(
            !final_request.contains("msg_recovered"),
            "provider-private Codex output IDs crossed a route epoch: {final_request}"
        );
        for raw in codex.requests().iter().chain(deepseek.requests().iter()) {
            assert!(!raw.contains("kimi-route-key"), "{raw}");
        }
    }

    #[test]
    fn unified_routes_come_from_the_registry_and_drop_on_a_missing_key() {
        let config = UnifiedGatewayConfig::new(Some("d".to_string()), false)
            .with_openrouter(Some("o".to_string()))
            .with_route(ModelProvider::Kimi, Some("k".to_string()));
        for provider in [
            ModelProvider::DeepSeek,
            ModelProvider::Kimi,
            ModelProvider::OpenRouter,
        ] {
            let route = config.route_for(provider).unwrap();
            assert_eq!(
                route.base_url,
                crate::provider_registry::descriptor_for(provider)
                    .unwrap()
                    .anthropic_base_url
            );
            assert!(config.provider_available(provider));
        }
        // Claude and Codex have no descriptor, so no direct route.
        let config = config
            .with_route(ModelProvider::Claude, Some("c".to_string()))
            .with_route(ModelProvider::Kimi, None);
        assert!(config.route_for(ModelProvider::Claude).is_none());
        assert!(!config.provider_available(ModelProvider::Claude));
        assert!(!config.provider_available(ModelProvider::Kimi));
        assert!(!config.provider_available(ModelProvider::Codex));
        assert!(!format!("{config:?}").contains("\"d\""));
    }

    #[test]
    fn unified_native_claude_preserves_effort_payload_and_required_headers_byte_for_byte() {
        let anthropic = FakeResponses::start();
        let codex = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let bridge = BridgeHandle::start(unified_config(&anthropic, &codex, &deepseek)).unwrap();
        let body = r#"{ "model":"claude-opus-4-8", "messages":[{"role":"user","content":"fixture prompt"}], "thinking":{"type":"adaptive","fixture":{"keep":true}}, "output_config":{"effort":"xhigh","format":{"type":"json_schema"}}, "stream":false }"#;

        let response = unified_request(&bridge, body, "native-byte-session", None);
        assert_eq!(status(&response), 200, "{response}");
        let requests = anthropic.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(captured_body(&requests[0]), body);
        let sent = requests[0].to_ascii_lowercase();
        for required in [
            "authorization: bearer claude-oauth-canary",
            "x-api-key: claude-api-key-canary",
            "anthropic-version: 2023-06-01",
            "anthropic-beta: oauth-2025-04-20,fixture-capability",
        ] {
            assert!(sent.contains(required), "missing {required}: {sent}");
        }
        for forbidden in [bridge.bearer_token(), "deepseek-route-canary"] {
            assert!(!requests[0].contains(forbidden), "leaked {forbidden}");
        }
        assert!(codex.requests().is_empty());
        assert!(deepseek.requests().is_empty());
    }

    #[test]
    fn unified_codex_models_and_efforts_reach_the_exact_responses_fields() {
        let anthropic = FakeResponses::start();
        let codex = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let bridge = BridgeHandle::start(unified_config(&anthropic, &codex, &deepseek)).unwrap();
        let models =
            provider_catalog::models_for_provider(ModelProvider::Codex).collect::<Vec<_>>();
        let mut expected = Vec::new();
        for (model_index, model) in models.iter().enumerate() {
            let discovery = model.discovery_id.expect("Codex discovery id");
            for effort in model.supported_efforts {
                let body = serde_json::json!({
                    "model": discovery,
                    "messages": [{"role": "user", "content": "route matrix"}],
                    "thinking": {"type": "adaptive"},
                    "output_config": {"effort": effort.as_str()},
                    "stream": false,
                })
                .to_string();
                let session = format!("codex-{model_index}");
                let response = unified_request(&bridge, &body, &session, None);
                assert_eq!(status(&response), 200, "{discovery} {effort}: {response}");
                expected.push((model.wire_id, effort.as_str()));
            }
        }

        let suffix = r#"{"model":"clud-claude-codex-terra@max","messages":[{"role":"user","content":"suffix wins"}],"thinking":{"type":"enabled","budget_tokens":9000},"output_config":{"effort":"low"},"stream":false}"#;
        assert_eq!(
            status(&unified_request(&bridge, suffix, "codex-suffix", None)),
            200
        );
        expected.push(("gpt-5.6-terra", "max"));

        let thinking = r#"{"model":"clud-claude-codex-luna","messages":[{"role":"user","content":"thinking fallback"}],"thinking":{"type":"enabled","budget_tokens":9000},"stream":false}"#;
        assert_eq!(
            status(&unified_request(&bridge, thinking, "codex-thinking", None)),
            200
        );
        expected.push(("gpt-6-luna", "high"));

        let catalog_default = r#"{"model":"clud-claude-codex-sol","messages":[{"role":"user","content":"default fallback"}],"thinking":{"type":"adaptive"},"stream":false}"#;
        assert_eq!(
            status(&unified_request(
                &bridge,
                catalog_default,
                "codex-default",
                None
            )),
            200
        );
        expected.push(("gpt-6-sol", "low"));

        let requests = codex.requests();
        assert_eq!(requests.len(), expected.len());
        for (request, (model, effort)) in requests.iter().zip(expected) {
            let body: serde_json::Value =
                serde_json::from_str(captured_body(request)).expect("Responses JSON");
            assert_eq!(body["model"], model);
            assert_eq!(body["reasoning"]["effort"], effort);
            assert!(!request.contains("claude-oauth-canary"));
            assert!(!request.contains("claude-api-key-canary"));
            assert!(!request.contains("deepseek-route-canary"));
        }

        let before_rejection = codex.requests().len();
        for rejected in ["minimal", "ultra"] {
            let body = serde_json::json!({
                "model": "clud-claude-codex-terra",
                "messages": [{"role": "user", "content": "reject locally"}],
                "output_config": {"effort": rejected},
                "stream": false,
            })
            .to_string();
            let response =
                unified_request(&bridge, &body, &format!("codex-reject-{rejected}"), None);
            assert_eq!(status(&response), 400, "{response}");
            assert!(response.contains(rejected), "{response}");
            assert!(response.contains("xhigh"), "{response}");
        }
        assert_eq!(
            codex.requests().len(),
            before_rejection,
            "a rejected effort must make zero upstream requests"
        );
        assert!(anthropic.requests().is_empty());
        assert!(deepseek.requests().is_empty());
    }

    #[test]
    fn unified_deepseek_preserves_effort_for_both_models_without_codex_validation() {
        fn provider_effective_effort(incoming: &str) -> Option<&'static str> {
            match incoming {
                "low" | "medium" | "high" => Some("high"),
                "xhigh" | "max" => Some("max"),
                _ => None,
            }
        }

        let anthropic = FakeResponses::start();
        let codex = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let bridge = BridgeHandle::start(unified_config(&anthropic, &codex, &deepseek)).unwrap();
        let models = [
            ("clud-claude-deepseek-v4-pro-0813", "deepseek-v4-pro[1m]"),
            ("clud-claude-deepseek-flash", "deepseek-flash[1m]"),
        ];
        // DeepSeek, not clud, owns this compatibility mapping. Assert both the
        // unchanged gateway payload and the provider-documented effective
        // level so this acceptance matrix cannot silently drift.
        let efforts = [
            ("low", "high"),
            ("medium", "high"),
            ("high", "high"),
            ("xhigh", "max"),
            ("max", "max"),
        ];
        for (model_index, (discovery, wire)) in models.iter().enumerate() {
            for (effort_index, (incoming, effective)) in efforts.iter().enumerate() {
                let body = serde_json::json!({
                    "model": discovery,
                    "messages": [{"role": "user", "content": "deepseek matrix"}],
                    "thinking": {"type": "adaptive", "fixture": {"keep": true}},
                    "output_config": {"effort": incoming, "fixture": {"keep": true}},
                    "stream": false,
                })
                .to_string();
                let response = unified_request(
                    &bridge,
                    &body,
                    &format!("deepseek-{model_index}-{effort_index}"),
                    None,
                );
                assert_eq!(status(&response), 200, "{discovery} {incoming}: {response}");
                let requests = deepseek.requests();
                let sent = requests.last().expect("DeepSeek request");
                let sent_body: serde_json::Value =
                    serde_json::from_str(captured_body(sent)).expect("Anthropic JSON");
                assert_eq!(sent_body["model"], *wire);
                assert_eq!(
                    sent_body["thinking"],
                    serde_json::json!({
                        "type": "adaptive",
                        "fixture": {"keep": true},
                    })
                );
                assert_eq!(
                    sent_body["output_config"],
                    serde_json::json!({
                        "effort": incoming,
                        "fixture": {"keep": true},
                    })
                );
                assert_eq!(
                    provider_effective_effort(
                        sent_body["output_config"]["effort"]
                            .as_str()
                            .expect("DeepSeek effort string")
                    ),
                    Some(*effective),
                    "provider mapping for {incoming}"
                );
                assert!(sent
                    .to_ascii_lowercase()
                    .contains("authorization: bearer deepseek-route-canary"));
                assert!(!sent.contains("claude-oauth-canary"));
                assert!(!sent.contains("claude-api-key-canary"));
                assert!(!sent.contains(bridge.bearer_token()));
            }
        }

        let future = r#"{"model":"clud-claude-deepseek-flash","messages":[{"role":"user","content":"future effort"}],"output_config":{"effort":"future-provider-level"},"stream":false}"#;
        assert_eq!(
            status(&unified_request(&bridge, future, "deepseek-future", None)),
            200
        );
        let requests = deepseek.requests();
        let body: serde_json::Value =
            serde_json::from_str(captured_body(requests.last().unwrap())).expect("Anthropic JSON");
        assert_eq!(body["output_config"]["effort"], "future-provider-level");
        assert!(anthropic.requests().is_empty());
        assert!(codex.requests().is_empty());
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    fn unified_provider_switch_reseeds_codex_and_keeps_main_and_agent_efforts_independent() {
        let anthropic = FakeResponses::start();
        let codex = FakeResponses::start();
        let deepseek = FakeResponses::start();
        let bridge = BridgeHandle::start(unified_config(&anthropic, &codex, &deepseek)).unwrap();
        let session = "switch-session";
        let turn = |model: &str, messages: serde_json::Value, effort: &str| {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": effort},
                "stream": false,
            })
            .to_string()
        };

        let claude_first = turn(
            "claude-opus-4-8",
            serde_json::json!([{"role": "user", "content": "claude first"}]),
            "high",
        );
        assert_eq!(
            status(&unified_request(&bridge, &claude_first, session, None)),
            200
        );
        let codex_first = turn(
            "clud-claude-codex-terra",
            serde_json::json!([
                {"role": "user", "content": "claude first"},
                {"role": "assistant", "content": "claude reply"},
                {"role": "user", "content": "codex first"}
            ]),
            "high",
        );
        assert_eq!(
            status(&unified_request(&bridge, &codex_first, session, None)),
            200
        );
        let deepseek_turn = turn(
            "clud-claude-deepseek-v4-pro-0813",
            serde_json::json!([
                {"role": "user", "content": "claude first"},
                {"role": "assistant", "content": "claude reply"},
                {"role": "user", "content": "codex first"},
                {"role": "assistant", "content": "bridged reply"},
                {"role": "user", "content": "deepseek turn"}
            ]),
            "high",
        );
        assert_eq!(
            status(&unified_request(&bridge, &deepseek_turn, session, None)),
            200
        );
        let claude_return = turn(
            "claude-opus-4-8",
            serde_json::json!([
                {"role": "user", "content": "claude first"},
                {"role": "assistant", "content": "claude reply"},
                {"role": "user", "content": "codex first"},
                {"role": "assistant", "content": "bridged reply"},
                {"role": "user", "content": "deepseek turn"},
                {"role": "assistant", "content": "deepseek reply"},
                {"role": "user", "content": "claude return"}
            ]),
            "high",
        );
        assert_eq!(
            status(&unified_request(&bridge, &claude_return, session, None)),
            200
        );
        let codex_return = turn(
            "clud-claude-codex-luna",
            serde_json::json!([
                {"role": "user", "content": "claude first"},
                {"role": "assistant", "content": "claude reply"},
                {"role": "user", "content": "codex first"},
                {"role": "assistant", "content": "bridged reply"},
                {"role": "user", "content": "deepseek turn"},
                {"role": "assistant", "content": "deepseek reply"},
                {"role": "user", "content": "claude return"},
                {"role": "assistant", "content": "claude return reply"},
                {"role": "user", "content": "codex return"}
            ]),
            "high",
        );
        assert_eq!(
            status(&unified_request(&bridge, &codex_return, session, None)),
            200
        );

        let agent = turn(
            "clud-claude-deepseek-flash",
            serde_json::json!([{"role": "user", "content": "agent override"}]),
            "xhigh",
        );
        assert_eq!(
            status(&unified_request(
                &bridge,
                &agent,
                session,
                Some("child-agent")
            )),
            200
        );

        let codex_requests = codex.requests();
        let reseeded: serde_json::Value = serde_json::from_str(captured_body(
            codex_requests.last().expect("returning Codex request"),
        ))
        .expect("Responses JSON");
        assert_eq!(reseeded["model"], "gpt-6-luna");
        assert_eq!(reseeded["reasoning"]["effort"], "high");
        let text = reseeded["input"].to_string();
        for visible in [
            "claude first",
            "claude reply",
            "codex first",
            "deepseek turn",
            "deepseek reply",
            "claude return",
            "claude return reply",
            "codex return",
        ] {
            assert!(text.contains(visible), "missing {visible}: {text}");
        }
        let deepseek_requests = deepseek.requests();
        let agent_body: serde_json::Value = serde_json::from_str(captured_body(
            deepseek_requests.last().expect("agent DeepSeek request"),
        ))
        .expect("Anthropic JSON");
        assert_eq!(agent_body["output_config"]["effort"], "xhigh");
        for request in anthropic.requests() {
            let body: serde_json::Value =
                serde_json::from_str(captured_body(&request)).expect("Anthropic JSON");
            assert_eq!(body["output_config"]["effort"], "high");
        }
    }

    #[test]
    fn transport_context_failure_compacts_and_retries_once() {
        let upstream = FakeResponses::start_with_responses(vec![
            None,
            Some(context_length_status_response()),
            Some(compact_success_response()),
            None,
        ]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let first = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );
        assert_eq!(status(&first), 200, "{first}");
        let continuation = r#"{"model":"claude-x","messages":[{"role":"user","content":"first"},{"role":"assistant","content":"done"},{"role":"user","content":"pending"}],"stream":false}"#;
        let recovered = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), continuation),
        );
        assert_eq!(status(&recovered), 200, "{recovered}");
        assert!(recovered.contains("bridged reply"), "{recovered}");
        let requests = upstream.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses/compact HTTP/1.1"))
                .count(),
            1,
            "{requests:#?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .count(),
            3,
            "the transport error must be one bounded recovery cycle: {requests:#?}"
        );
    }

    #[test]
    fn authenticated_lifecycle_hook_payloads_compact_and_clear_the_bridge_session() {
        let upstream = FakeResponses::start_with_responses(vec![
            None,
            Some(compact_success_response()),
            None,
            None,
        ]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();
        let first = request(
            addr,
            &authorized("POST", "/v1/messages", &token, PROBE_BODY),
        );
        assert_eq!(status(&first), 200, "{first}");

        let mismatched = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/compact",
                &token,
                r#"{"hook_event_name":"SessionStart","source":"clear"}"#,
            ),
        );
        assert_eq!(status(&mismatched), 400, "{mismatched}");

        let compact = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/compact",
                &token,
                r#"{"hook_event_name":"PreCompact","trigger":"manual"}"#,
            ),
        );
        assert_eq!(status(&compact), 204, "{compact}");
        let compact_requests = upstream.requests();
        assert_eq!(
            compact_requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses/compact HTTP/1.1"))
                .count(),
            1,
            "{compact_requests:#?}"
        );

        let wrong = request(
            addr,
            &authorized("POST", "/_clud/context/clear", "wrong", ""),
        );
        assert_eq!(status(&wrong), 401, "{wrong}");
        let clear = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/clear",
                &token,
                r#"{"hook_event_name":"SessionStart","source":"clear"}"#,
            ),
        );
        assert_eq!(status(&clear), 204, "{clear}");
        let fresh = request(
            addr,
            &authorized(
                "POST",
                "/v1/messages",
                &token,
                r#"{"model":"claude-x","messages":[{"role":"user","content":"fresh"}],"stream":false}"#,
            ),
        );
        assert_eq!(status(&fresh), 200, "{fresh}");
        let requests = upstream.requests();
        let post_clear = requests
            .iter()
            .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
            .nth(1)
            .expect("post-clear inference request");
        let post_clear_body = post_clear
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .expect("upstream request has an HTTP body");
        let fresh_input = regex::Regex::new(r#""text"\s*:\s*"fresh""#).unwrap();
        let stale_input = regex::Regex::new(r#""text"\s*:\s*"hi""#).unwrap();
        assert!(fresh_input.is_match(post_clear_body), "{post_clear}");
        assert!(
            !stale_input.is_match(post_clear_body),
            "stale pre-clear input leaked: {post_clear}"
        );
    }

    #[test]
    fn automatic_compaction_fallback_resets_rehydrated_tool_history() {
        let upstream = FakeResponses::start_with_responses(vec![
            Some(parallel_function_call_response(3)),
            None,
            None,
        ]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();
        let headers = [("X-Claude-Code-Session-Id", "session-auto-compact")];
        let first = request(
            addr,
            &authorized_with_headers("POST", "/v1/messages", &token, PROBE_STREAM_BODY, &headers),
        );
        assert_eq!(status(&first), 200, "{first}");

        let compact = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/compact",
                &token,
                r#"{"hook_event_name":"PreCompact","trigger":"auto","session_id":"session-auto-compact"}"#,
            ),
        );
        assert_eq!(status(&compact), 204, "{compact}");
        assert!(
            !upstream
                .requests()
                .iter()
                .any(|request| request.starts_with("POST /v1/responses/compact HTTP/1.1")),
            "an incomplete continuation must be discarded locally, not sent upstream"
        );

        let mut replay: serde_json::Value =
            serde_json::from_str(&split_parallel_result_body(3, true)).unwrap();
        replay["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"role": "user", "content": "compact this context"}));
        let replay = replay.to_string();
        let compaction_inference = request(
            addr,
            &authorized_with_headers("POST", "/v1/messages", &token, &replay, &headers),
        );
        assert_eq!(status(&compaction_inference), 200, "{compaction_inference}");

        let finished = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/compact-finished",
                &token,
                r#"{"hook_event_name":"SessionStart","source":"compact","session_id":"session-auto-compact"}"#,
            ),
        );
        assert_eq!(status(&finished), 204, "{finished}");

        let compacted_turn = request(
            addr,
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                &token,
                r#"{"model":"claude-x","messages":[{"role":"user","content":"COMPACTED SUMMARY"},{"role":"user","content":"after compact"}],"stream":false}"#,
                &headers,
            ),
        );
        assert_eq!(status(&compacted_turn), 200, "{compacted_turn}");
        let requests = upstream.requests();
        let compaction_request = requests
            .iter()
            .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
            .nth(1)
            .expect("Claude compaction inference");
        assert!(
            compaction_request.contains("call_0"),
            "{compaction_request}"
        );
        assert!(
            compaction_request.contains("result-0"),
            "{compaction_request}"
        );
        let fresh_request = requests
            .iter()
            .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
            .nth(2)
            .expect("post-compaction inference");
        assert!(
            fresh_request.contains("COMPACTED SUMMARY"),
            "{fresh_request}"
        );
        assert!(
            !fresh_request.contains("call_0"),
            "the post-compaction reset must not retain the old tool replay: {fresh_request}"
        );
    }

    #[test]
    fn automatic_compaction_fallback_handles_unavailable_endpoint() {
        let upstream =
            FakeResponses::start_with_responses(vec![None, Some(not_found_response()), None, None]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();
        let headers = [("X-Claude-Code-Session-Id", "session-compact-404")];
        let first = request(
            addr,
            &authorized_with_headers("POST", "/v1/messages", &token, PROBE_BODY, &headers),
        );
        assert_eq!(status(&first), 200, "{first}");

        let compact = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/compact",
                &token,
                r#"{"hook_event_name":"PreCompact","trigger":"auto","session_id":"session-compact-404"}"#,
            ),
        );
        assert_eq!(status(&compact), 204, "{compact}");

        let compaction_inference = request(
            addr,
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                &token,
                r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"compact this context"}],"stream":false}"#,
                &headers,
            ),
        );
        assert_eq!(status(&compaction_inference), 200, "{compaction_inference}");
        let finished = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/compact-finished",
                &token,
                r#"{"hook_event_name":"SessionStart","source":"compact","session_id":"session-compact-404"}"#,
            ),
        );
        assert_eq!(status(&finished), 204, "{finished}");
        let after = request(
            addr,
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                &token,
                r#"{"model":"claude-x","messages":[{"role":"user","content":"SUMMARY AFTER 404"}],"stream":false}"#,
                &headers,
            ),
        );
        assert_eq!(status(&after), 200, "{after}");
        let requests = upstream.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses/compact HTTP/1.1"))
                .count(),
            1
        );
        let final_request = requests.last().expect("post-compaction inference");
        assert!(
            final_request.contains("SUMMARY AFTER 404"),
            "{final_request}"
        );
        assert!(!final_request.contains("bridged reply"), "{final_request}");
    }

    #[test]
    fn automatic_compaction_fallback_resets_an_initially_empty_history() {
        let upstream = FakeResponses::start_with_responses(vec![None, None]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();
        let headers = [("X-Claude-Code-Session-Id", "session-empty-compact")];

        let compact = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/compact",
                &token,
                r#"{"hook_event_name":"PreCompact","trigger":"auto","session_id":"session-empty-compact"}"#,
            ),
        );
        assert_eq!(status(&compact), 204, "{compact}");
        let compaction_inference = request(
            addr,
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                &token,
                r#"{"model":"claude-x","messages":[{"role":"user","content":"FULL PRECOMPACTION HISTORY"},{"role":"user","content":"compact this context"}],"stream":false}"#,
                &headers,
            ),
        );
        assert_eq!(status(&compaction_inference), 200, "{compaction_inference}");
        let finished = request(
            addr,
            &authorized(
                "POST",
                "/_clud/context/compact-finished",
                &token,
                r#"{"hook_event_name":"SessionStart","source":"compact","session_id":"session-empty-compact"}"#,
            ),
        );
        assert_eq!(status(&finished), 204, "{finished}");
        let after = request(
            addr,
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                &token,
                r#"{"model":"claude-x","messages":[{"role":"user","content":"SUMMARY FROM EMPTY"}],"stream":false}"#,
                &headers,
            ),
        );
        assert_eq!(status(&after), 200, "{after}");
        let final_request = upstream
            .requests()
            .pop()
            .expect("post-compaction inference");
        assert!(
            final_request.contains("SUMMARY FROM EMPTY"),
            "{final_request}"
        );
        assert!(
            !final_request.contains("FULL PRECOMPACTION HISTORY"),
            "{final_request}"
        );
    }

    #[test]
    /// Admission/concurrency behaviour moved to its own tests when queueing
    /// replaced immediate rejection; this one owns the size and time bounds.
    fn enforces_body_header_and_timeout_bounds() {
        let config = BridgeConfig {
            max_body_bytes: 64,
            max_header_bytes: 256,
            header_timeout: Duration::from_millis(100),
            max_concurrency: 1,
            ..BridgeConfig::default()
        };
        let bridge = BridgeHandle::start(config).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();

        let oversized_body = request(
            addr,
            &format!(
                "POST /v1/messages HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 65\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(status(&oversized_body), 413);

        let large_header = "x".repeat(300);
        let oversized_header = request(
            addr,
            &format!("GET / HTTP/1.1\r\nX-Large: {large_header}\r\n\r\n"),
        );
        assert_eq!(status(&oversized_header), 431);

        let mut slow = TcpStream::connect(addr).unwrap();
        slow.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        slow.write_all(b"GET / HTTP/1.1\r\nHost: local").unwrap();
        thread::sleep(Duration::from_millis(175));
        let mut timeout_response = String::new();
        slow.read_to_string(&mut timeout_response).unwrap();
        assert_eq!(status(&timeout_response), 408);

        let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
        let bridge = BridgeHandle::start(
            BridgeConfig {
                header_timeout: Duration::from_millis(150),
                ..BridgeConfig::default()
            }
            .with_admission_notifier(admitted_tx),
        )
        .unwrap();
        let mut dripping = TcpStream::connect(bridge.socket_addr()).unwrap();
        dripping
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        dripping.write_all(b"G").unwrap();
        admitted_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("slow-drip request worker admission signal");
        let mut drip_writer = dripping.try_clone().unwrap();
        let (drip_started_tx, drip_started_rx) = std::sync::mpsc::sync_channel(1);
        let writer = thread::spawn(move || {
            for (index, byte) in b"ET / HTTP/1.1\r\nHost: slow-drip"
                .iter()
                .cycle()
                .take(60)
                .enumerate()
            {
                if drip_writer.write_all(&[*byte]).is_err() {
                    break;
                }
                if index == 0 {
                    let _ = drip_started_tx.send(());
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        drip_started_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("slow-drip writer start signal");
        let started = Instant::now();
        let mut absolute_timeout_response = String::new();
        let read_result = dripping.read_to_string(&mut absolute_timeout_response);
        if let Err(error) = read_result {
            assert!(matches!(
                error.kind(),
                io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
            ));
        }
        if !absolute_timeout_response.is_empty() {
            assert_eq!(status(&absolute_timeout_response), 408);
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(dripping);
        writer.join().unwrap();
    }

    #[test]
    fn shutdown_and_drop_are_idempotent_and_close_the_listener() {
        let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
        let mut bridge = BridgeHandle::start(
            BridgeConfig {
                header_timeout: Duration::from_secs(10),
                // Pinned, not defaulted: since #989 the default admits two
                // workers, and this test needs a saturated listener so the
                // second socket is still in the backlog at shutdown. It owns
                // teardown and shutdown idempotency, not the default bound.
                max_concurrency: 1,
                ..BridgeConfig::default()
            }
            .with_admission_notifier(admitted_tx),
        )
        .unwrap();
        let addr = bridge.socket_addr();
        let mut slow_drip = TcpStream::connect(addr).unwrap();
        slow_drip
            .write_all(b"GET / HTTP/1.1\r\nHost: local")
            .unwrap();
        admitted_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("request worker admission signal");
        assert_eq!(bridge.active_requests(), 1);
        // This connection remains in the kernel backlog: the sole worker (see
        // the pinned bound above) is blocked on the slow header and must not
        // create a second worker.
        let mut queued = TcpStream::connect(addr).unwrap();
        queued
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        queued
            .write_all(b"GET / HTTP/1.1\r\nHost: queued\r\n\r\n")
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(bridge.active_requests(), 1);
        let shutdown_started = Instant::now();
        bridge.shutdown().unwrap();
        assert!(shutdown_started.elapsed() < Duration::from_secs(5));
        let mut byte = [0_u8; 1];
        let queued_result = queued.read(&mut byte);
        assert!(
            matches!(queued_result, Ok(0))
                || queued_result.as_ref().is_err_and(|error| {
                    matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::NotConnected
                    )
                }),
            "listener shutdown must tear down a queued socket: {queued_result:?}"
        );
        bridge.shutdown().unwrap();

        // Probing a released ephemeral port is racy: a test running in
        // parallel can bind it, so the connect succeeds (#1929). Prove drop
        // closes the listener through a socket it had already accepted.
        let mut held = {
            let bridge = BridgeHandle::start(BridgeConfig::default()).unwrap();
            TcpStream::connect(bridge.socket_addr()).unwrap()
        };
        // macOS refuses setsockopt with EINVAL once the peer has already
        // closed the socket; the read below still observes the close.
        let _ = held.set_read_timeout(Some(Duration::from_secs(5)));
        let held_result = held.read(&mut byte);
        assert!(
            matches!(held_result, Ok(0))
                || held_result.as_ref().is_err_and(|error| {
                    matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::NotConnected
                    )
                }),
            "dropping the bridge must close its sockets: {held_result:?}"
        );
    }

    #[test]
    fn diagnostics_redact_token_base_url_and_test_upstream() {
        let config = BridgeConfig::default().with_test_upstream_url(Some(
            "http://127.0.0.1:45678/private-test-upstream".to_string(),
        ));
        let bridge = BridgeHandle::start(config.clone()).unwrap();
        let rendered_config = format!("{config:?}");
        let rendered_handle = format!("{bridge:?}");
        for secret in [
            bridge.bearer_token(),
            bridge.base_url(),
            "http://127.0.0.1:45678/private-test-upstream",
        ] {
            assert!(!rendered_config.contains(secret));
            assert!(!rendered_handle.contains(secret));
        }
    }

    #[test]
    fn test_upstream_override_requires_debug_and_integration_gate() {
        let value = Some("http://127.0.0.1:45678".to_string());
        assert_eq!(
            resolve_test_upstream_override(true, true, value.clone()),
            value
        );
        assert_eq!(
            resolve_test_upstream_override(false, true, value.clone()),
            None
        );
        assert_eq!(resolve_test_upstream_override(true, false, value), None);
    }

    #[test]
    fn unified_integration_upstreams_are_all_or_nothing_and_explicitly_gated() {
        let urls = || {
            (
                Some("http://127.0.0.1:4100".to_string()),
                Some("http://127.0.0.1:4101".to_string()),
                Some("http://127.0.0.1:4102".to_string()),
                Some("http://127.0.0.1:4103".to_string()),
            )
        };
        let (codex, anthropic, deepseek, openrouter) = urls();
        assert!(resolve_unified_integration_upstreams(
            true,
            codex.clone(),
            anthropic.clone(),
            deepseek.clone(),
            openrouter.clone(),
        )
        .is_some());
        assert!(resolve_unified_integration_upstreams(
            false, codex, anthropic, deepseek, openrouter
        )
        .is_none());
        let (codex, anthropic, deepseek, _) = urls();
        assert!(
            resolve_unified_integration_upstreams(true, codex, anthropic, deepseek, None).is_none()
        );
        let (_, anthropic, deepseek, openrouter) = urls();
        assert!(
            resolve_unified_integration_upstreams(true, None, anthropic, deepseek, openrouter,)
                .is_none()
        );
        let (codex, anthropic, _deepseek, openrouter) = urls();
        assert!(resolve_unified_integration_upstreams(
            true,
            codex,
            anthropic,
            Some("  ".to_string()),
            openrouter,
        )
        .is_none());
    }

    /// Split a chunked response into its decoded body. Deliberately strict:
    /// a malformed chunk header panics rather than silently returning a short
    /// body, so a framing regression fails loudly.
    fn decode_chunked_body(response: &str) -> String {
        let (headers, mut rest) = response.split_once("\r\n\r\n").expect("header terminator");
        assert!(
            headers.contains("Transfer-Encoding: chunked"),
            "streamed response must be chunked, got headers: {headers}"
        );
        assert!(
            !headers.contains("Content-Length"),
            "a streamed response cannot carry Content-Length: {headers}"
        );
        let mut body = String::new();
        loop {
            let (size_line, remainder) = rest.split_once("\r\n").expect("chunk size line");
            let size = usize::from_str_radix(size_line.trim(), 16).expect("hex chunk size");
            if size == 0 {
                break;
            }
            let (chunk, remainder) = remainder.split_at(size);
            body.push_str(chunk);
            rest = remainder
                .strip_prefix("\r\n")
                .expect("chunk data must be CRLF terminated");
        }
        body
    }

    #[test]
    fn streamed_responses_are_chunked_and_flushed_frame_by_frame() {
        let frame_hold = Duration::from_millis(120);
        let upstream = FakeResponses::start();
        let bridge =
            BridgeHandle::start(bridged_config(&upstream).with_frame_hold(frame_hold)).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();

        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(authorized("POST", "/v1/messages", &token, PROBE_STREAM_BODY).as_bytes())
            .unwrap();

        // Read incrementally and timestamp the first frame against completion.
        // A single buffered write would make these two instants identical.
        let started = Instant::now();
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 512];
        let mut first_frame_at = None;
        loop {
            let count = stream.read(&mut chunk).unwrap();
            if count == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..count]);
            if first_frame_at.is_none()
                && String::from_utf8_lossy(&raw).contains("\"type\":\"message_start\"")
            {
                first_frame_at = Some(started.elapsed());
            }
        }
        let completed_at = started.elapsed();
        let first_frame_at = first_frame_at.expect("message_start frame never arrived");

        let response = String::from_utf8(raw).expect("utf-8 response");
        assert_eq!(status(&response), 200);
        assert!(response.contains("text/event-stream"));

        // The whole point of the phase: usable output well before the end.
        assert!(
            completed_at >= first_frame_at + frame_hold * 2,
            "stream was not progressive: first frame at {first_frame_at:?}, done at {completed_at:?}"
        );

        let body = decode_chunked_body(&response);
        // Translated upstream content, not a canned fixture.
        assert!(body.contains(r#""text":"bridged "#));
        assert!(body.contains("event: content_block_stop"));
        assert!(body.starts_with("event: message_start\ndata:"));
        assert!(body.ends_with("\n\n"));
        assert!(body.contains("event: message_stop"));
    }

    /// An upstream that answers with `reply` (if any) and then holds the socket
    /// open without saying anything more, so a read from it can only end in a
    /// timeout rather than an EOF.
    ///
    /// #1263 needs exactly that shape: a *quiet* peer, which is what a stalled
    /// provider looks like from the bridge. `FakeResponses` cannot express it --
    /// it always writes a complete reply and shuts the socket down, which reads
    /// as a clean end of stream. The sockets stay alive in the returned vector
    /// for as long as the caller holds it.
    fn quiet_upstream(reply: Option<&'static str>) -> (String, JoinHandle<Vec<TcpStream>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut held = Vec::new();
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let mut request = [0_u8; 8192];
                        let _ = stream.read(&mut request);
                        if let Some(reply) = reply {
                            let _ = stream.write_all(reply.as_bytes());
                            let _ = stream.flush();
                        }
                        held.push(stream);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            held
        });
        (url, handle)
    }

    /// #1263: a hung upstream is not a broken one. The proxy answered *every*
    /// transport failure with 502 `api_error`, which tells the harness "this
    /// route is dead" rather than "retry me" -- while the 504 `timeout_error`
    /// mapping already existed for every other timeout the bridge raises
    /// (`codex_pipeline`). Nothing has been written at this point, so the
    /// status is still ours to choose (DD-029).
    #[test]
    fn a_hung_upstream_answers_504_not_502() {
        let (url, upstream) = quiet_upstream(None);
        let config = BridgeConfig {
            stream_idle_timeout: Duration::from_millis(200),
            ..BridgeConfig::default()
        }
        .with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                .with_upstreams(url.clone(), url),
        );
        let mut bridge = BridgeHandle::start(config).unwrap();

        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "claude-opus-4-1",
                "hung-upstream",
                "native-claude-oauth-canary",
            ),
        );

        assert_eq!(status(&response), 504, "{response}");
        assert!(
            response.contains(r#""type":"timeout_error""#),
            "a timeout must read as retryable, not as a dead route: {response}"
        );
        assert!(
            !response.contains("gateway upstream unavailable"),
            "a quiet socket is not an unavailable gateway: {response}"
        );
        bridge.shutdown().unwrap();
        drop(upstream.join().unwrap());
    }

    /// #1263 / DD-029: once the first frame is committed the status is spent,
    /// so a mid-stream failure can only be reported in-band. This arm used to
    /// write the terminating chunk and close, which on the wire is
    /// byte-identical to a stream that ended because the model finished -- a
    /// client still waiting on its `message_stop` saw a truncated turn with no
    /// reason attached, and nothing anywhere said the upstream had gone quiet.
    #[test]
    fn a_stalled_upstream_stream_reports_an_in_band_error() {
        let (url, upstream) = quiet_upstream(Some(concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: text/event-stream\r\n",
            "Content-Length: 1000000\r\n",
            "Connection: close\r\n\r\n",
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        )));
        let config = BridgeConfig {
            stream_idle_timeout: Duration::from_millis(200),
            ..BridgeConfig::default()
        }
        .with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                .with_upstreams(url.clone(), url),
        );
        let mut bridge = BridgeHandle::start(config).unwrap();

        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "claude-opus-4-1",
                "stalled-stream",
                "native-claude-oauth-canary",
            ),
        );

        assert_eq!(status(&response), 200, "{response}");
        assert!(
            response.contains("event: error"),
            "a stalled stream must not look like a clean end: {response}"
        );
        assert!(
            response.contains(r#""type":"timeout_error""#),
            "the in-band frame must say the request stalled: {response}"
        );
        assert!(response.contains("model request stalled"), "{response}");
        bridge.shutdown().unwrap();
        drop(upstream.join().unwrap());
    }

    /// #1263: the hop's budget is *idle*, not total -- which is what
    /// `DEFAULT_STREAM_IDLE_TIMEOUT`'s doc comment always claimed and what the
    /// code did not do. A stream that keeps producing frames is healthy however
    /// long it runs (a model that thinks for minutes emits deltas the whole
    /// time), so a long turn must not be cut off for the crime of outlasting
    /// the timeout. Under `Request::timeout()` -- a whole-request deadline that
    /// *overrides* `timeout_read()` -- this exact upstream was truncated
    /// mid-turn, and the truncation was invisible.
    ///
    /// The margin is deliberately lopsided: the turn outlasts the budget by a
    /// factor of two, while every *gap* sits 30x inside it. A test of "idle,
    /// not total" fails when a gap exceeds the budget, so the gap is what needs
    /// the headroom on a shared runner -- see the loop below.
    #[test]
    fn a_long_but_continuously_streaming_turn_is_not_cut_off() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let upstream = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut held = Vec::new();
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        // Nagle off. The drip below is a long run of small
                        // writes, which is exactly the shape Nagle coalesces:
                        // with an unacknowledged segment in flight it holds the
                        // next one until the ACK lands, and against a delayed
                        // ACK that is a ~200ms stall on macOS. The bridge is
                        // reading this with an idle budget, so the kernel's
                        // batching would be indistinguishable from the upstream
                        // going quiet -- the test would fail on the runner's
                        // TCP settings rather than on the behaviour under test.
                        stream.set_nodelay(true).unwrap();
                        let mut request = [0_u8; 8192];
                        let _ = stream.read(&mut request);
                        let frames = (0..60)
                            .map(|index| {
                                format!(
                                    "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"tick{index}\"}}}}\n\n"
                                )
                            })
                            .collect::<Vec<_>>();
                        let length: usize = frames.iter().map(String::len).sum();
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.flush();
                        // One frame every 100ms against an *absolute* schedule,
                        // so the gaps cannot drift: each gap sits 30x inside the
                        // 3s idle budget while the turn as a whole runs twice as
                        // long as it. The first shape of this test paired 80ms
                        // frames with a 200ms budget -- only 120ms of slack --
                        // and failed on a loaded macOS runner, where the bridge
                        // logged "upstream stream stalled after 120 bytes:
                        // timed out reading response" (#1264's CI). The property
                        // under test is "idle, not total", so the *gap* margin is
                        // the one that has to be wide; jitter can only make the
                        // total-duration side more true, never less.
                        let started = Instant::now();
                        for (index, frame) in frames.iter().enumerate() {
                            let target = started + Duration::from_millis(100 * (index as u64 + 1));
                            if let Some(wait) = target.checked_duration_since(Instant::now()) {
                                thread::sleep(wait);
                            }
                            let _ = stream.write_all(frame.as_bytes());
                            let _ = stream.flush();
                        }
                        held.push(stream);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            held
        });
        let config = BridgeConfig {
            stream_idle_timeout: Duration::from_secs(3),
            ..BridgeConfig::default()
        }
        .with_unified_gateway(
            UnifiedGatewayConfig::new(Some("deepseek-test-secret".to_string()), true)
                .with_upstreams(url.clone(), url),
        );
        let mut bridge = BridgeHandle::start(config).unwrap();

        let response = request(
            bridge.socket_addr(),
            &unified_message_request(
                &bridge,
                "claude-opus-4-1",
                "long-stream",
                "native-claude-oauth-canary",
            ),
        );

        assert_eq!(status(&response), 200, "{response}");
        assert!(
            response.contains("tick59"),
            "a continuously streaming turn must not be cut off at the idle budget: {response}"
        );
        assert!(
            !response.contains("event: error"),
            "a healthy long stream must not be reported as stalled: {response}"
        );
        bridge.shutdown().unwrap();
        drop(upstream.join().unwrap());
    }

    /// The SSE a compat upstream sends for one `Bash` tool call whose
    /// arguments arrive as several `input_json_delta` fragments (#1532). The
    /// command carries a multi-byte character so a byte-level split can land
    /// inside it.
    fn fragmented_tool_use_sse() -> (String, &'static str) {
        let full_input = r#"{"command":"echo héllo && ls -la","description":"list files"}"#;
        let fragments = [
            "",
            r#"{"comm"#,
            r#"and":"echo h"#,
            "é",
            r#"llo && ls -la","desc"#,
            r#"ription":"list files"}"#,
        ];
        let mut sse = String::from(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"m\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"Bash\",\"input\":{}}}\n\n",
        );
        for fragment in fragments {
            let data = serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": fragment},
            });
            sse.push_str(&format!("event: content_block_delta\ndata: {data}\n\n"));
        }
        sse.push_str(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        (sse, full_input)
    }

    /// An upstream that answers every request with `body`, written in
    /// `piece`-byte writes with a pause between them, so the bridge's reads
    /// land mid-frame, mid-JSON and mid-UTF-8.
    fn dribbling_sse_upstream(
        body: String,
        piece: usize,
        connections: usize,
    ) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            for _ in 0..connections {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_nodelay(true).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let mut request = [0_u8; 16384];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                for segment in body.as_bytes().chunks(piece) {
                    stream.write_all(segment).unwrap();
                    stream.flush().unwrap();
                    thread::sleep(Duration::from_millis(2));
                }
                let _ = stream.shutdown(Shutdown::Write);
            }
        });
        (url, handle)
    }

    /// Byte-level chunked decode: the relayed chunks may split a UTF-8
    /// sequence, so the response cannot be read as a `String` first.
    fn decode_chunked_bytes(raw: &[u8]) -> Vec<u8> {
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header terminator");
        let headers = String::from_utf8_lossy(&raw[..split]);
        assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
        assert!(headers.contains("Transfer-Encoding: chunked"), "{headers}");
        let mut rest = &raw[split + 4..];
        let mut body = Vec::new();
        loop {
            let line_end = rest
                .windows(2)
                .position(|window| window == b"\r\n")
                .expect("chunk size line");
            let size =
                usize::from_str_radix(std::str::from_utf8(&rest[..line_end]).unwrap().trim(), 16)
                    .expect("hex chunk size");
            rest = &rest[line_end + 2..];
            if size == 0 {
                return body;
            }
            body.extend_from_slice(&rest[..size]);
            assert_eq!(
                &rest[size..size + 2],
                b"\r\n",
                "chunk must be CRLF terminated"
            );
            rest = &rest[size + 2..];
        }
    }

    /// What a client rebuilds from the stream: the concatenated
    /// `partial_json` of every `input_json_delta` for block 0.
    fn reassembled_tool_input(sse: &str) -> String {
        sse.split("\n\n")
            .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .filter(|event| event["delta"]["type"] == "input_json_delta")
            .map(|event| event["delta"]["partial_json"].as_str().unwrap().to_string())
            .collect()
    }

    /// #1532: a streamed `tool_use` whose arguments arrive as fragmented
    /// `input_json_delta` frames -- delivered in 1-, 5- and 7-byte writes, so
    /// every frame, JSON string and multi-byte character straddles a read
    /// boundary -- reaches the client byte-for-byte on the OpenRouter,
    /// DeepSeek and Kimi compat routes. The relay forwards response bytes
    /// verbatim and never re-frames SSE, so an empty tool call seen on a
    /// compat route did not originate in the bridge.
    #[test]
    fn compat_routes_relay_fragmented_tool_use_input_intact() {
        let (sse, full_input) = fragmented_tool_use_sse();
        assert_eq!(reassembled_tool_input(&sse), full_input, "fixture sanity");
        for piece in [1_usize, 5, 7] {
            for route in ["openrouter", "deepseek", "kimi"] {
                let (url, upstream) = dribbling_sse_upstream(sse.clone(), piece, 1);
                let unused = FakeResponses::start();
                let (gateway, model) = match route {
                    "openrouter" => (
                        UnifiedGatewayConfig::new(None, false)
                            .with_openrouter(Some("openrouter-key".to_string()))
                            .with_upstreams(unused.base_url.clone(), String::new())
                            .with_openrouter_upstream(url),
                        "clud-claude-openrouter-sonnet",
                    ),
                    "deepseek" => (
                        UnifiedGatewayConfig::new(Some("deepseek-key".to_string()), false)
                            .with_upstreams(unused.base_url.clone(), url),
                        "clud-claude-deepseek-flash",
                    ),
                    _ => (
                        UnifiedGatewayConfig::new(Some("deepseek-key".to_string()), false)
                            .with_upstreams(unused.base_url.clone(), unused.base_url.clone())
                            .with_route_upstream(ModelProvider::Kimi, url)
                            .with_route(ModelProvider::Kimi, Some("kimi-key".to_string())),
                        "clud-claude-kimi-k3",
                    ),
                };
                let mut bridge =
                    BridgeHandle::start(BridgeConfig::default().with_unified_gateway(gateway))
                        .unwrap();
                let mut client = TcpStream::connect(bridge.socket_addr()).unwrap();
                client.set_read_timeout(Some(CLIENT_READ_TIMEOUT)).unwrap();
                client
                    .write_all(
                        unified_message_request(&bridge, model, "tool-session", "native-oauth")
                            .as_bytes(),
                    )
                    .unwrap();
                client.shutdown(Shutdown::Write).unwrap();
                let mut raw = Vec::new();
                client.read_to_end(&mut raw).unwrap();
                let body = decode_chunked_bytes(&raw);
                assert_eq!(
                    body,
                    sse.as_bytes(),
                    "{route} ({piece}-byte upstream writes): relayed SSE must be byte-identical"
                );
                let relayed = String::from_utf8(body).unwrap();
                assert_eq!(
                    reassembled_tool_input(&relayed),
                    full_input,
                    "{route} ({piece}-byte upstream writes): tool_use input lost"
                );
                assert!(
                    unused.requests().is_empty(),
                    "{route}: the turn must stay on its own route"
                );
                bridge.shutdown().unwrap();
                upstream.join().unwrap();
            }
        }
    }

    #[test]
    fn first_frame_timeout_returns_a_pre_commit_gateway_error() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let upstream_url = format!("http://{}", listener.local_addr().unwrap());
        let upstream = thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                thread::sleep(Duration::from_millis(80));
            }
        });
        let bridge = BridgeHandle::start(
            BridgeConfig {
                first_frame_timeout: Duration::from_millis(20),
                ..BridgeConfig::default()
            }
            .with_test_upstream_url(Some(upstream_url)),
        )
        .unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );

        assert_eq!(status(&response), 504);
        assert!(!response.contains("text/event-stream"));
        upstream.join().unwrap();
    }

    #[test]
    fn header_body_and_stream_timeouts_are_independent() {
        // Header budget is short; body budget is long. A client that sends its
        // headers promptly and its body slowly must succeed -- under the single
        // whole-connection deadline this replaced, it returned 408.
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(BridgeConfig {
            header_timeout: Duration::from_millis(300),
            body_timeout: Duration::from_secs(5),
            ..bridged_config(&upstream)
        })
        .unwrap();
        let token = bridge.bearer_token().to_owned();
        let body = PROBE_BODY;

        let mut stream = TcpStream::connect(bridge.socket_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(
                format!(
                    "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        stream.flush().unwrap();

        // Longer than the header budget, comfortably inside the body budget.
        thread::sleep(Duration::from_millis(600));
        stream.write_all(body.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut raw = Vec::new();
        let mut chunk = [0_u8; 512];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => raw.extend_from_slice(&chunk[..count]),
                Err(error) => panic!("read failed after {} bytes: {error}", raw.len()),
            }
        }
        let response = String::from_utf8_lossy(&raw).into_owned();
        assert_eq!(
            status(&response),
            200,
            "slow body must not be charged the header budget: {response}"
        );
        assert!(response.contains("bridged reply"));
    }

    /// Regression guard: a body split across many segments must be reassembled.
    /// Accepted sockets inherit the listener's non-blocking mode on Windows,
    /// which made every read that outran the client's next segment look like a
    /// timeout and answered 408. Real Claude requests carrying a transcript or
    /// an image always span segments, so this is the shape that matters.
    #[test]
    fn bodies_split_across_segments_are_reassembled() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let token = bridge.bearer_token().to_owned();
        let filler = "x".repeat(4096);
        let body = format!(
            r#"{{"model":"claude-x","messages":[{{"role":"user","content":"hi"}}],"stream":false,"note":"{filler}"}}"#
        );

        let mut stream = TcpStream::connect(bridge.socket_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(
                format!(
                    "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        stream.flush().unwrap();

        for segment in body.as_bytes().chunks(512) {
            thread::sleep(Duration::from_millis(20));
            stream.write_all(segment).unwrap();
            stream.flush().unwrap();
        }

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert_eq!(
            status(&response),
            200,
            "segmented body must not read as a timeout: {response}"
        );
        assert!(response.contains("bridged reply"));
    }

    /// Justifies `DEFAULT_MAX_BODY_BYTES`.
    ///
    /// This is a *constructed* representative request, not captured production
    /// traffic -- stated plainly because the distinction matters. It is built
    /// from the parts a real Claude Code turn always carries: a system prompt,
    /// a set of tool definitions with JSON Schemas, a multi-turn transcript,
    /// and one screenshot. The screenshot alone is what breaks the fixture-era
    /// cap: base64 inflates bytes by 4/3, so even a modest image clears 1 MiB
    /// before any text is counted.
    #[test]
    fn a_representative_request_fits_the_body_cap() {
        let system = "You are Claude Code. ".repeat(500);
        let tools: Vec<serde_json::Value> = (0..15)
            .map(|index| {
                serde_json::json!({
                    "name": format!("tool_{index}"),
                    "description": "A tool with a realistic schema. ".repeat(20),
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "x".repeat(200)},
                            "content": {"type": "string", "description": "y".repeat(200)},
                        },
                        "required": ["path"],
                    },
                })
            })
            .collect();
        let mut messages: Vec<serde_json::Value> = (0..20)
            .map(|index| {
                serde_json::json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("turn {index}: {}", "transcript text ".repeat(120)),
                })
            })
            .collect();
        // One 1-megapixel screenshot. 3 bytes per pixel, then base64's 4/3.
        let screenshot = "A".repeat(1024 * 1024 * 3 / 3 * 4);
        messages.push(serde_json::json!({
            "role": "user",
            "content": [{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": screenshot},
            }],
        }));
        let request = serde_json::json!({
            "model": "claude-sonnet-5",
            "max_tokens": 8192,
            "system": system,
            "tools": tools,
            "messages": messages,
            "stream": true,
        });
        let encoded = serde_json::to_vec(&request).unwrap();

        const FIXTURE_ERA_CAP: usize = 1024 * 1024;
        assert!(
            encoded.len() > FIXTURE_ERA_CAP,
            "the old 1 MiB cap was meant to be the thing this outgrows; got {} bytes",
            encoded.len()
        );
        assert!(
            encoded.len() < DEFAULT_MAX_BODY_BYTES,
            "representative request ({} bytes) must fit the configured cap ({DEFAULT_MAX_BODY_BYTES})",
            encoded.len()
        );
        // It must also translate: a cap that admits a request the translator
        // then refuses would not have bought anything.
        crate::codex_translate::translate_bytes(
            &encoded,
            &crate::codex_translate::TranslateOptions::default(),
        )
        .expect("a representative request must translate");
    }

    /// Justifies the admission policy: concurrent requests beyond the bound
    /// wait in the listen backlog instead of collecting a 503.
    #[test]
    fn concurrent_requests_beyond_the_bound_queue_rather_than_fail() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(
            BridgeConfig {
                max_concurrency: 1,
                ..bridged_config(&upstream)
            }
            .with_request_hold(Duration::from_millis(100)),
        )
        .unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let token = token.clone();
                thread::spawn(move || {
                    request(
                        addr,
                        &authorized("POST", "/v1/messages", &token, PROBE_BODY),
                    )
                })
            })
            .collect();

        for handle in handles {
            let response = handle.join().expect("request thread");
            assert_eq!(
                status(&response),
                200,
                "a queued request must not be answered 503: {response}"
            );
            assert!(response.contains("bridged reply"));
        }
        assert_eq!(
            upstream
                .requests()
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .count(),
            4,
            "each queued original must reach upstream once"
        );
    }

    /// Opt-in, repeatable bridge overhead report for #630. Run this locally
    /// on a quiet machine; it deliberately has no wall-clock assertion.
    #[test]
    #[ignore = "opt-in benchmark; see bench/codex_bridge/README.md"]
    fn benchmark_loopback_bridge_overhead() {
        const REQUESTS: usize = 200;
        let upstream = FakeResponses::start();
        let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();
        let rss_before = current_rss_bytes();
        let started = Instant::now();

        for _ in 0..REQUESTS {
            let response = request(
                addr,
                &authorized("POST", "/v1/messages", &token, PROBE_BODY),
            );
            assert_eq!(
                status(&response),
                200,
                "benchmark request failed: {response}"
            );
        }
        bridge.shutdown().unwrap();
        let elapsed = started.elapsed();
        let rss_after = current_rss_bytes();
        println!(
            "{}",
            serde_json::json!({
                "schema": "clud.bench.codex_bridge.v1",
                "requests": REQUESTS,
                "elapsed_ms": elapsed.as_millis(),
                "requests_per_second": REQUESTS as f64 / elapsed.as_secs_f64(),
                "rss_before_bytes": rss_before,
                "rss_after_bytes": rss_after,
                "rss_growth_bytes": rss_after.saturating_sub(rss_before),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            })
        );
    }

    /// #1054 RED: local admission saturation is not an API error.  A second
    /// socket stays in the kernel backlog past the old finite budget, then its
    /// original request is admitted exactly once when the sole worker exits.
    #[test]
    fn a_saturated_bridge_admits_the_original_request_after_the_old_budget() {
        let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
        let upstream = FakeResponses::start();
        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("bridge.jsonl");
        // Keep the first worker alive past the former admission deadline.
        // The second request must remain unaccepted rather than collecting a
        // synthetic 503, then make one normal upstream call after admission.
        let bridge = BridgeHandle::start(
            BridgeConfig {
                max_concurrency: 1,
                header_timeout: Duration::from_secs(5),
                ..bridged_config(&upstream)
            }
            .with_request_hold(Duration::from_secs(2))
            .with_admission_notifier(admitted_tx)
            .with_log_path(log_path.clone()),
        )
        .unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();

        let mut occupied = TcpStream::connect(addr).unwrap();
        occupied
            .write_all(authorized("HEAD", "/v1/messages", &token, "").as_bytes())
            .unwrap();
        admitted_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("first request admitted");

        let queued_token = token.clone();
        let queued = thread::spawn(move || {
            request(
                addr,
                &authorized("POST", "/v1/messages", &queued_token, PROBE_BODY),
            )
        });
        thread::sleep(Duration::from_millis(500));
        assert!(
            !queued.is_finished(),
            "the queued socket must outlive the old admission budget"
        );
        let response = queued.join().expect("queued request thread");
        assert_eq!(status(&response), 200, "queued request: {response}");
        assert!(!response.contains("bridge busy"));
        assert_eq!(
            upstream
                .requests()
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .count(),
            1,
            "the queued original must make exactly one upstream request"
        );
        drop(occupied);
        let mut bridge = bridge;
        bridge.shutdown().unwrap();
        let log = std::fs::read_to_string(log_path).unwrap_or_default();
        assert!(log.contains("admission_queued"), "{log}");
        assert!(log.contains("admission_acquired"), "{log}");
        assert!(log.contains("wait_ms"), "{log}");
    }

    /// Saturation alone is not an admission queue.  The listener must have an
    /// actual later socket before the aggregate queue observability is emitted.
    #[test]
    fn a_lone_held_request_emits_no_admission_events() {
        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("bridge.jsonl");
        let upstream = FakeResponses::start();
        let mut bridge = BridgeHandle::start(
            bridged_config(&upstream)
                .with_request_hold(Duration::from_millis(250))
                .with_log_path(log_path.clone()),
        )
        .unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );
        assert_eq!(status(&response), 200, "{response}");
        bridge.shutdown().unwrap();
        let log = std::fs::read_to_string(log_path).unwrap_or_default();
        assert!(!log.contains("admission_queued"), "{log}");
        assert!(!log.contains("admission_acquired"), "{log}");
    }

    /// #989 RED: the default must admit a second connection while the first
    /// is still held. At `DEFAULT_MAX_CONCURRENCY == 1` the later socket sat
    /// in the listen backlog and the accept loop logged `admission_queued`;
    /// the assertion here is the *absence* of admission events rather than a
    /// wall-clock threshold, because a timing threshold is flaky across exec
    /// lanes.
    #[test]
    fn two_concurrent_requests_are_admitted_under_the_default_concurrency() {
        assert_eq!(
            BridgeConfig::default().max_concurrency,
            2,
            "the default must leave one slot for the foreground turn and one for anything else"
        );
        let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("bridge.jsonl");
        let upstream = FakeResponses::start();
        let mut bridge = BridgeHandle::start(
            bridged_config(&upstream)
                .with_request_hold(Duration::from_secs(2))
                .with_admission_notifier(admitted_tx)
                .with_log_path(log_path.clone()),
        )
        .unwrap();
        let addr = bridge.socket_addr();
        let token = bridge.bearer_token().to_owned();

        let first_token = token.clone();
        let first = thread::spawn(move || {
            request(
                addr,
                &authorized("POST", "/v1/messages", &first_token, PROBE_BODY),
            )
        });
        admitted_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("first request admitted");

        let second_token = token.clone();
        let second = thread::spawn(move || {
            request(
                addr,
                &authorized("POST", "/v1/messages", &second_token, PROBE_BODY),
            )
        });

        let first = first.join().expect("first request thread");
        let second = second.join().expect("second request thread");
        assert_eq!(status(&first), 200, "first request: {first}");
        assert_eq!(status(&second), 200, "second request: {second}");
        assert_eq!(
            upstream
                .requests()
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .count(),
            2,
            "both requests must reach upstream once each"
        );

        bridge.shutdown().unwrap();
        let log = std::fs::read_to_string(log_path).unwrap_or_default();
        assert!(
            !log.contains("admission_queued"),
            "the second connection must not wait in the backlog: {log}"
        );
        assert!(!log.contains("admission_acquired"), "{log}");
    }

    /// Regression guard: Claude Code sends `POST /v1/messages?beta=true`.
    /// Matching the raw request target 404s a valid request -- and the mock
    /// probe never caught it because it sends a bare path. Found only by
    /// running a real client against the bridge.
    #[test]
    fn a_query_string_does_not_change_the_route() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let token = bridge.bearer_token().to_owned();
        for path in [
            "/v1/messages",
            "/v1/messages?beta=true",
            "/v1/messages?beta=true&foo=bar",
        ] {
            let response = request(
                bridge.socket_addr(),
                &authorized("POST", path, &token, PROBE_BODY),
            );
            assert_eq!(status(&response), 200, "path {path}: {response}");
            assert!(response.contains("bridged reply"), "path {path}");
        }
        // Unknown routes still 404, query string or not.
        let unknown = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/unknown?beta=true", &token, PROBE_BODY),
        );
        assert_eq!(status(&unknown), 404);
    }

    #[test]
    fn the_debug_seam_speaks_the_responses_protocol_and_carries_no_downstream_secret() {
        // Phase 2's seam passed the Anthropic body through unchanged, so the
        // end-to-end tests proved transport and auth but nothing about
        // translation. Assert on what the fake actually receives, so a
        // translation regression fails here.
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );
        assert_eq!(status(&response), 200);
        assert!(response.contains(r#""text":"bridged reply""#));

        let sent = upstream.requests().remove(0);
        assert!(
            sent.starts_with("POST /v1/responses HTTP/1.1"),
            "the bridge must address the Responses endpoint: {sent}"
        );
        assert!(sent.contains("Authorization: Bearer clud-test-upstream-key"));
        // Translated shape, not a passed-through Anthropic body.
        let body = sent.split("\r\n\r\n").nth(1).expect("upstream body");
        let json: serde_json::Value = serde_json::from_str(body).expect("JSON body");
        assert_eq!(json["model"], "gpt-6-sol");
        assert_eq!(json["stream"], true, "upstream is always streamed");
        assert_eq!(json["input"][0]["content"][0]["type"], "input_text");
        assert!(json.get("messages").is_none(), "Anthropic shape leaked");

        // The harness's own downstream bearer must never travel upstream.
        assert!(!sent.contains(bridge.bearer_token()));
    }

    #[test]
    fn cache_identity_is_conversation_scoped_not_request_scoped() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let session = "claude-session-private";
        let turns = [
            None,
            None,
            Some("agent-a-private"),
            Some("agent-a-private"),
            Some("agent-b-private"),
        ];

        for agent in turns {
            let mut headers = vec![("X-Claude-Code-Session-Id", session)];
            if let Some(agent) = agent {
                headers.push(("x-claude-code-agent-id", agent));
            }
            let response = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    bridge.bearer_token(),
                    PROBE_BODY,
                    &headers,
                ),
            );
            assert_eq!(status(&response), 200, "{response}");
        }

        let requests = upstream.requests();
        assert_eq!(requests.len(), 5);
        let identity = |request: &str| {
            let body: serde_json::Value = serde_json::from_str(captured_body(request)).unwrap();
            (
                captured_header(request, "session-id").to_string(),
                captured_header(request, "thread-id").to_string(),
                captured_header(request, "x-client-request-id").to_string(),
                body["prompt_cache_key"].as_str().unwrap().to_string(),
            )
        };
        let identities = requests
            .iter()
            .map(|request| identity(request))
            .collect::<Vec<_>>();
        let main = ConversationKey::from_headers(Some(session), None).id;
        let agent_a = ConversationKey::from_headers(Some(session), Some("agent-a-private")).id;
        let agent_b = ConversationKey::from_headers(Some(session), Some("agent-b-private")).id;

        for index in [0, 1] {
            assert_eq!(
                identities[index],
                (main.clone(), main.clone(), main.clone(), main.clone())
            );
        }
        for index in [2, 3] {
            assert_eq!(
                identities[index],
                (
                    agent_a.clone(),
                    agent_a.clone(),
                    agent_a.clone(),
                    agent_a.clone()
                )
            );
        }
        assert_eq!(
            identities[4],
            (
                agent_b.clone(),
                agent_b.clone(),
                agent_b.clone(),
                agent_b.clone()
            )
        );
        assert_ne!(main, agent_a);
        assert_ne!(main, agent_b);
        assert_ne!(agent_a, agent_b);
        for request in &requests {
            for forbidden in [session, "agent-a-private", "agent-b-private"] {
                assert!(
                    !request.contains(forbidden),
                    "raw Claude identity leaked: {request}"
                );
            }
        }
    }

    #[test]
    fn headerless_cache_identity_is_stable_per_bridge_but_not_shared_between_launches() {
        let upstream = FakeResponses::start();
        let first = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let second = BridgeHandle::start(bridged_config(&upstream)).unwrap();

        for bridge in [&first, &first, &second, &second] {
            let response = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
            );
            assert_eq!(status(&response), 200, "{response}");
        }

        let identities = upstream
            .requests()
            .iter()
            .map(|request| {
                let body: serde_json::Value = serde_json::from_str(captured_body(request)).unwrap();
                let identity = captured_header(request, "session-id");
                assert_eq!(captured_header(request, "thread-id"), identity);
                assert_eq!(captured_header(request, "x-client-request-id"), identity);
                assert_eq!(body["prompt_cache_key"].as_str(), Some(identity));
                identity.to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(identities.len(), 4);
        assert_eq!(identities[0], identities[1]);
        assert_eq!(identities[2], identities[3]);
        assert_ne!(identities[0], identities[2]);
        assert!(identities
            .iter()
            .all(|identity| identity.starts_with("bridge-")));
        assert!(identities
            .iter()
            .all(|identity| identity != "bridge-session"));
    }

    /// What a user typing `/model luna@high` actually produces on the wire.
    ///
    /// The premise this rests on is that the harness forwards the model
    /// string unvalidated behind a custom `ANTHROPIC_BASE_URL`; this test
    /// pins our half of that contract — a catalog-known id (anything else is
    /// refused up front since #1005) is parsed, expanded, and sent, with the
    /// suffix landing in `reasoning.effort` rather than in the model id.
    #[test]
    fn a_request_selecting_a_model_and_effort_reaches_upstream_expanded() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"luna@high","messages":[{"role":"user","content":"hi"}],"stream":false}"#,
            ),
        );
        assert_eq!(status(&response), 200);

        let sent = upstream.requests().remove(0);
        let body = sent.split("\r\n\r\n").nth(1).expect("upstream body");
        let json: serde_json::Value = serde_json::from_str(body).expect("JSON body");
        assert_eq!(json["model"], "gpt-6-luna");
        assert_eq!(json["reasoning"]["effort"], "high");
        assert!(
            !body.contains("luna@high"),
            "the suffix must not travel as part of the model id: {body}"
        );
    }

    /// `/effort minimal` must fail at the bridge boundary rather than run at
    /// a different effort (#821).
    ///
    /// `minimal` is a real Responses value that gpt-5.6 does not accept, so
    /// it is the one spelling a user can plausibly type and have silently
    /// downgraded. Nothing may reach upstream, and the client must be told
    /// what is accepted.
    #[test]
    fn an_unsupported_output_config_effort_is_rejected_before_upstream() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"gpt-5.6-terra","messages":[{"role":"user","content":"hi"}],"output_config":{"effort":"minimal"},"stream":false}"#,
            ),
        );

        assert_eq!(status(&response), 400);
        assert!(response.contains("minimal"), "{response}");
        assert!(response.contains("xhigh"), "{response}");
        assert!(
            upstream.requests().is_empty(),
            "a rejected effort must not be billed upstream"
        );
    }

    #[test]
    fn six_one_rejects_unsupported_effort_before_upstream() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        for model in ["clud-claude-codex-sol-6-1", "clud-claude-codex-terra-6-1"] {
            let body = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}],
                "output_config": {"effort": "none"},
                "stream": false,
            })
            .to_string();
            let response = request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
            );
            assert_eq!(status(&response), 400, "{model}: {response}");
        }
        assert!(upstream.requests().is_empty());
    }

    /// A launch-time `--model` selection is the default for requests that do
    /// not name one — which is every request the harness sends, since it
    /// sends its own `claude-*` id.
    #[test]
    fn the_launch_time_selection_becomes_the_default_for_claude_ids() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(
            bridged_config(&upstream)
                .with_default_model(Some(ModelSpec::parse("sol@xhigh").unwrap())),
        )
        .unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );
        assert_eq!(status(&response), 200);

        let sent = upstream.requests().remove(0);
        let body = sent.split("\r\n\r\n").nth(1).expect("upstream body");
        let json: serde_json::Value = serde_json::from_str(body).expect("JSON body");
        assert_eq!(json["model"], "gpt-6-sol");
        assert_eq!(json["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn discovery_id_preserves_launch_pin_instead_of_reresolving_remote_default() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(
            bridged_config(&upstream)
                .with_default_model(Some(ModelSpec::parse("gpt-5.6-sol@low").unwrap()))
                .with_selected_model_cli_id(Some("codex-sol".to_string())),
        )
        .unwrap();
        let body = PROBE_BODY.replace("claude-x", "clud-claude-codex-sol");
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &body),
        );
        assert_eq!(status(&response), 200, "{response}");
        let sent = upstream.requests().remove(0);
        let body: serde_json::Value =
            serde_json::from_str(sent.split("\r\n\r\n").nth(1).expect("request body")).unwrap();
        assert_eq!(body["model"], "gpt-5.6-sol");
    }

    #[test]
    fn pre_output_context_failure_compacts_canonical_history_and_retries_once() {
        let upstream = FakeResponses::start_with_responses(vec![
            None,
            Some(context_length_failure_response()),
            Some(compact_success_response()),
            Some(recovery_success_response()),
        ]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let session = [("X-Claude-Code-Session-Id", "identity-through-compact")];
        let first = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_STREAM_BODY,
                &session,
            ),
        );
        assert_eq!(status(&first), 200, "{first}");

        let second = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"first"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"pending"}],"stream":true}"#,
                &session,
            ),
        );
        assert_eq!(status(&second), 200, "{second}");
        let body = decode_chunked_body(&second);
        assert_eq!(body.matches("event: message_start").count(), 1, "{body}");
        assert_eq!(body.matches("recovered").count(), 1, "{body}");
        assert!(!body.contains("context is too long"), "{body}");
        assert!(!body.contains("event: error"), "{body}");

        let requests = upstream.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .count(),
            3,
            "one initial turn plus exactly two ordinary attempts: {requests:#?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses/compact HTTP/1.1"))
                .count(),
            1,
            "exactly one compact request: {requests:#?}"
        );
        let compact = requests
            .iter()
            .find(|request| request.starts_with("POST /v1/responses/compact HTTP/1.1"))
            .expect("compact request");
        let compact_body: serde_json::Value =
            serde_json::from_str(compact.split("\r\n\r\n").nth(1).expect("compact body"))
                .expect("compact JSON");
        let compact_text: Vec<_> = compact_body["input"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|item| {
                item.pointer("/content")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
            })
            .collect();
        assert!(compact_text.contains(&"hi"));
        assert!(!compact_text.contains(&"pending"));
        let retry = requests
            .iter()
            .rfind(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
            .expect("retry request");
        let retry_body: serde_json::Value =
            serde_json::from_str(retry.split("\r\n\r\n").nth(1).expect("retry body"))
                .expect("retry JSON");
        assert_eq!(retry_body["input"][0]["type"], "compaction");
        assert_eq!(
            retry_body["input"][0]["encrypted_content"],
            "opaque-summary"
        );
        assert_eq!(retry_body["input"][1]["content"][0]["text"], "pending");

        // The request, the internal compaction request, and its retry all
        // represent one Claude conversation. None may get a fresh UUID just
        // because it crosses a lifecycle boundary.
        let expected_identity =
            ConversationKey::from_headers(Some("identity-through-compact"), None).id;
        for request in &requests {
            assert_eq!(captured_header(request, "session-id"), expected_identity);
            assert_eq!(captured_header(request, "thread-id"), expected_identity);
            assert_eq!(
                captured_header(request, "x-client-request-id"),
                expected_identity
            );
        }
    }

    #[test]
    fn workflow_child_does_not_replay_parent_function_call() {
        let upstream =
            FakeResponses::start_with_responses(vec![Some(function_call_response()), None]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let session = [("X-Claude-Code-Session-Id", "session-parent-child")];
        let parent = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_BODY,
                &session,
            ),
        );
        assert_eq!(status(&parent), 200, "{parent}");

        let mut child_headers = session.to_vec();
        child_headers.push(("x-claude-code-agent-id", "agent-child"));
        let child = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"child"}],"stream":false}"#,
                &child_headers,
            ),
        );
        assert_eq!(status(&child), 200, "{child}");

        let requests = upstream.requests();
        let child_body: serde_json::Value = serde_json::from_str(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .nth(1)
                .expect("child upstream request")
                .split("\r\n\r\n")
                .nth(1)
                .expect("child request body"),
        )
        .expect("child JSON body");
        assert!(
            !child_body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call"),
            "child replayed the parent function call: {child_body}"
        );
    }

    #[test]
    fn agent_continuation_preserves_eight_split_parallel_tool_results() {
        let upstream = FakeResponses::start_with_responses(vec![
            Some(parallel_function_call_response(8)),
            None,
        ]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let headers = [
            ("X-Claude-Code-Session-Id", "session-parallel"),
            ("x-claude-code-agent-id", "agent-parallel"),
        ];
        let first = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_BODY,
                &headers,
            ),
        );
        assert_eq!(status(&first), 200, "{first}");

        let continuation = split_parallel_result_body(8, true);
        let second = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                &continuation,
                &headers,
            ),
        );
        assert_eq!(status(&second), 200, "{second}");

        let requests = upstream.requests();
        let body: serde_json::Value = serde_json::from_str(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .nth(1)
                .expect("continuation upstream request")
                .split("\r\n\r\n")
                .nth(1)
                .expect("continuation request body"),
        )
        .expect("continuation JSON");
        let input = body["input"].as_array().unwrap();
        let calls = input
            .iter()
            .filter(|item| item["type"] == "function_call")
            .collect::<Vec<_>>();
        let outputs = input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 8, "all calls must remain canonical: {body}");
        assert_eq!(outputs.len(), 8, "all results must reach upstream: {body}");
        for index in 0..8 {
            assert_eq!(calls[index]["call_id"], format!("call_{index}"));
            assert_eq!(outputs[index]["call_id"], format!("call_{index}"));
        }
    }

    /// Regression for #960: a failed turn does not commit its pending tool
    /// result, and Claude records a later assistant error before replaying the
    /// conversation. The bridge must recover the real result from that full
    /// replay rather than wedging on the final-assistant suffix boundary.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    fn failed_stream_recovers_pending_output_from_full_replay() {
        let failed_assistants = [
            (
                "synthetic-error",
                serde_json::json!({
                    "role": "assistant",
                    "content": "API Error: Server error mid-response.",
                }),
            ),
            (
                "partial-tool-use",
                serde_json::json!({
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "call_partial",
                        "name": "SendMessage",
                        "input": {},
                    }],
                }),
            ),
        ];

        for (case, failed_assistant) in failed_assistants {
            let upstream = FakeResponses::start_with_responses(vec![
                Some(function_call_response()),
                Some(visible_transport_failure_response()),
                None,
                None,
            ]);
            let tmp = tempfile::tempdir().unwrap();
            let log_path = tmp.path().join("bridge.jsonl");
            let mut bridge =
                BridgeHandle::start(bridged_config(&upstream).with_log_path(log_path.clone()))
                    .unwrap();
            let headers = [("X-Claude-Code-Session-Id", "secret-session-960")];

            let first = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    bridge.bearer_token(),
                    PROBE_STREAM_BODY,
                    &headers,
                ),
            );
            assert_eq!(status(&first), 200, "{case}: {first}");

            let failed = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    bridge.bearer_token(),
                    &failed_turn_replay_body(Vec::new()),
                    &headers,
                ),
            );
            assert_eq!(status(&failed), 200, "{case}: {failed}");
            assert!(failed.contains("partial"), "{case}: {failed}");
            assert!(failed.contains("event: error"), "{case}: {failed}");
            assert_eq!(
                upstream
                    .requests()
                    .iter()
                    .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                    .count(),
                2,
                "{case}: a visible failed turn must not be retried"
            );

            let recovered = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    bridge.bearer_token(),
                    &failed_turn_replay_body(vec![
                        failed_assistant.clone(),
                        serde_json::json!({"role": "user", "content": "continue"}),
                    ]),
                    &headers,
                ),
            );
            assert_eq!(status(&recovered), 200, "{case}: {recovered}");

            let later = request(
                bridge.socket_addr(),
                &authorized_with_headers(
                    "POST",
                    "/v1/messages",
                    bridge.bearer_token(),
                    &failed_turn_replay_body(vec![
                        failed_assistant,
                        serde_json::json!({"role": "user", "content": "continue"}),
                        serde_json::json!({"role": "assistant", "content": "bridged reply"}),
                        serde_json::json!({"role": "user", "content": "still usable"}),
                    ]),
                    &headers,
                ),
            );
            assert_eq!(status(&later), 200, "{case}: {later}");

            let requests = upstream
                .requests()
                .into_iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .collect::<Vec<_>>();
            assert_eq!(requests.len(), 4, "{case}: recovery did not reach upstream");
            for request in &requests[2..] {
                let body: serde_json::Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").expect("upstream body").1)
                        .expect("upstream JSON");
                let outputs = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| {
                        item["type"] == "function_call_output" && item["call_id"] == "call_parent"
                    })
                    .count();
                assert_eq!(outputs, 1, "{case}: recovered output duplicated: {body}");
            }

            bridge.shutdown().unwrap();
            let log = std::fs::read_to_string(log_path).unwrap();
            assert!(
                log.contains(r#""event":"pending_outputs_recovered""#),
                "{case}: {log}"
            );
            assert!(log.contains(r#""recovered_count":1"#), "{case}: {log}");
            assert!(
                log.contains(r#""conversation_scope":"main""#),
                "{case}: {log}"
            );
            for forbidden in ["call_parent", "result-parent", "secret-session-960"] {
                assert!(
                    !log.contains(forbidden),
                    "{case}: leaked {forbidden}: {log}"
                );
            }
        }
    }

    /// A harness transcript whose compaction dropped the assistant `tool_use`
    /// but kept the user `tool_result` — the shape that wedged production.
    fn orphaned_tool_result_body() -> String {
        serde_json::json!({
            "model": "claude-x",
            "messages": [
                {"role": "user", "content": "carry on"},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_0",
                    "content": "result-0",
                }]},
            ],
            "stream": false,
        })
        .to_string()
    }

    /// End-to-end reproduction of the `/loop` wedge, through the real bridge.
    ///
    /// Sequence, exactly as the fastled session ran it:
    ///   1. a turn that leaves a tool batch outstanding (canonical history now
    ///      holds a `function_call` with no result);
    ///   2. `POST /_clud/context/compact` — `validate_canonical_history` fails
    ///      on that outstanding call, so the bridge takes the documented
    ///      fallback and `clear_items()` the history;
    ///   3. the harness sends its own compacted transcript, in which the
    ///      assistant `tool_use` is gone but the user `tool_result` remains.
    ///
    /// Before the fix, step 3 reached upstream verbatim and earned a permanent
    /// `invalid_request_error: No tool call found for function call output`,
    /// which then repeated on every scheduled firing.
    #[test]
    fn compaction_fallback_then_orphaned_result_still_reaches_upstream_valid() {
        let upstream = FakeResponses::start_with_responses(vec![
            Some(parallel_function_call_response(2)),
            None,
        ]);
        let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();

        // 1. Upstream answers with a parallel tool batch, so canonical
        //    history now holds `function_call`s whose results have not
        //    arrived — the "tool batch outstanding" state the bridge's own
        //    comment calls out.
        let first = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );
        assert_eq!(status(&first), 200, "{first}");

        // 2. Compaction with the batch outstanding -> documented fallback,
        //    which clears the canonical history.
        let compacted = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/_clud/context/compact",
                bridge.bearer_token(),
                r#"{"hook_event_name":"PreCompact","trigger":"auto"}"#,
            ),
        );
        assert_eq!(
            status(&compacted),
            204,
            "the fallback must not block the harness's compaction lifecycle: {compacted}"
        );

        // 3. The harness's own compacted transcript, orphaned.
        let after = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                &orphaned_tool_result_body(),
            ),
        );
        assert_ne!(
            status(&after),
            400,
            "an orphaned tool result must not wedge the session: {after}"
        );

        // The decisive assertion: whatever we sent upstream carried no
        // function_call_output without a matching function_call.
        let sent = upstream.requests();
        let last = sent.last().expect("a request must have reached upstream");
        let body: serde_json::Value =
            serde_json::from_str(last.split("\r\n\r\n").nth(1).expect("request body"))
                .expect("upstream body is json");
        let input = body["input"].as_array().cloned().unwrap_or_default();
        let calls: std::collections::HashSet<&str> = input
            .iter()
            .filter(|item| item["type"] == "function_call")
            .filter_map(|item| item["call_id"].as_str())
            .collect();
        let orphans = input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .filter(|item| {
                item["call_id"]
                    .as_str()
                    .is_none_or(|id| !calls.contains(id))
            })
            .count();
        assert_eq!(orphans, 0, "orphaned tool result reached upstream: {body}");
        // And the result itself was preserved rather than dropped.
        assert!(
            last.contains("result-0"),
            "the tool output must survive the repair: {last}"
        );
        let _ = bridge.shutdown();
    }

    #[test]
    fn unmatched_parallel_call_is_rejected_locally_with_safe_diagnostics() {
        let upstream = FakeResponses::start_with_responses(vec![
            Some(parallel_function_call_response(2)),
            None,
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge =
            BridgeHandle::start(bridged_config(&upstream).with_log_path(log_path.clone())).unwrap();
        let headers = [
            ("X-Claude-Code-Session-Id", "secret-session-id"),
            ("x-claude-code-agent-id", "secret-agent-id"),
        ];
        let first = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_BODY,
                &headers,
            ),
        );
        assert_eq!(status(&first), 200, "{first}");

        let malformed = split_parallel_result_body(2, false);
        let second = request(
            bridge.socket_addr(),
            &authorized_with_headers(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                &malformed,
                &headers,
            ),
        );
        assert_eq!(status(&second), 400, "{second}");
        bridge.shutdown().unwrap();

        let upstream_requests = upstream
            .requests()
            .into_iter()
            .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
            .count();
        assert_eq!(
            upstream_requests, 1,
            "invalid continuation reached upstream"
        );
        let log = std::fs::read_to_string(log_path).unwrap();
        assert!(
            log.contains(r#""event":"continuation_invariant_failure""#),
            "{log}"
        );
        assert!(log.contains(r#""unmatched_call_count":2"#), "{log}");
        assert!(log.contains(r#""conversation_scope":"agent""#), "{log}");
        for forbidden in [
            "call_0",
            "call_1",
            "secret-session-id",
            "secret-agent-id",
            "missing results",
        ] {
            assert!(!log.contains(forbidden), "leaked {forbidden:?}: {log}");
        }
    }

    #[test]
    fn second_context_failure_is_terminal_and_a_later_turn_still_works() {
        let upstream = FakeResponses::start_with_responses(vec![
            None,
            Some(context_length_failure_response()),
            Some(compact_success_response()),
            Some(context_length_failure_response()),
            Some(recovery_success_response()),
        ]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let first = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_STREAM_BODY,
            ),
        );
        assert_eq!(status(&first), 200, "{first}");
        let failed = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"first"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"pending"}],"stream":true}"#,
            ),
        );
        assert_eq!(status(&failed), 200, "{failed}");
        let failed_body = decode_chunked_body(&failed);
        assert_eq!(
            failed_body.matches("event: message_start").count(),
            1,
            "{failed_body}"
        );
        assert_eq!(
            failed_body.matches("context is too long").count(),
            1,
            "{failed_body}"
        );
        assert!(!failed_body.contains("recovered"), "{failed_body}");

        let later = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"independent"}],"stream":true}"#,
            ),
        );
        assert_eq!(status(&later), 200, "{later}");
        assert!(decode_chunked_body(&later).contains("recovered"), "{later}");
        let requests = upstream.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses/compact HTTP/1.1"))
                .count(),
            1,
            "the second context failure must not compact again: {requests:#?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .count(),
            4,
            "one first turn, two bounded attempts, then one independent turn: {requests:#?}"
        );
    }

    #[test]
    fn visible_context_failure_never_compacts_or_retries() {
        let visible_then_failed = response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"partial\"}\n\n\
             event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"invalid_request\",\"code\":\"context_length_exceeded\"}}}\n\n",
        );
        let upstream = FakeResponses::start_with_responses(vec![
            None,
            Some(visible_then_failed),
            Some(recovery_success_response()),
        ]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let first = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_STREAM_BODY,
            ),
        );
        assert_eq!(status(&first), 200, "{first}");
        let second = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"first"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"pending"}],"stream":true}"#,
            ),
        );
        assert_eq!(status(&second), 200, "{second}");
        let body = decode_chunked_body(&second);
        assert!(body.contains("partial"), "{body}");
        assert!(body.contains("context is too long"), "{body}");
        assert!(!body.contains("recovered"), "{body}");
        let requests = upstream.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /v1/responses HTTP/1.1"))
                .count(),
            2,
            "the visible turn must not retry: {requests:#?}"
        );
        assert!(
            !requests
                .iter()
                .any(|request| request.starts_with("POST /v1/responses/compact HTTP/1.1")),
            "the visible turn must not compact: {requests:#?}"
        );
    }

    /// The reported incident, reproduced at the HTTP layer.
    ///
    /// A drained account previously reached the user as
    /// `upstream provider returned status 429` with no `Retry-After`, so the
    /// only reset time on screen came from the client's own separate
    /// accounting -- about a different limit than the one that broke the turn.
    #[test]
    fn an_in_band_400_after_a_successful_turn_is_redacted_and_classified() {
        let secret = "second-turn prompt SECRET_PROMPT";
        let tool_output = "tool-output SECRET_TOOL_OUTPUT";
        let upstream_secret = "upstream-message SECRET_UPSTREAM_DETAIL";
        let failed_event = format!(
            "event: response.failed\ndata: {{\"type\":\"response.failed\",\"response\":{{\"error\":{{\"type\":\"invalid_request\",\"code\":\"context_length_exceeded\",\"message\":\"{upstream_secret}\"}},\"request_id\":\"req_second_turn\"}}}}\n\n"
        );
        let failed_reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{failed_event}",
            failed_event.len()
        )
        .into_bytes();
        let upstream = FakeResponses::start_with_responses(vec![
            None,
            Some(failed_reply.clone()),
            Some(compact_success_response()),
            Some(failed_reply),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge =
            BridgeHandle::start(bridged_config(&upstream).with_log_path(log_path.clone())).unwrap();

        let first = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_STREAM_BODY,
            ),
        );
        assert_eq!(status(&first), 200, "{first}");
        let continuation = format!(
            r#"{{"model":"claude-x","messages":[{{"role":"user","content":"{secret}"}},{{"role":"assistant","content":"done"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"call_1","content":"{tool_output}"}}]}}],"stream":true}}"#
        );
        let second = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), &continuation),
        );
        bridge.shutdown().unwrap();

        assert_eq!(status(&second), 200, "{second}");
        assert!(second.contains("context is too long"), "{second}");
        let text = std::fs::read_to_string(log_path).unwrap();
        assert!(
            text.contains(r#""event":"in_band_upstream_failure""#),
            "{text}"
        );
        assert!(text.contains(r#""category":"context_length""#), "{text}");
        assert!(
            text.contains(r#""code":"context_length_exceeded""#),
            "{text}"
        );
        assert!(text.contains(r#""request_id":"req_second_turn""#), "{text}");
        assert!(text.contains(r#""phase":"continuation""#), "{text}");
        // The scenario is "an in-band 400 *after a successful turn*", so the
        // log legitimately holds one line per turn. Parsing the whole file as
        // a single JSON value fails with `trailing characters, line: 2`.
        // Select the event by name, the way
        // `a_non_streaming_in_band_400_is_logged_and_sanitized` below already
        // does against the same log.
        let event = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|event| event["event"] == "in_band_upstream_failure")
            .expect("in-band failure event");
        assert_eq!(
            event.pointer("/request_shape/input_kinds"),
            Some(&serde_json::json!([
                "message",
                "message",
                "function_call_output"
            ]))
        );
        assert_eq!(
            event.pointer("/request_shape/has_reasoning"),
            Some(&serde_json::json!(true))
        );
        for forbidden in [secret, tool_output, upstream_secret, "Authorization"] {
            assert!(
                !second.contains(forbidden),
                "client leaked {forbidden:?}: {second}"
            );
            assert!(
                !text.contains(forbidden),
                "log leaked {forbidden:?}: {text}"
            );
        }
    }

    #[test]
    fn a_non_streaming_in_band_400_is_logged_and_sanitized() {
        let upstream_secret = "non-streaming SECRET_UPSTREAM_DETAIL";
        let failed_event = format!(
            "event: response.failed\ndata: {{\"type\":\"response.failed\",\"response\":{{\"error\":{{\"type\":\"invalid_request\",\"code\":\"cyber_policy\",\"message\":\"{upstream_secret}\"}},\"request_id\":\"req_non_streaming\"}}}}\n\n"
        );
        let failed_reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{failed_event}",
            failed_event.len()
        )
        .into_bytes();
        let upstream = FakeResponses::start_with_response(Some(failed_reply));
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("bridge.jsonl");
        let mut bridge =
            BridgeHandle::start(bridged_config(&upstream).with_log_path(log_path.clone())).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );
        bridge.shutdown().unwrap();

        assert_eq!(status(&response), 400, "{response}");
        assert!(response.contains("provider policy"), "{response}");
        assert!(!response.contains(upstream_secret), "{response}");
        let text = std::fs::read_to_string(log_path).unwrap();
        let event = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|event| event["event"] == "in_band_upstream_failure")
            .expect("in-band failure event");
        assert_eq!(event["category"], "policy", "{text}");
        assert_eq!(event["code"], "cyber_policy", "{text}");
        assert_eq!(event["request_id"], "req_non_streaming", "{text}");
        assert!(!text.contains(upstream_secret), "{text}");
    }

    #[test]
    fn an_exhausted_account_answers_429_billing_error_with_a_retry_after() {
        let body = r#"{"error":{"code":"usage_limit_reached","message":"quota exhausted for acct_42","resets_in_seconds":442242}}"#;
        let reply = format!(
            "HTTP/1.1 429 Too Many Requests
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
            body.len()
        )
        .into_bytes();
        let upstream = FakeResponses::start_with_response(Some(reply));
        let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );
        bridge.shutdown().unwrap();

        assert_eq!(status(&response), 429);
        assert!(
            response.starts_with("HTTP/1.1 429 Too Many Requests"),
            "the status line had no reason phrase: {response}"
        );
        assert!(
            response.contains("Retry-After: 442242"),
            "no Retry-After header: {response}"
        );
        assert!(response.contains("billing_error"), "{response}");
        assert!(response.contains("quota exhausted"), "{response}");
        // Human-readable, not a raw second count to do arithmetic on.
        assert!(response.contains("5d 2h"), "{response}");
        // The upstream body never crosses over.
        assert!(!response.contains("acct_42"), "{response}");
        // Attempted exactly once: a multi-day exhaustion is not transient.
        assert_eq!(
            upstream.requests().len(),
            1,
            "an exhausted plan was retried"
        );
    }

    #[test]
    fn first_replayed_request_seeds_full_history_for_later_continuations() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let first_body = r#"{"model":"claude-x","messages":[{"role":"user","content":"old user"},{"role":"assistant","content":"old assistant"},{"role":"user","content":"first pending"}],"stream":false}"#;
        let first = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), first_body),
        );
        assert_eq!(status(&first), 200, "{first}");

        let second_body = r#"{"model":"claude-x","messages":[{"role":"user","content":"old user"},{"role":"assistant","content":"old assistant"},{"role":"user","content":"first pending"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"later pending"}],"stream":false}"#;
        let second = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), second_body),
        );
        assert_eq!(status(&second), 200, "{second}");

        let requests = upstream.requests();
        let body: serde_json::Value = serde_json::from_str(
            requests[1]
                .split("\r\n\r\n")
                .nth(1)
                .expect("second upstream body"),
        )
        .expect("second upstream JSON");
        let texts: Vec<_> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter_map(|part| part["text"].as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "old user",
                "old assistant",
                "first pending",
                "later pending"
            ],
            "the first request must seed its complete translated replay exactly once"
        );
    }

    #[test]
    fn continuation_merges_new_developer_instruction_without_replaying_history() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let first_body = r#"{"model":"claude-x","system":"first instruction","messages":[{"role":"user","content":"old user"}],"stream":false}"#;
        assert_eq!(
            status(&request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", bridge.bearer_token(), first_body),
            )),
            200
        );

        let second_body = r#"{"model":"claude-x","system":"updated instruction","messages":[{"role":"user","content":"old user"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"new pending"}],"stream":false}"#;
        assert_eq!(
            status(&request(
                bridge.socket_addr(),
                &authorized("POST", "/v1/messages", bridge.bearer_token(), second_body),
            )),
            200
        );

        let requests = upstream.requests();
        let body: serde_json::Value = serde_json::from_str(
            requests[1]
                .split("\r\n\r\n")
                .nth(1)
                .expect("second upstream body"),
        )
        .expect("second upstream JSON");
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input.len(),
            2,
            "replayed display turns must not be duplicated"
        );
        assert_eq!(body["instructions"], "updated instruction");
        assert_eq!(input[0]["content"][0]["text"], "old user");
        assert_eq!(input[1]["content"][0]["text"], "new pending");
    }

    #[test]
    fn in_band_failure_followed_by_eof_preserves_provider_classification() {
        let failed = response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"usage_limit_reached\",\"message\":\"secret account detail\"}}}\n\n",
        );
        let upstream = FakeResponses::start_with_response(Some(failed));
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );

        assert_eq!(status(&response), 429, "{response}");
        assert!(response.contains("billing_error"), "{response}");
        assert!(response.contains("quota exhausted"), "{response}");
        assert!(!response.contains("secret account detail"), "{response}");
    }

    #[test]
    fn visible_in_band_auth_failure_marks_the_next_turn_without_replay() {
        let failed = response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"hello\"}\n\n\
             event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_token\",\"message\":\"Bearer secret-token\"}}}\n\n",
        );
        let upstream = FakeResponses::start_with_response(Some(failed));
        let config = bridged_config(&upstream);
        let marker = Arc::clone(&config.auth_recheck);
        let bridge = BridgeHandle::start(config).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                PROBE_STREAM_BODY,
            ),
        );

        assert_eq!(status(&response), 200, "{response}");
        assert!(response.contains("authentication_error"), "{response}");
        assert!(!response.contains("secret-token"), "{response}");
        assert!(marker.lock().unwrap().is_some());
        assert_eq!(
            upstream.requests().len(),
            1,
            "visible output must not replay"
        );
    }

    #[test]
    fn non_streaming_in_band_auth_failure_also_marks_the_next_turn() {
        let failed = response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_token\",\"message\":\"Bearer secret-token\"}}}\n\n",
        );
        let upstream = FakeResponses::start_with_response(Some(failed));
        let config = bridged_config(&upstream);
        let marker = Arc::clone(&config.auth_recheck);
        let bridge = BridgeHandle::start(config).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );

        assert_eq!(status(&response), 502, "{response}");
        assert!(!response.contains("secret-token"), "{response}");
        assert!(marker.lock().unwrap().is_some());
        assert_eq!(upstream.requests().len(), 1);
    }

    #[test]
    fn marked_next_turn_forces_one_credential_recheck() {
        struct RecoveringCredentials(AtomicUsize);
        impl CredentialSource for RecoveringCredentials {
            fn resolve(&self) -> Result<crate::codex_upstream::UpstreamTarget, UpstreamError> {
                Ok(crate::codex_upstream::UpstreamTarget::new(
                    "https://example.test",
                    "Bearer rejected",
                ))
            }

            fn recover_after_unauthorized(
                &self,
                _rejected: &crate::codex_upstream::UpstreamTarget,
            ) -> Result<Option<crate::codex_upstream::UpstreamTarget>, UpstreamError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Some(crate::codex_upstream::UpstreamTarget::new(
                    "https://example.test",
                    "Bearer recovered",
                )))
            }
        }

        let credentials = RecoveringCredentials(AtomicUsize::new(0));
        let marker = Mutex::new(Some(crate::codex_upstream::UpstreamTarget::new(
            "https://example.test",
            "Bearer rejected",
        )));
        assert!(recover_marked_credentials(&credentials, &marker).unwrap());
        assert!(marker.lock().unwrap().is_none());
        assert!(!recover_marked_credentials(&credentials, &marker).unwrap());
        assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn response_incomplete_is_a_successful_max_tokens_completion() {
        let upstream = FakeResponses::start_with_response(Some(incomplete_response()));
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized("POST", "/v1/messages", bridge.bearer_token(), PROBE_BODY),
        );

        assert_eq!(status(&response), 200, "{response}");
        let message: serde_json::Value =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).expect("response body"))
                .expect("Anthropic message JSON");
        assert_eq!(message["stop_reason"], "max_tokens");
        assert_eq!(message["content"][0]["text"], "truncated");
        assert_eq!(message["usage"]["output_tokens"], 1);
    }

    #[test]
    fn non_streaming_capacity_rejection_evicts_stale_history_after_reply() {
        let upstream = FakeResponses::start();
        let limits = HistoryLimits {
            max_conversations: 1,
            max_items_per_conversation: 1,
            max_bytes_per_conversation: 1024,
        };
        let bridge =
            BridgeHandle::start(bridged_config(&upstream).with_history_limits(limits)).unwrap();
        let first = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"stale first"}],"stream":false}"#,
            ),
        );
        assert_eq!(status(&first), 200, "{first}");
        let capacity_rejected = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"stale first"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"capacity turn"}],"stream":false}"#,
            ),
        );
        assert_eq!(status(&capacity_rejected), 200, "{capacity_rejected}");
        let fresh = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"fresh seed"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"later pending"}],"stream":false}"#,
            ),
        );
        assert_eq!(status(&fresh), 200, "{fresh}");

        let requests = upstream.requests();
        let body: serde_json::Value = serde_json::from_str(
            requests[2]
                .split("\r\n\r\n")
                .nth(1)
                .expect("fresh upstream body"),
        )
        .expect("fresh upstream JSON");
        let texts: Vec<_> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter_map(|part| part["text"].as_str())
            .collect();
        assert_eq!(texts, vec!["fresh seed", "bridged reply", "later pending"]);
    }

    #[test]
    fn streaming_capacity_rejection_evicts_stale_history_after_reply() {
        let upstream = FakeResponses::start();
        let limits = HistoryLimits {
            max_conversations: 1,
            max_items_per_conversation: 1,
            max_bytes_per_conversation: 1024,
        };
        let bridge =
            BridgeHandle::start(bridged_config(&upstream).with_history_limits(limits)).unwrap();
        let first = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"stale first"}],"stream":true}"#,
            ),
        );
        assert_eq!(status(&first), 200, "{first}");
        let capacity_rejected = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"stale first"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"capacity turn"}],"stream":true}"#,
            ),
        );
        assert_eq!(status(&capacity_rejected), 200, "{capacity_rejected}");
        let fresh = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"fresh seed"},{"role":"assistant","content":"bridged reply"},{"role":"user","content":"later pending"}],"stream":true}"#,
            ),
        );
        assert_eq!(status(&fresh), 200, "{fresh}");

        let requests = upstream.requests();
        let body: serde_json::Value = serde_json::from_str(
            requests[2]
                .split("\r\n\r\n")
                .nth(1)
                .expect("fresh upstream body"),
        )
        .expect("fresh upstream JSON");
        let texts: Vec<_> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter_map(|part| part["text"].as_str())
            .collect();
        assert_eq!(texts, vec!["fresh seed", "bridged reply", "later pending"]);
    }

    #[test]
    fn eof_before_response_completed_is_an_upstream_failure_and_does_not_record_history() {
        let incomplete = response_with_events(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"partial\"}\n\n",
        );
        let upstream = FakeResponses::start_with_responses(vec![Some(incomplete), None]);
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let failed = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"discard me"}],"stream":false}"#,
            ),
        );
        assert_eq!(status(&failed), 502, "{failed}");
        assert!(failed.contains("upstream unreachable"), "{failed}");

        let later = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"claude-x","messages":[{"role":"user","content":"keep me"}],"stream":false}"#,
            ),
        );
        assert_eq!(status(&later), 200, "{later}");
        let requests = upstream.requests();
        let body: serde_json::Value = serde_json::from_str(
            requests[1]
                .split("\r\n\r\n")
                .nth(1)
                .expect("later upstream body"),
        )
        .expect("later upstream JSON");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["content"][0]["text"], "keep me");
    }

    /// A typo must not be silently billed as the default model.
    ///
    /// Since #1005 the bridge refuses `tera` itself, so the translator's
    /// `SelectionError::UnknownAlias` text is no longer reachable from this
    /// route -- `codex_model::tests::an_unknown_alias_names_the_valid_ones`
    /// keeps that assertion. What this test pins is the bridge's own case-1
    /// wording: the typo is named, and the servable IDs are listed.
    #[test]
    fn a_typo_of_a_model_alias_is_a_400_naming_the_servable_ids() {
        let upstream = FakeResponses::start();
        let bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let response = request(
            bridge.socket_addr(),
            &authorized(
                "POST",
                "/v1/messages",
                bridge.bearer_token(),
                r#"{"model":"tera","messages":[{"role":"user","content":"hi"}],"stream":false}"#,
            ),
        );
        assert_eq!(status(&response), 400);
        assert!(
            response.contains("unknown clud Codex model 'tera'"),
            "{response}"
        );
        assert!(response.contains("clud-claude-codex-terra"), "{response}");
        assert!(!response.contains("clud knows the model"), "{response}");
        assert!(
            upstream.requests().is_empty(),
            "a rejected selection must not reach upstream"
        );
    }

    /// #1529: the advisor capability is paired (tools[] entry + anthropic-beta
    /// value). On a compat route both halves must leave together, and nothing
    /// else in the request may change.
    #[test]
    fn advisor_capability_halves_are_stripped_together() {
        let body = serde_json::json!({
            "model": "deepseek/deepseek-v4.1-flash",
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}},
                {"type": "advisor_20260301", "name": "advisor", "model": "claude-opus-4-8"}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let headers = vec![
            ("x-api-key".to_string(), "k".to_string()),
            (
                "anthropic-beta".to_string(),
                "oauth-2025-04-20, advisor-tool-2026-03-01,fixture-capability".to_string(),
            ),
        ];
        let (out_body, out_headers) =
            super::strip_advisor_capability(&serde_json::to_vec(&body).unwrap(), &headers);
        let out: serde_json::Value = serde_json::from_slice(&out_body).unwrap();
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "Read");
        assert_eq!(out["messages"], body["messages"]);
        assert_eq!(
            out_headers,
            vec![
                ("x-api-key".to_string(), "k".to_string()),
                (
                    "anthropic-beta".to_string(),
                    "oauth-2025-04-20,fixture-capability".to_string()
                ),
            ]
        );

        // Advisor-only tools and beta: both vanish entirely, never half.
        let body = serde_json::json!({
            "tools": [{"type": "advisor_20260301", "name": "advisor"}],
            "tool_choice": {"type": "auto"}
        });
        let headers = vec![(
            "anthropic-beta".to_string(),
            "advisor-tool-2026-03-01".to_string(),
        )];
        let (out_body, out_headers) =
            super::strip_advisor_capability(&serde_json::to_vec(&body).unwrap(), &headers);
        let out: serde_json::Value = serde_json::from_slice(&out_body).unwrap();
        assert!(out.get("tools").is_none());
        assert!(out.get("tool_choice").is_none());
        assert!(out_headers.is_empty());

        // A request without the advisor is forwarded byte-identical.
        let raw = br#"{"model":"m","tools":[{"name":"Read"}]}"#;
        let (out_body, _) = super::strip_advisor_capability(raw, &[]);
        assert_eq!(out_body, raw.to_vec());
    }
}

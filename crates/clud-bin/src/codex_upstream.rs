//! Upstream Responses client for the Codex bridge (issue #627 phase 3, step 4).
//!
//! Two responsibilities, kept apart on purpose:
//!
//! - [`CredentialSource`] yields the base URL, auth material, account headers,
//!   and model policy. Putting this behind a trait is what lets #629's
//!   subscription auth reuse every line of translation without touching it.
//! - [`UpstreamClient`] performs one streaming `POST /v1/responses`, handing
//!   each byte chunk to a sink as it arrives.
//!
//! ## The retry boundary
//!
//! The issue's rule is "never replay a request after downstream-visible output
//! has begun", and step 1 is why it is absolute: once `write_event_stream` has
//! flushed a single frame, the 200 and its headers are already on the wire, so
//! there is no status code left to change and a replay would duplicate content
//! the user has already seen. The client therefore tracks whether the sink has
//! accepted anything, and refuses to retry once it has — regardless of how
//! retryable the failure looks.
//!
//! ## Secrets
//!
//! No client header is forwarded upstream. The allowlist is empty by
//! construction: this module builds every outbound header itself, so the
//! harness's own Anthropic bearer cannot leak into an upstream request. In the
//! other direction, transport failures are classified into fixed strings
//! rather than carrying the library's message, because those messages embed
//! the URL and would put the endpoint into logs.

use std::error::Error as _;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::codex_auth::{self, SubscriptionCredentials};
use crate::codex_model::ModelSpec;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";
/// ChatGPT-subscription auth is a *different backend*, not just a different
/// token (`openai/codex` `model-provider-info`). Endpoint, system-prompt
/// placement, and required headers all change with it.
pub const CODEX_BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Identifies the client to the Codex backend.
pub const CODEX_ORIGINATOR: &str = "codex_cli_rs";
/// Codex release verified against GPT-6 Sol on the ChatGPT backend.
///
/// Keep this separate from clud's package version: the backend interprets it
/// as a Codex compatibility version. See the request-header regression test.
pub const CODEX_CLIENT_VERSION: &str = "0.157.1";
const CODEX_BETA_HEADER_VALUE: &str = "responses=experimental";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle timeout between reads, not a cap on the whole turn: a model may think
/// for minutes before its first token, which is healthy, whereas a socket that
/// yields nothing for this long is not.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_OVERALL_TIMEOUT: Duration = Duration::from_secs(3600);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Retry budget for a failure we positively recognise as transient.
const DEFAULT_MAX_ATTEMPTS: u32 = 4;
/// Retry budget for a 5xx whose body matches neither the permanent nor the
/// transient signatures. Deliberately smaller than the transient budget: #764
/// documents how treating every unrecognised 5xx as fully retryable is what
/// turns one bad request into a credential-burning cascade upstream.
const DEFAULT_UNKNOWN_MAX_ATTEMPTS: u32 = 2;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(500);
/// Ceiling on any single backoff sleep, including one derived from
/// `Retry-After`: a generous server hint must not pin a turn open.
const DEFAULT_MAX_RETRY_DELAY: Duration = Duration::from_secs(8);
/// Ceiling on time spent sleeping between attempts, across the whole request.
const DEFAULT_MAX_RETRY_ELAPSED: Duration = Duration::from_secs(30);
/// How much of an error body is read before classification. Enough for a JSON
/// error envelope, small enough to bound allocation on a hostile response.
const ERROR_BODY_PREFIX_LIMIT: usize = 8 * 1024;
/// Treat a login this close to its expiry as already expired, so a turn cannot
/// start on a token that dies mid-flight.
const TOKEN_EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// The one credential failure a user can fix without reading a log.
///
/// Shared rather than duplicated because [`crate::codex_pipeline`] matches on
/// it to decide that this reason — unlike the others, which name environment
/// variables — is safe and useful to forward to the harness.
pub const CREDENTIALS_EXPIRED: &str = "the Codex login has expired -- run `codex login`";
/// Expiry guidance for clud's separately-managed subscription credential.
pub const CLUD_CREDENTIALS_EXPIRED: &str =
    "the Codex login has expired -- run `clud auth login codex`";

/// Whether a failure could ever succeed on a fresh attempt.
///
/// This is the distinction the retry loop was missing (#764): upstream returns
/// *permanent* rejections wearing a 5xx costume — a model that needs a newer
/// client, an unsupported parameter — and retrying those can never succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Retrying cannot help. Fail on the first attempt.
    Permanent,
    /// Recognised as retryable: transport, 408/429, or a 5xx whose body reads
    /// like an outage.
    Transient,
    /// A 5xx we do not recognise. Retried, but on a reduced budget.
    Unknown,
    /// The account is out of quota or credits. Retrying is not merely
    /// unhelpful, it is actively wrong: a multi-day exhaustion would burn the
    /// whole budget in under a second and report nothing the user can act on.
    /// Split out of [`Self::Permanent`] because it is the one failure with a
    /// *reset time* and a remedy, and the client-facing message says so.
    Exhausted,
    /// The route's own *configuration* makes this request unacceptable, and no
    /// amount of retrying changes that: a gateway guardrail excluded the only
    /// endpoint that could have served it, so a second attempt names the same
    /// excluded endpoint.
    ///
    /// Split out of [`Self::Permanent`] because — unlike a malformed request,
    /// which fails identically on every route — these bytes are fine and only
    /// *this* route refuses them. That makes it a legitimate failover rather
    /// than a second account spent reproducing an error.
    PolicyRejected,
}

/// A non-2xx upstream response, reduced to what is safe to keep.
///
/// The raw body is **never** retained. It is read to a bounded prefix, used to
/// classify the failure, mined for a scrubbed one-line detail, and dropped. The
/// previous code discarded the whole response unread, which left an operator
/// unable to tell a Cloudflare edge blip from a hard rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamFailure {
    status: u16,
    class: FailureClass,
    /// Exact provider signal that the request exceeded the model context.
    /// This is deliberately a boolean rather than retained body/code text:
    /// control flow may use the allowlisted value without carrying upstream
    /// payloads into logs or errors.
    context_length_exceeded: bool,
    /// `x-request-id`. Opaque correlation handle, safe to log and to name in a
    /// client-facing message.
    request_id: Option<String>,
    /// `cf-ray`. Its presence is what identifies a failure as edge-side rather
    /// than application-side.
    cf_ray: Option<String>,
    /// `Retry-After`, already clamped to the configured ceiling.
    retry_after: Option<Duration>,
    /// `resets_in_seconds` / `resets_at` from a `usage_limit_reached` body.
    resets_in: Option<Duration>,
    /// Scrubbed reason for logs. Token-shaped runs are redacted, and this never
    /// travels downstream.
    detail: Option<String>,
}

impl UpstreamFailure {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn class(&self) -> FailureClass {
        self.class
    }

    pub fn is_context_length_exceeded(&self) -> bool {
        self.context_length_exceeded
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub fn cf_ray(&self) -> Option<&str> {
        self.cf_ray.as_deref()
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub fn resets_in(&self) -> Option<Duration> {
        self.resets_in
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// Attempt budget this failure earns.
    fn max_attempts(&self, config: &UpstreamConfig) -> u32 {
        match self.class {
            FailureClass::Permanent | FailureClass::Exhausted | FailureClass::PolicyRejected => 1,
            FailureClass::Transient => config.max_attempts.max(1),
            FailureClass::Unknown => config.unknown_max_attempts.max(1),
        }
    }

    /// One line for the operator log. Safe by construction: every field here is
    /// either a status, an opaque id, or already scrubbed.
    pub fn diagnostic(&self) -> String {
        let mut parts = vec![
            format!("status={}", self.status),
            format!("class={:?}", self.class),
        ];
        if let Some(id) = &self.request_id {
            parts.push(format!("request-id={id}"));
        }
        if let Some(ray) = &self.cf_ray {
            parts.push(format!("cf-ray={ray}"));
        }
        if let Some(after) = self.retry_after {
            parts.push(format!("retry-after={}s", after.as_secs()));
        }
        if let Some(resets) = self.resets_in {
            parts.push(format!("resets-in={}s", resets.as_secs()));
        }
        if let Some(detail) = &self.detail {
            parts.push(format!("detail={detail:?}"));
        }
        parts.join(" ")
    }

    /// Build from a status plus the response's headers and body prefix.
    ///
    /// Split from the transport so classification is testable without a socket.
    pub fn from_parts(
        status: u16,
        header: impl Fn(&str) -> Option<String>,
        body_prefix: &str,
        max_retry_delay: Duration,
    ) -> Self {
        let request_id = header("x-request-id").filter(|value| !value.trim().is_empty());
        let cf_ray = header("cf-ray").filter(|value| !value.trim().is_empty());
        let retry_after = header("retry-after")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .map(|value| value.min(max_retry_delay));
        let resets_in = parse_resets_in(body_prefix);
        let detail = error_detail(body_prefix);
        let class = classify(status, body_prefix);
        let context_length_exceeded = context_length_exceeded(body_prefix);
        Self {
            status,
            class,
            context_length_exceeded,
            request_id,
            cf_ray,
            retry_after,
            resets_in,
            detail,
        }
    }
}

fn context_length_exceeded(body_prefix: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body_prefix) else {
        return false;
    };
    ["/error/code", "/code"].into_iter().any(|pointer| {
        value.pointer(pointer).and_then(serde_json::Value::as_str)
            == Some("context_length_exceeded")
    })
}

/// Body signatures that mean "this request will never succeed as written".
///
/// Kept as substrings of the lowercased body rather than a JSON shape, because
/// the same rejection arrives as a 400, a 422 or a 502 depending on which hop
/// produced it.
const PERMANENT_SIGNATURES: &[&str] = &[
    "requires a newer version",
    "unsupported_value",
    "unsupported_parameter",
    "unsupported_country_region_territory",
    "invalid_request_error",
    "unknown provider",
    "model_not_found",
    "does not exist",
    "invalid_api_key",
];

/// Body signatures that mark an account as out of quota or credits.
///
/// Checked before every other rule, including the 408/429-are-transient rule:
/// the ChatGPT backend reports exhaustion as a 429, which is indistinguishable
/// from an ordinary throttle by status alone. That ambiguity is why a real
/// exhaustion was retried three times in ~750ms and reported as a bare
/// "status 429".
const EXHAUSTED_SIGNATURES: &[&str] = &[
    "insufficient_quota",
    "usage_limit_reached",
    "quota_exceeded",
    "out of credits",
    "credit balance",
    "billing_hard_limit_reached",
    // A period-scoped allowance reads as an ordinary throttle by status and by
    // code; only the period word separates "slow down" from "come back
    // tomorrow" (#968).
    "free-models-per-day",
    "daily limit",
    "daily quota",
    "requires more credits",
];

/// Body signatures that mean "this *route's configuration* refuses this
/// request", as opposed to the request itself being malformed.
///
/// OpenRouter-shaped and deliberately loose, because the exclusion reason is
/// free prose assembled by the gateway rather than a stable error code. Checked
/// before [`PERMANENT_SIGNATURES`] so that a guardrail rejection is never read
/// as a generic request rejection.
const POLICY_REJECTED_SIGNATURES: &[&str] = &[
    // "0 endpoints out of 1 requested are available matching your guardrail
    // restrictions and data policy".
    "guardrail",
    "zdr",
    "zero data retention",
    "data policy",
    "endpoints out of",
    "no endpoints available",
];

/// Body signatures that positively mark a 5xx as an outage rather than a
/// rejection. Anything else at 5xx is [`FailureClass::Unknown`].
const TRANSIENT_SIGNATURES: &[&str] = &[
    "bad gateway",
    "gateway timeout",
    "service unavailable",
    "temporarily unavailable",
    "overloaded",
    "try again",
    "circuit_open",
    "circuit open",
    "cloudflare",
    "internal server error",
    "server_error",
    "timeout",
];

/// Statuses the bridge passes through untouched; retrying them cannot help.
const NON_RETRYABLE_STATUSES: &[u16] = &[400, 401, 403, 404, 405, 409, 413, 422];

fn classify(status: u16, body_prefix: &str) -> FailureClass {
    let body = body_prefix.to_ascii_lowercase();
    if EXHAUSTED_SIGNATURES
        .iter()
        .any(|signature| body.contains(signature))
    {
        return FailureClass::Exhausted;
    }
    // Before the permanent table and the status list: a policy rejection wears
    // a 400 exactly like a malformed request, but only this route can serve it
    // differently, so the status alone would strand a conversation that another
    // route could have answered.
    if POLICY_REJECTED_SIGNATURES
        .iter()
        .any(|signature| body.contains(signature))
    {
        return FailureClass::PolicyRejected;
    }
    if PERMANENT_SIGNATURES
        .iter()
        .any(|signature| body.contains(signature))
    {
        return FailureClass::Permanent;
    }
    if NON_RETRYABLE_STATUSES.contains(&status) {
        return FailureClass::Permanent;
    }
    // 408 and 429 are retryable by definition; 429 additionally carries a reset
    // hint that the retry loop honours.
    if status == 408 || status == 429 {
        return FailureClass::Transient;
    }
    if status >= 500 {
        if body.trim().is_empty()
            || TRANSIENT_SIGNATURES
                .iter()
                .any(|signature| body.contains(signature))
        {
            return FailureClass::Transient;
        }
        return FailureClass::Unknown;
    }
    FailureClass::Permanent
}

/// Pull `resets_in_seconds` (or `resets_at`, as a delta) out of a rate-limit
/// body. Unparsed until CLIProxyAPI#1666; clud made the same omission.
fn parse_resets_in(body_prefix: &str) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body_prefix).ok()?;
    for pointer in [
        "/error/resets_in_seconds",
        "/resets_in_seconds",
        "/error/resets_in",
        "/resets_in",
    ] {
        if let Some(seconds) = value.pointer(pointer).and_then(serde_json::Value::as_u64) {
            return Some(Duration::from_secs(seconds));
        }
    }
    None
}

/// A short, scrubbed reason for the operator log.
///
/// Prefers the enum-like `code`/`type` fields. `message` is free text written
/// by upstream and can quote account identifiers or key fragments, so it is
/// passed through [`scrub`] before it is kept, and it never leaves the log.
fn error_detail(body_prefix: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body_prefix).ok()?;
    let at = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let code = at("/error/code")
        .or_else(|| at("/error/type"))
        .or_else(|| at("/code"));
    let message = at("/error/message")
        .or_else(|| at("/message"))
        .or_else(|| at("/detail"));
    let detail = match (code, message) {
        (Some(code), Some(message)) => format!("{code}: {}", scrub(&message)),
        (Some(code), None) => code,
        (None, Some(message)) => scrub(&message),
        (None, None) => return None,
    };
    Some(truncate_chars(&detail, 200))
}

/// Redact token-shaped runs from free text.
///
/// Deliberately blunt: a long unbroken alphanumeric run in an error message is
/// far more likely to be a key, an org id or an account id than a word worth
/// keeping.
fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 20 || run.starts_with("sk-") || run.starts_with("org_") {
            out.push_str("[redacted]");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for character in text.chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            run.push(character);
        } else {
            flush(&mut run, &mut out);
            out.push(character);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "..."
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    /// No usable credentials. Deliberately never names the variable's value.
    #[error("upstream credentials unavailable: {0}")]
    Credentials(&'static str),
    /// Upstream returned a non-2xx status, reduced to a classified summary.
    /// The raw body is never carried: it can contain account identifiers and
    /// key fragments.
    #[error("upstream returned status {}", .0.status)]
    Status(UpstreamFailure),
    /// Classified transport failure. A fixed string, never the library's
    /// message, which embeds the URL.
    #[error("upstream transport failure: {0}")]
    Transport(&'static str),
    /// The compact endpoint returned a JSON body that does not match its typed contract.
    #[error("upstream compact response was malformed")]
    CompactMalformed,
    /// The response exceeded the configured byte budget.
    #[error("upstream response exceeded the size budget")]
    TooLarge,
    /// The overall deadline elapsed.
    #[error("upstream request timed out")]
    Timeout,
    /// The caller asked to stop.
    #[error("upstream request cancelled")]
    Cancelled,
    /// The sink rejected a chunk — normally the downstream client hung up.
    #[error("downstream sink failed: {0}")]
    Downstream(&'static str),
}

impl UpstreamError {
    /// Whether this is the provider's exact context-window signal. Status
    /// codes and free-form messages are intentionally not accepted here.
    pub fn is_context_length_exceeded(&self) -> bool {
        matches!(self, Self::Status(failure) if failure.is_context_length_exceeded())
    }

    /// Whether this failure could plausibly succeed on a fresh attempt.
    ///
    /// Note this is only half the decision: [`UpstreamClient::stream`] also
    /// requires that nothing has reached the sink yet, and consults
    /// [`UpstreamError::max_attempts`] for how much budget the failure earns.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Timeout => true,
            Self::Status(failure) => !matches!(
                failure.class,
                FailureClass::Permanent | FailureClass::Exhausted | FailureClass::PolicyRejected
            ),
            Self::Credentials(_)
            | Self::CompactMalformed
            | Self::TooLarge
            | Self::Cancelled
            | Self::Downstream(_) => false,
        }
    }

    /// Total attempts this failure is worth, including the one already spent.
    ///
    /// A transport failure gets the full transient budget; a classified status
    /// gets whatever its class earns.
    pub fn max_attempts(&self, config: &UpstreamConfig) -> u32 {
        match self {
            Self::Transport(_) | Self::Timeout => config.max_attempts.max(1),
            Self::Status(failure) => failure.max_attempts(config),
            _ => 1,
        }
    }

    /// A server-supplied hint about when to try again, already clamped.
    pub fn retry_hint(&self) -> Option<Duration> {
        match self {
            Self::Status(failure) => failure.retry_after,
            _ => None,
        }
    }

    /// The classified summary, when this is a status failure.
    pub fn failure(&self) -> Option<&UpstreamFailure> {
        match self {
            Self::Status(failure) => Some(failure),
            _ => None,
        }
    }
}

/// Everything needed to address the upstream, resolved per request.
#[derive(Clone)]
pub struct UpstreamTarget {
    base_url: String,
    authorization: String,
    extra_headers: Vec<(String, String)>,
    model_override: Option<ModelSpec>,
    account_id: Option<String>,
    prompt_cache_key: Option<String>,
}

impl UpstreamTarget {
    pub fn new(base_url: impl Into<String>, authorization: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            authorization: authorization.into(),
            extra_headers: Vec::new(),
            model_override: None,
            account_id: None,
            prompt_cache_key: None,
        }
    }

    /// ChatGPT account id. Sent as `ChatGPT-Account-ID`, and its presence is
    /// what marks a target as the Codex backend.
    pub fn with_account_id(mut self, account_id: Option<String>) -> Self {
        self.account_id = account_id;
        self
    }

    /// Buys prompt-cache hits across turns; omitting it pays full input price.
    pub fn with_prompt_cache_key(mut self, key: Option<String>) -> Self {
        self.prompt_cache_key = key;
        self
    }

    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }

    /// Whether this target is the ChatGPT Codex backend rather than the
    /// platform API. Drives system-prompt placement (#750 A3).
    pub fn uses_codex_backend(&self) -> bool {
        self.base_url.contains("chatgpt.com")
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    pub fn with_model_override(mut self, model: Option<ModelSpec>) -> Self {
        self.model_override = model;
        self
    }

    pub fn model_override(&self) -> Option<&ModelSpec> {
        self.model_override.as_ref()
    }

    /// Absolute URL of the Responses endpoint.
    ///
    /// A base that already names the endpoint is left alone, so an operator
    /// can point at a gateway that exposes it at a fixed path.
    pub fn responses_url(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        if trimmed.ends_with("/responses") {
            trimmed.to_string()
        } else if trimmed.ends_with("/v1") || self.uses_codex_backend() {
            // The Codex backend exposes `/responses` directly under its base;
            // it has no `/v1` segment.
            format!("{trimmed}/responses")
        } else {
            format!("{trimmed}/v1/responses")
        }
    }

    /// Absolute URL of the typed Responses compaction endpoint.
    pub fn compact_url(&self) -> String {
        format!("{}/compact", self.responses_url())
    }

    /// Absolute URL of the OpenAI images generation endpoint on this target.
    ///
    /// The Codex backend exposes `/images/generations` beside `/responses`
    /// under the same base (`codexDirectImagesGenerations` in upstream
    /// `codex_openai_images.go:47`); an OpenAI-compatible base keeps its `/v1`
    /// segment, exactly as [`Self::responses_url`] does.
    pub fn images_url(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        if trimmed.ends_with("/images/generations") {
            trimmed.to_string()
        } else if self.uses_codex_backend() {
            format!("{trimmed}/images/generations")
        } else {
            format!("{trimmed}/v1/images/generations")
        }
    }
}

/// Neither the bearer nor the endpoint appears here: error snapshots and log
/// lines must not become a credential map.
impl std::fmt::Debug for UpstreamTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamTarget")
            .field("base_url", &"[redacted]")
            .field("authorization", &"[redacted]")
            .field("extra_headers", &self.extra_headers.len())
            .field("model_override", &self.model_override)
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

/// Where upstream credentials come from.
///
/// #629 adds a subscription-backed implementation; because translation only
/// ever sees this trait, none of it has to change when that lands.
pub trait CredentialSource: Send + Sync {
    fn resolve(&self) -> Result<UpstreamTarget, UpstreamError>;

    /// A rejected subscription bearer may be refreshed before the response is
    /// visible. API keys and other sources have no recovery action.
    fn recover_after_unauthorized(
        &self,
        _rejected: &UpstreamTarget,
    ) -> Result<Option<UpstreamTarget>, UpstreamError> {
        Ok(None)
    }
}

#[path = "codex_upstream_credentials.rs"]
mod codex_upstream_credentials;
#[cfg(test)]
use codex_upstream_credentials::{base64url_decode, resolve_api_key_target};
use codex_upstream_credentials::{new_session_id, RetryObserver};
pub use codex_upstream_credentials::{
    ApiKeyCredentials, CludSubscriptionCredentials, CodexBridgeCredentialError,
    CodexCliCredentials, ResolvedCredentials, StreamOutcome, UpstreamConfig,
};
pub struct UpstreamClient<C: CredentialSource> {
    credentials: C,
    config: UpstreamConfig,
    /// Stable upstream cache/session identity.
    session_id: String,
    retry_observer: Option<RetryObserver>,
    shutdown_sockets: Option<std::sync::Arc<crate::serve_transport::UpstreamSockets>>,
    /// The bearer actually sent on the last attempt may differ from the
    /// source's initial bearer after a guarded 401 refresh.
    last_target: Mutex<Option<UpstreamTarget>>,
    redaction: Option<crate::secret_redaction::stream::SharedSession>,
}

impl<C: CredentialSource> std::fmt::Debug for UpstreamClient<C> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamClient")
            .field("config", &self.config)
            .finish()
    }
}

/// The upstream's answer to an images request.
///
/// The status is kept beside the body because this door forwards the upstream
/// envelope verbatim: an images client parses `{"error": {...}}` from a 4xx
/// exactly as it parses `{"data": [...]}` from a 200, so re-encoding it here
/// could only lose fields clud does not know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagesReply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl<C: CredentialSource> UpstreamClient<C> {
    pub fn new(credentials: C, config: UpstreamConfig) -> Self {
        Self {
            credentials,
            config,
            session_id: new_session_id(),
            retry_observer: None,
            shutdown_sockets: None,
            last_target: Mutex::new(None),
            redaction: None,
        }
    }

    /// Track standalone HTTPS connections so shutdown interrupts blocked reads.
    pub(crate) fn with_shutdown_sockets(
        mut self,
        sockets: std::sync::Arc<crate::serve_transport::UpstreamSockets>,
    ) -> Self {
        self.shutdown_sockets = Some(sockets);
        self
    }

    fn agent(&self) -> ureq::Agent {
        let builder = ureq::AgentBuilder::new()
            .timeout_connect(self.config.connect_timeout)
            .timeout_read(self.config.read_timeout);
        match &self.shutdown_sockets {
            Some(sockets) => crate::serve_transport::tracked_agent(builder, sockets),
            None => builder.build(),
        }
    }

    pub(crate) fn credential_target(&self) -> Result<UpstreamTarget, UpstreamError> {
        if let Some(target) = self
            .last_target
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
        {
            return Ok(target);
        }
        self.credentials.resolve()
    }

    pub fn with_retry_observer(
        mut self,
        observer: impl Fn(&UpstreamError, u32, u32, Option<Duration>) + Send + Sync + 'static,
    ) -> Self {
        self.retry_observer = Some(std::sync::Arc::new(observer));
        self
    }

    /// Override the generated client identity when a caller owns a longer
    /// conversation lifetime. The bridge uses its hashed conversation key so
    /// sequential turns retain prompt-cache locality without exposing the
    /// harness's raw session or agent identifiers upstream.
    pub fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = session_id;
        self
    }

    pub fn with_redaction(
        mut self,
        session: Option<crate::secret_redaction::stream::SharedSession>,
    ) -> Self {
        self.redaction = session;
        self
    }

    fn redacted_body<'a>(
        &self,
        body: &'a [u8],
        target: &UpstreamTarget,
    ) -> Result<std::borrow::Cow<'a, [u8]>, UpstreamError> {
        let Some(session) = &self.redaction else {
            return Ok(std::borrow::Cow::Borrowed(body));
        };
        let mut session = session
            .lock()
            .map_err(|_| UpstreamError::Transport("secret redaction unavailable"))?;
        session.add_known(
            target
                .authorization
                .strip_prefix("Bearer ")
                .unwrap_or(&target.authorization),
        );
        session
            .redact_body(body)
            .map_err(|_| UpstreamError::Transport("invalid request for secret redaction"))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn credentials(&self) -> &C {
        &self.credentials
    }

    /// Send a typed non-streaming compaction request with the same credential
    /// source and header allowlist as an inference request.
    pub fn compact(
        &self,
        body: &[u8],
        cancel: &AtomicBool,
    ) -> Result<crate::codex_translate::CompactResponse, UpstreamError> {
        if cancel.load(Ordering::Acquire) {
            return Err(UpstreamError::Cancelled);
        }
        let target = self.credentials.resolve()?;
        let body = self.redacted_body(body, &target)?;
        let agent = self.agent();
        let mut request = agent
            .post(&target.compact_url())
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .set("Authorization", &target.authorization)
            .set("originator", CODEX_ORIGINATOR)
            .set("version", CODEX_CLIENT_VERSION)
            .set("session-id", &self.session_id)
            .set("thread-id", &self.session_id)
            .set("x-client-request-id", &self.session_id)
            .set(
                "User-Agent",
                &format!("{CODEX_ORIGINATOR}/{CODEX_CLIENT_VERSION} (clud)"),
            );
        if let Some(account_id) = target.account_id.as_deref() {
            request = request.set("ChatGPT-Account-ID", account_id);
        }
        for (name, value) in &target.extra_headers {
            request = request.set(name, value);
        }
        let response = match request.send_bytes(&body) {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                return Err(UpstreamError::Status(capture_failure(
                    status,
                    response,
                    self.config.max_retry_delay,
                )));
            }
            Err(ureq::Error::Transport(_)) => {
                return Err(UpstreamError::Transport("connection failed"));
            }
        };
        let mut body = Vec::new();
        let mut reader = response
            .into_reader()
            .take(self.config.max_response_bytes.saturating_add(1) as u64);
        reader
            .read_to_end(&mut body)
            .map_err(|_| UpstreamError::Transport("read failed"))?;
        if body.len() > self.config.max_response_bytes {
            return Err(UpstreamError::TooLarge);
        }
        serde_json::from_slice(&body).map_err(|_| UpstreamError::CompactMalformed)
    }

    /// Stream one Responses request, handing each chunk to `sink` as it lands.
    ///
    /// `cancel` is polled between reads. A blocking read cannot be interrupted,
    /// so cancellation latency is bounded by `read_timeout` rather than being
    /// immediate; that is the cost of not putting an async runtime behind a
    /// synchronous bridge.
    pub fn stream(
        &self,
        body: &[u8],
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), UpstreamError>,
    ) -> Result<StreamOutcome, UpstreamError> {
        let delivered = AtomicBool::new(false);
        let mut legacy_sink = |chunk: &[u8]| {
            // Preserve the legacy raw-byte commit boundary for direct callers.
            // The pipeline uses `stream_with_commit` to mark only frames it
            // has actually made downstream-visible.
            delivered.store(true, Ordering::Release);
            sink(chunk)
        };
        self.stream_with_commit(body, cancel, &mut legacy_sink, &delivered)
    }

    pub(crate) fn stream_with_commit(
        &self,
        body: &[u8],
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), UpstreamError>,
        delivered: &AtomicBool,
    ) -> Result<StreamOutcome, UpstreamError> {
        let mut target = self.credentials.resolve()?;
        let deadline = Instant::now() + self.config.overall_timeout;
        let mut attempt = 0_u32;
        let mut slept = Duration::ZERO;
        let mut auth_retried = false;

        loop {
            attempt += 1;
            if cancel.load(Ordering::Acquire) {
                return Err(UpstreamError::Cancelled);
            }
            *self
                .last_target
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(target.clone());
            match self.attempt(&target, body, cancel, deadline, sink, delivered) {
                Ok(bytes) => {
                    return Ok(StreamOutcome {
                        attempts: attempt,
                        bytes,
                    })
                }
                Err(error) => {
                    if !auth_retried
                        && !delivered.load(Ordering::Acquire)
                        && !cancel.load(Ordering::Acquire)
                        && Instant::now() < deadline
                        && error
                            .failure()
                            .is_some_and(|failure| failure.status() == 401)
                    {
                        if let Some(recovered) =
                            self.credentials.recover_after_unauthorized(&target)?
                        {
                            auth_retried = true;
                            target = recovered;
                            continue;
                        }
                    }
                    // The absolute rule, unchanged (DD-029): once anything has
                    // reached the sink the response is committed, however
                    // retryable the failure looks. Everything below only ever
                    // widens the *pre-commit* window.
                    let budget = error.max_attempts(&self.config);
                    let retry_allowed = !delivered.load(Ordering::Acquire)
                        && error.is_retryable()
                        && attempt < budget
                        && Instant::now() < deadline;
                    if !retry_allowed {
                        if let Some(observer) = &self.retry_observer {
                            observer(&error, attempt, budget, None);
                        }
                        return Err(error);
                    }
                    // A server hint wins over our own guess, but is clamped by
                    // the same ceiling so it cannot pin the turn open.
                    let backoff = match error.retry_hint() {
                        Some(hint) => hint.min(self.config.max_retry_delay),
                        None => backoff_delay(attempt, &self.config, jitter_fraction()),
                    };
                    if slept + backoff > self.config.max_retry_elapsed {
                        if let Some(observer) = &self.retry_observer {
                            observer(&error, attempt, budget, None);
                        }
                        return Err(error);
                    }
                    if let Some(observer) = &self.retry_observer {
                        observer(&error, attempt, budget, Some(backoff));
                    }
                    slept += backoff;
                    if wait_or_cancel(backoff, cancel) {
                        return Err(UpstreamError::Cancelled);
                    }
                }
            }
        }
    }

    /// One upstream request carrying clud's full identity header set.
    ///
    /// Every header is constructed here and nothing from the downstream request
    /// is copied, so the harness's Anthropic bearer cannot travel upstream.
    /// `session-id`/`thread-id` are hyphenated upstream, not underscored.
    fn identity_request(&self, target: &UpstreamTarget, url: &str, accept: &str) -> ureq::Request {
        let mut request = self
            .agent()
            .post(url)
            .set("Content-Type", "application/json")
            .set("Accept", accept)
            .set("Authorization", &target.authorization)
            .set("originator", CODEX_ORIGINATOR)
            .set("OpenAI-Beta", CODEX_BETA_HEADER_VALUE)
            .set("version", CODEX_CLIENT_VERSION)
            .set("session-id", &self.session_id)
            .set("thread-id", &self.session_id)
            .set("x-client-request-id", &self.session_id)
            .set(
                "User-Agent",
                &format!("{CODEX_ORIGINATOR}/{CODEX_CLIENT_VERSION} (clud)"),
            );
        if let Some(account_id) = target.account_id.as_deref() {
            request = request.set("ChatGPT-Account-ID", account_id);
        }
        for (name, value) in &target.extra_headers {
            request = request.set(name, value);
        }
        request
    }

    /// POST a prepared images body and return the upstream's answer.
    ///
    /// Deliberately ONE attempt with no retry: a retried generation is a second
    /// billed image the caller never asked for, so a transport failure is
    /// surfaced instead of silently re-spending. A non-2xx answer is returned
    /// as a reply, not an error, because the upstream's own error envelope is
    /// the contract the client parses.
    pub fn images(&self, body: &[u8], cancel: &AtomicBool) -> Result<ImagesReply, UpstreamError> {
        /// Base64 images are large but finite; the cap keeps a hostile or
        /// broken upstream from growing the bridge's heap without bound.
        const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

        if cancel.load(Ordering::Acquire) {
            return Err(UpstreamError::Cancelled);
        }
        let target = self.credentials.resolve()?;
        *self
            .last_target
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(target.clone());
        let request = self
            .identity_request(&target, &target.images_url(), "application/json")
            .timeout(self.config.overall_timeout);
        let read_reply =
            |reader: &mut dyn Read, status: u16| -> Result<ImagesReply, UpstreamError> {
                let mut buffer = Vec::new();
                reader
                    .take(MAX_RESPONSE_BYTES)
                    .read_to_end(&mut buffer)
                    .map_err(|error| {
                        if error.kind() == std::io::ErrorKind::TimedOut {
                            UpstreamError::Timeout
                        } else {
                            UpstreamError::Transport("images response could not be read")
                        }
                    })?;
                Ok(ImagesReply {
                    status,
                    body: buffer,
                })
            };
        match request.send_bytes(body) {
            Ok(response) => {
                let status = response.status();
                let mut reader = response.into_reader();
                read_reply(&mut reader, status)
            }
            Err(ureq::Error::Status(status, response)) => {
                let mut reader = response.into_reader();
                read_reply(&mut reader, status)
            }
            Err(ureq::Error::Transport(error)) => Err(if is_timeout_transport(&error) {
                UpstreamError::Timeout
            } else {
                UpstreamError::Transport("connection failed")
            }),
        }
    }

    fn attempt(
        &self,
        target: &UpstreamTarget,
        body: &[u8],
        cancel: &AtomicBool,
        deadline: Instant,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), UpstreamError>,
        delivered: &AtomicBool,
    ) -> Result<usize, UpstreamError> {
        let body = self.redacted_body(body, target)?;
        let attempt_deadline = self
            .config
            .first_frame_timeout
            .map(|timeout| Instant::now() + timeout);

        // Every header is constructed in `identity_request`; nothing from the
        // downstream request is copied, so the harness's Anthropic bearer
        // cannot travel upstream. `session-id`/`thread-id` are hyphenated
        // upstream, not underscored.
        let request = self.identity_request(target, &target.responses_url(), "text/event-stream");

        let response = match match attempt_deadline {
            Some(deadline) => request
                .timeout(deadline.saturating_duration_since(Instant::now()))
                .send_bytes(&body),
            None => request.send_bytes(&body),
        } {
            Ok(response) => response,
            // The response is *read*, not discarded. Its headers carry the
            // `cf-ray` that distinguishes an edge failure from an application
            // one, and its body is what separates a permanent rejection from a
            // transient outage (#764). Neither survives past classification.
            Err(ureq::Error::Status(status, response)) => {
                return Err(UpstreamError::Status(capture_failure(
                    status,
                    response,
                    self.config.max_retry_delay,
                )));
            }
            Err(ureq::Error::Transport(error)) => {
                return Err(
                    if attempt_deadline.is_some_and(|at| Instant::now() >= at)
                        && is_timeout_transport(&error)
                    {
                        UpstreamError::Timeout
                    } else {
                        UpstreamError::Transport("connection failed")
                    },
                );
            }
        };

        let mut reader = response.into_reader();
        let first_frame_deadline = attempt_deadline;
        let mut buffer = [0_u8; 8192];
        let mut total = 0_usize;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(UpstreamError::Cancelled);
            }
            if Instant::now() >= deadline
                || (!delivered.load(Ordering::Acquire)
                    && first_frame_deadline.is_some_and(|at| Instant::now() >= at))
            {
                return Err(UpstreamError::Timeout);
            }
            let count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    return Err(
                        if !delivered.load(Ordering::Acquire)
                            && first_frame_deadline.is_some_and(|at| Instant::now() >= at)
                        {
                            UpstreamError::Timeout
                        } else {
                            UpstreamError::Transport("read timed out")
                        },
                    );
                }
                Err(_) => return Err(UpstreamError::Transport("read failed")),
            };
            total += count;
            if total > self.config.max_response_bytes {
                return Err(UpstreamError::TooLarge);
            }
            // The pipeline alone knows whether the chunk became a complete,
            // downstream-visible frame, so it owns the commit marker.
            sink(&buffer[..count])?;
        }
        Ok(total)
    }
}

/// Whether ureq wrapped an underlying socket timeout while parsing a response.
fn is_timeout_transport(error: &ureq::Transport) -> bool {
    error
        .source()
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some_and(|source| source.kind() == std::io::ErrorKind::TimedOut)
}

/// Reduce an error response to a classified summary, then drop it.
///
/// Reading the body is bounded by [`ERROR_BODY_PREFIX_LIMIT`]; a body that
/// cannot be read at all still yields a usable classification from the status.
fn capture_failure(
    status: u16,
    response: ureq::Response,
    max_retry_delay: Duration,
) -> UpstreamFailure {
    let header = |name: &str| response.header(name).map(str::to_string);
    let request_id = header("x-request-id");
    let cf_ray = header("cf-ray");
    let retry_after = header("retry-after");

    let mut body = Vec::new();
    let mut reader = response.into_reader().take(ERROR_BODY_PREFIX_LIMIT as u64);
    let _ = reader.read_to_end(&mut body);
    let body_prefix = String::from_utf8_lossy(&body);

    UpstreamFailure::from_parts(
        status,
        |name| match name {
            "x-request-id" => request_id.clone(),
            "cf-ray" => cf_ray.clone(),
            "retry-after" => retry_after.clone(),
            _ => None,
        },
        &body_prefix,
        max_retry_delay,
    )
}

/// Exponential backoff with jitter, clamped to the configured ceiling.
///
/// `jitter` is a fraction in `[0, 1)` supplied by the caller so the policy is a
/// pure function and can be tested without sampling randomness. Jitter matters
/// here: a fixed window makes every client retry in lockstep, which is how a
/// recovering upstream gets knocked straight back over.
fn backoff_delay(attempt: u32, config: &UpstreamConfig, jitter: f64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    let base = config
        .retry_delay
        .saturating_mul(1_u32 << exponent)
        .min(config.max_retry_delay);
    // Jitter spans the lower half of the window, so the delay stays within
    // `[base/2, base]` and can never exceed the ceiling.
    let span = base.mul_f64(0.5);
    let clamped = jitter.clamp(0.0, 1.0);
    base - span.mul_f64(clamped)
}

/// A jitter fraction in `[0, 1)`. Falls back to the midpoint if the OS entropy
/// source is unavailable — a fixed delay is worse than a jittered one, but far
/// better than failing the request over it.
fn jitter_fraction() -> f64 {
    let mut bytes = [0_u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
}

/// Sleep in short slices so cancellation is observed during backoff.
/// Returns true when cancelled.
fn wait_or_cancel(total: Duration, cancel: &AtomicBool) -> bool {
    let slice = Duration::from_millis(25);
    let mut waited = Duration::ZERO;
    while waited < total {
        if cancel.load(Ordering::Acquire) {
            return true;
        }
        let step = slice.min(total - waited);
        std::thread::sleep(step);
        waited += step;
    }
    cancel.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    /// A scripted local HTTP server. Each connection consumes one script entry
    /// and records the request it received.
    struct FakeUpstream {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        hits: Arc<AtomicUsize>,
        handle: Option<std::thread::JoinHandle<()>>,
        shutdown: Arc<AtomicBool>,
    }

    impl FakeUpstream {
        /// `script` yields one response body (raw HTTP) per connection; the
        /// last entry repeats once exhausted.
        fn start(script: Vec<Vec<u8>>) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let hits = Arc::new(AtomicUsize::new(0));
            let shutdown = Arc::new(AtomicBool::new(false));

            let thread_requests = Arc::clone(&requests);
            let thread_hits = Arc::clone(&hits);
            let thread_shutdown = Arc::clone(&shutdown);
            let handle = std::thread::spawn(move || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();

                    // Read headers, then exactly Content-Length bytes.
                    let mut raw = Vec::new();
                    let mut byte = [0_u8; 1];
                    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut byte) {
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
                    let mut body = vec![0_u8; length];
                    if length > 0 {
                        let _ = stream.read_exact(&mut body);
                    }
                    thread_requests
                        .lock()
                        .unwrap()
                        .push(format!("{head}{}", String::from_utf8_lossy(&body)));

                    let index = thread_hits.fetch_add(1, Ordering::AcqRel);
                    let reply = script
                        .get(index)
                        .or_else(|| script.last())
                        .cloned()
                        .unwrap_or_default();
                    let _ = stream.write_all(&reply);
                    let _ = stream.flush();
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            });

            Self {
                base_url: format!("http://{addr}"),
                requests,
                hits,
                handle: Some(handle),
                shutdown,
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::Acquire)
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for FakeUpstream {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn sse_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn status_response(status: u16) -> Vec<u8> {
        let body = r#"{"error":{"message":"key sk-secret-abc for org org_9"}}"#;
        format!(
            "HTTP/1.1 {status} Err\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn client(base_url: &str, config: UpstreamConfig) -> UpstreamClient<ApiKeyCredentials> {
        UpstreamClient::new(
            ApiKeyCredentials::new("sk-test-key", Some(base_url.to_string())).unwrap(),
            config,
        )
    }

    fn fast_config() -> UpstreamConfig {
        UpstreamConfig {
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(2),
            first_frame_timeout: None,
            overall_timeout: Duration::from_secs(10),
            retry_delay: Duration::from_millis(10),
            ..UpstreamConfig::default()
        }
    }

    #[test]
    fn resolves_the_endpoint_from_several_base_url_shapes() {
        let cases = [
            (
                "https://api.openai.com",
                "https://api.openai.com/v1/responses",
            ),
            (
                "https://api.openai.com/",
                "https://api.openai.com/v1/responses",
            ),
            ("https://gw.test/v1", "https://gw.test/v1/responses"),
            (
                "https://gw.test/v1/responses",
                "https://gw.test/v1/responses",
            ),
            (
                "https://gw.test/v1/responses/",
                "https://gw.test/v1/responses",
            ),
        ];
        for (base, expected) in cases {
            let target = UpstreamTarget::new(base, "Bearer x");
            assert_eq!(target.responses_url(), expected, "base {base}");
        }
    }

    #[test]
    fn compaction_url_follows_platform_and_subscription_routes() {
        assert_eq!(
            UpstreamTarget::new(DEFAULT_BASE_URL, "Bearer x").compact_url(),
            "https://api.openai.com/v1/responses/compact"
        );
        assert_eq!(
            UpstreamTarget::new(CODEX_BACKEND_BASE_URL, "Bearer x").compact_url(),
            "https://chatgpt.com/backend-api/codex/responses/compact"
        );
    }

    #[test]
    fn images_url_follows_platform_and_subscription_routes() {
        assert_eq!(
            UpstreamTarget::new(DEFAULT_BASE_URL, "Bearer x").images_url(),
            "https://api.openai.com/v1/images/generations"
        );
        assert_eq!(
            UpstreamTarget::new(CODEX_BACKEND_BASE_URL, "Bearer x").images_url(),
            "https://chatgpt.com/backend-api/codex/images/generations"
        );
        // A base that already names the endpoint is left alone, so an operator
        // can point at a gateway with a fixed path.
        assert_eq!(
            UpstreamTarget::new("https://gw.test/images/generations/", "Bearer x").images_url(),
            "https://gw.test/images/generations"
        );
    }

    #[test]
    fn redaction_at_the_send_boundary_covers_credentials_and_retries() {
        let server = FakeUpstream::start(vec![status_response(503), sse_response("data: ok\n\n")]);
        let shared = Arc::new(Mutex::new(
            crate::secret_redaction::reversible::Session::new(Vec::new()).unwrap(),
        ));
        let client =
            client(&server.base_url, fast_config()).with_redaction(Some(Arc::clone(&shared)));
        client
            .stream(
                br#"{"model":"m","input":[{"role":"user","content":"sk-test-key"}]}"#,
                &AtomicBool::new(false),
                &mut |_| Ok(()),
            )
            .unwrap();
        let token = shared
            .lock()
            .unwrap()
            .redact_text("sk-test-key")
            .into_owned();
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert!(request.contains("Authorization: Bearer sk-test-key"));
            let body = request.split_once("\r\n\r\n").unwrap().1;
            assert!(!body.contains("sk-test-key"));
            assert!(body.contains(&token));
        }
    }

    #[test]
    fn compaction_redacts_the_final_translated_history_body() {
        let reply = r#"{"output":[{"type":"compaction","encrypted_content":"summary"}]}"#;
        let server = FakeUpstream::start(vec![body_response(200, reply, &[])]);
        let shared = Arc::new(Mutex::new(
            crate::secret_redaction::reversible::Session::new(Vec::new()).unwrap(),
        ));
        let client = client(&server.base_url, fast_config()).with_redaction(Some(shared));
        client
            .compact(
                br#"{"model":"m","input":[{"role":"user","content":"sk-test-key"}]}"#,
                &AtomicBool::new(false),
            )
            .unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /v1/responses/compact "));
        let body = requests[0].split_once("\r\n\r\n").unwrap().1;
        assert!(!body.contains("sk-test-key"));
        assert!(body.contains("clud_secret_"));
    }

    #[test]
    fn compact_request_uses_the_responses_compact_route_and_json_accept_header() {
        let body = r#"{"output":[{"type":"compaction","encrypted_content":"summary"}]}"#;
        let server = FakeUpstream::start(vec![body_response(200, body, &[])]);
        let client = client(&server.base_url, fast_config());
        let compact = client
            .compact(
                br#"{"model":"gpt-5.6-terra","input":[],"parallel_tool_calls":true,"text":{}}"#,
                &AtomicBool::new(false),
            )
            .expect("compact response");
        assert!(matches!(
            compact.output.as_slice(),
            [crate::codex_translate::InputItem::Compaction { encrypted_content, .. }]
                if encrypted_content == "summary"
        ));
        let request = server.requests().remove(0);
        assert!(request.starts_with("POST /v1/responses/compact HTTP/1.1"));
        assert!(request.contains("Accept: application/json"));
        assert!(request.contains("version: 0.157.1"));
    }

    #[test]
    fn api_key_resolution_has_no_implicit_fallback() {
        // Present -> used, with the default base.
        let target = resolve_api_key_target(Some("  sk-abc  ".into()), None).unwrap();
        assert_eq!(
            target.responses_url(),
            "https://api.openai.com/v1/responses"
        );

        // Absent or blank -> a hard error, never a downgrade to another source.
        for missing in [None, Some(String::new()), Some("   ".into())] {
            let error =
                resolve_api_key_target(missing, None).expect_err("missing key must not resolve");
            assert_eq!(
                error,
                UpstreamError::Credentials("OPENAI_API_KEY is not set")
            );
        }

        let custom =
            resolve_api_key_target(Some("k".into()), Some("https://gw.test".into())).unwrap();
        assert_eq!(custom.responses_url(), "https://gw.test/v1/responses");
    }

    #[test]
    fn clud_subscription_is_a_distinct_codex_backend_source() {
        let credentials = CludSubscriptionCredentials::from_record(SubscriptionCredentials {
            access_token: "subscription-secret".to_string(),
            refresh_token: "refresh-secret".to_string(),
            account_id: Some("account-123".to_string()),
            email: None,
            expires_at_unix: None,
        })
        .unwrap();
        let target = credentials.resolve().unwrap();
        assert!(target.uses_codex_backend());
        assert!(target.responses_url().ends_with("/codex/responses"));
        assert!(!format!("{target:?}").contains("subscription-secret"));
        assert!(!format!("{credentials:?}").contains("refresh-secret"));
    }

    #[test]
    fn expired_subscription_never_falls_back_to_an_api_key() {
        let result = ResolvedCredentials::resolve_with(
            Err(UpstreamError::Credentials(CLUD_CREDENTIALS_EXPIRED)),
            || panic!("an expired subscription must not select OPENAI_API_KEY"),
        );
        assert!(matches!(
            result,
            Err(UpstreamError::Credentials(CLUD_CREDENTIALS_EXPIRED))
        ));
    }

    #[test]
    fn diagnostics_never_carry_the_key_or_endpoint() {
        let target = UpstreamTarget::new("https://secret.example/v1", "Bearer sk-secret-abc")
            .with_header("OpenAI-Beta", "responses=v1");
        let rendered = format!("{target:?}");
        assert!(!rendered.contains("sk-secret-abc"));
        assert!(!rendered.contains("secret.example"));

        let credentials = ApiKeyCredentials::new("sk-secret-abc", None).unwrap();
        assert!(!format!("{credentials:?}").contains("sk-secret-abc"));
        let client = UpstreamClient::new(credentials, UpstreamConfig::default());
        assert!(!format!("{client:?}").contains("sk-secret-abc"));

        // Errors carry a classification, not the library's message.
        assert!(
            !format!("{}", UpstreamError::Transport("connection failed"))
                .contains("secret.example")
        );
    }

    #[test]
    fn streams_chunks_to_the_sink_in_order() {
        let payload = "event: a\ndata: 1\n\nevent: b\ndata: 2\n\n";
        let server = FakeUpstream::start(vec![sse_response(payload)]);
        let client = client(&server.base_url, fast_config());
        let mut seen = Vec::new();
        let outcome = client
            .stream(b"{}", &AtomicBool::new(false), &mut |chunk| {
                seen.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();

        assert_eq!(outcome.attempts, 1);
        assert_eq!(String::from_utf8(seen).unwrap(), payload);
        assert_eq!(outcome.bytes, payload.len());
    }

    #[test]
    fn first_frame_timeout_is_pre_commit_and_retryable() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                std::thread::sleep(Duration::from_millis(80));
                let _ = stream.write_all(&sse_response("data: too late\n\n"));
            }
        });
        let client = client(
            &base_url,
            UpstreamConfig {
                first_frame_timeout: Some(Duration::from_millis(20)),
                read_timeout: Duration::from_secs(1),
                max_attempts: 2,
                retry_delay: Duration::ZERO,
                ..fast_config()
            },
        );
        let mut seen = Vec::new();
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |chunk| {
                seen.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap_err();

        assert_eq!(error, UpstreamError::Timeout);
        assert!(
            seen.is_empty(),
            "a first-frame timeout must not commit output"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_healthy_first_frame_arriving_within_its_budget_succeeds() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        // Serve *every* attempt, not just the first (#994).
        //
        // This accepted once and then returned, dropping the listener with it.
        // The retry loop below was therefore unwinnable: if attempt 1 failed
        // for any reason at all, attempts 2-5 connected to a socket nobody was
        // listening on and could only fail, and the test reported
        // "loopback connect kept failing across 5 attempts" — which reads like
        // five bad-luck SYNs and is really one failure plus four impossible
        // ones. That is the shape actually observed on the macOS lane, and no
        // amount of widening the first-frame budget could have fixed it,
        // because the budget was never what blew.
        listener.set_nonblocking(true).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let server_done = Arc::clone(&done);
        let server = std::thread::spawn(move || {
            while !server_done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut request = [0_u8; 1024];
                        let _ = stream.read(&mut request);
                        std::thread::sleep(Duration::from_millis(80));
                        // A client that gave up mid-attempt leaves a closed
                        // socket; that is the retry working, not a failure.
                        let _ = stream.write_all(&sse_response("data: healthy\n\n"));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        let client = client(
            &base_url,
            UpstreamConfig {
                // Keep ample scheduler/socket setup margin on shared macOS
                // runners (#994); the fake server still responds after only
                // 80 ms, so this continues to exercise the success path.
                // 15s: the earlier 5s margin blew again on a loaded runner
                // (run 33709832669) — the budget is not the property under
                // test here, the success path is.
                first_frame_timeout: Some(Duration::from_secs(15)),
                read_timeout: Duration::from_secs(15),
                ..fast_config()
            },
        );
        let mut outcome = None;
        let mut seen = Vec::new();
        let mut last_error = None;
        // A loaded shared macOS runner can refuse a fresh loopback SYN
        // (`Transport("connection failed")`, #994) — the property under test
        // is the healthy-frame success path, not first-SYN luck, so retry
        // the connection a bounded number of times.
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(100));
            }
            match client.stream(b"{}", &AtomicBool::new(false), &mut |chunk| {
                seen.extend_from_slice(chunk);
                Ok(())
            }) {
                Ok(result) => {
                    outcome = Some(result);
                    break;
                }
                Err(err) => {
                    seen.clear();
                    // Keep the cause: the old message asserted a connect
                    // failure regardless of what actually went wrong, which
                    // sent readers looking at loopback when the error was
                    // something else entirely.
                    last_error = Some(err);
                }
            }
        }
        let outcome = outcome.unwrap_or_else(|| {
            panic!("no attempt succeeded across 5 tries; last error: {last_error:?}")
        });

        // The client's own pre-commit retry is allowed to fire here: on the
        // same loaded runners the first loopback connect can be refused, the
        // client classifies that as transient, and attempt 2 succeeds — that
        // is the retry policy working, and it delivered exactly one healthy
        // frame (run 33827259075 saw `attempts == 2` with a clean payload).
        // What must hold is that the frame arrived and nothing was committed
        // twice; `streams_chunks_to_the_sink_in_order` pins the single-attempt
        // count against a server that never refuses.
        assert!(
            outcome.attempts >= 1 && outcome.attempts <= fast_config().max_attempts,
            "attempts must stay within the transient budget: {}",
            outcome.attempts
        );
        assert_eq!(seen, b"data: healthy\n\n");
        done.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[test]
    fn request_carries_only_headers_we_construct() {
        let server = FakeUpstream::start(vec![sse_response("data: ok\n\n")]);
        let credentials =
            ApiKeyCredentials::new("sk-upstream-key", Some(server.base_url.clone())).unwrap();
        let client = UpstreamClient::new(credentials, fast_config());
        client
            .stream(
                br#"{"model":"m"}"#,
                &AtomicBool::new(false),
                &mut |_| Ok(()),
            )
            .unwrap();

        let request = server.requests().remove(0);
        assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
        assert!(request.contains("Authorization: Bearer sk-upstream-key"));
        assert!(request.contains("Accept: text/event-stream"));
        assert!(request.contains("OpenAI-Beta: responses=experimental"));
        // openai/codex sends `codex_cli_rs/<Codex version>` and a
        // separate `version` header. Keep clud's own version out of both.
        assert!(request.contains("User-Agent: codex_cli_rs/0.157.1 (clud)"));
        assert!(request.contains("version: 0.157.1"));
        assert!(request.contains(r#"{"model":"m"}"#));
        // The harness's own downstream bearer must never appear upstream.
        assert!(!request.contains("x-api-key"));
        assert!(!request.to_lowercase().contains("anthropic"));
    }

    #[test]
    fn retryable_status_before_any_output_is_retried() {
        let server = FakeUpstream::start(vec![
            status_response(503),
            status_response(429),
            sse_response("data: recovered\n\n"),
        ]);
        let client = client(&server.base_url, fast_config());
        let mut seen = Vec::new();
        let outcome = client
            .stream(b"{}", &AtomicBool::new(false), &mut |chunk| {
                seen.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();

        assert_eq!(outcome.attempts, 3);
        assert_eq!(server.hits(), 3);
        assert_eq!(String::from_utf8(seen).unwrap(), "data: recovered\n\n");
    }

    #[test]
    fn non_retryable_status_fails_immediately_and_hides_the_body() {
        let server = FakeUpstream::start(vec![status_response(401)]);
        let client = client(&server.base_url, fast_config());
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err();

        let failure = error.failure().expect("a status failure");
        assert_eq!(failure.status(), 401);
        assert_eq!(failure.class(), FailureClass::Permanent);
        assert_eq!(server.hits(), 1, "a 401 must not be retried");
        // Reading the body to classify it must not turn the error into a
        // credential map: the scrubber runs before anything is retained.
        let rendered = format!("{error} {error:?} {}", failure.diagnostic());
        assert!(!rendered.contains("sk-secret-abc"), "{rendered}");
        assert!(!rendered.contains("org_9"), "{rendered}");
    }

    #[test]
    fn subscription_401_before_output_refreshes_and_replays_once() {
        struct RecoveringCredentials {
            base_url: String,
            refreshes: Arc<AtomicUsize>,
        }
        impl CredentialSource for RecoveringCredentials {
            fn resolve(&self) -> Result<UpstreamTarget, UpstreamError> {
                Ok(UpstreamTarget::new(&self.base_url, "Bearer old-access")
                    .with_account_id(Some("account-one".to_string())))
            }

            fn recover_after_unauthorized(
                &self,
                _rejected: &UpstreamTarget,
            ) -> Result<Option<UpstreamTarget>, UpstreamError> {
                self.refreshes.fetch_add(1, Ordering::SeqCst);
                Ok(Some(
                    UpstreamTarget::new(&self.base_url, "Bearer new-access")
                        .with_account_id(Some("account-one".to_string())),
                ))
            }
        }

        let server = FakeUpstream::start(vec![
            status_response(401),
            sse_response("data: recovered\n\n"),
        ]);
        let refreshes = Arc::new(AtomicUsize::new(0));
        let client = UpstreamClient::new(
            RecoveringCredentials {
                base_url: server.base_url.clone(),
                refreshes: Arc::clone(&refreshes),
            },
            fast_config(),
        );
        let mut output = Vec::new();
        let outcome = client
            .stream(b"{}", &AtomicBool::new(false), &mut |chunk| {
                output.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
        assert_eq!(outcome.attempts, 2);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(server.hits(), 2);
        assert!(server.requests()[0].contains("Authorization: Bearer old-access"));
        assert!(server.requests()[1].contains("Authorization: Bearer new-access"));
        assert_eq!(
            client.credential_target().unwrap().authorization,
            "Bearer new-access",
            "a later in-band rejection must mark the bearer actually sent"
        );
        assert_eq!(output, b"data: recovered\n\n");
    }

    #[test]
    fn attempts_are_capped() {
        let server = FakeUpstream::start(vec![status_response(500)]);
        let client = client(
            &server.base_url,
            UpstreamConfig {
                max_attempts: 2,
                ..fast_config()
            },
        );
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err();
        assert_eq!(error.failure().map(UpstreamFailure::status), Some(500));
        assert_eq!(server.hits(), 2);
    }

    /// The rule that step 1 makes absolute: once a byte has reached the sink,
    /// the response is committed and no failure may cause a replay.
    #[test]
    fn a_failure_after_output_has_begun_is_never_replayed() {
        // Claim a long body, deliver part of it, then hang up mid-stream.
        let truncated = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 4096\r\nConnection: close\r\n\r\ndata: partial\n\n".to_vec();
        let server = FakeUpstream::start(vec![truncated, sse_response("data: retry\n\n")]);
        let client = client(&server.base_url, fast_config());

        let mut seen = Vec::new();
        let result = client.stream(b"{}", &AtomicBool::new(false), &mut |chunk| {
            seen.extend_from_slice(chunk);
            Ok(())
        });

        assert!(result.is_err(), "a truncated body must surface as an error");
        assert_eq!(
            server.hits(),
            1,
            "the request was replayed after downstream output had begun"
        );
        assert_eq!(String::from_utf8(seen).unwrap(), "data: partial\n\n");
    }

    #[test]
    fn a_failing_sink_stops_the_stream_and_is_not_retried() {
        let server = FakeUpstream::start(vec![sse_response("data: one\n\n")]);
        let client = client(&server.base_url, fast_config());
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| {
                Err(UpstreamError::Downstream("client hung up"))
            })
            .unwrap_err();

        assert_eq!(error, UpstreamError::Downstream("client hung up"));
        assert_eq!(server.hits(), 1);
    }

    #[test]
    fn cancellation_is_observed_before_the_request() {
        let server = FakeUpstream::start(vec![sse_response("data: x\n\n")]);
        let client = client(&server.base_url, fast_config());
        let cancel = AtomicBool::new(true);
        let error = client.stream(b"{}", &cancel, &mut |_| Ok(())).unwrap_err();
        assert_eq!(error, UpstreamError::Cancelled);
        assert_eq!(server.hits(), 0);
    }

    #[test]
    fn cancellation_during_backoff_stops_the_retry_loop() {
        let server = FakeUpstream::start(vec![status_response(500)]);
        let client = client(
            &server.base_url,
            UpstreamConfig {
                retry_delay: Duration::from_millis(400),
                max_attempts: 5,
                ..fast_config()
            },
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            flag.store(true, Ordering::Release);
        });
        let error = client.stream(b"{}", &cancel, &mut |_| Ok(())).unwrap_err();
        assert_eq!(error, UpstreamError::Cancelled);
        assert!(server.hits() < 5, "backoff did not observe cancellation");
    }

    #[test]
    fn oversized_responses_are_refused() {
        let server = FakeUpstream::start(vec![sse_response(&"x".repeat(4096))]);
        let client = client(
            &server.base_url,
            UpstreamConfig {
                max_response_bytes: 1024,
                ..fast_config()
            },
        );
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err();
        assert_eq!(error, UpstreamError::TooLarge);
    }

    /// Build a failure without a socket, so classification is testable alone.
    fn failure_from(status: u16, body: &str) -> UpstreamFailure {
        UpstreamFailure::from_parts(status, |_| None, body, DEFAULT_MAX_RETRY_DELAY)
    }

    #[test]
    fn context_limit_detection_requires_the_exact_machine_code() {
        assert!(
            failure_from(400, r#"{"error":{"code":"context_length_exceeded"}}"#)
                .is_context_length_exceeded()
        );
        assert!(
            failure_from(400, r#"{"code":"context_length_exceeded"}"#).is_context_length_exceeded()
        );
        assert!(
            !failure_from(400, r#"{"error":{"code":"invalid_request"}}"#)
                .is_context_length_exceeded()
        );
        assert!(
            !failure_from(413, r#"{"error":{"message":"context length exceeded"}}"#)
                .is_context_length_exceeded()
        );
        assert!(!failure_from(400, "not json").is_context_length_exceeded());
    }

    fn body_response(status: u16, body: &str, headers: &[(&str, &str)]) -> Vec<u8> {
        let extra: String = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        format!(
            "HTTP/1.1 {status} Err\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn every_known_failure_signature_classifies_as_intended() {
        // Keep the signature tables load-bearing: changing or moving a signature
        // must not silently alter its retry class.
        for (class, signatures) in [
            (FailureClass::Permanent, PERMANENT_SIGNATURES),
            (FailureClass::Exhausted, EXHAUSTED_SIGNATURES),
            (FailureClass::Transient, TRANSIENT_SIGNATURES),
            (FailureClass::PolicyRejected, POLICY_REJECTED_SIGNATURES),
        ] {
            assert!(
                !signatures.is_empty(),
                "{class:?} signatures must not be empty"
            );
            for signature in signatures {
                let body = format!(r#"{{"error":{{"message":"{signature}"}}}}"#);
                assert_eq!(
                    failure_from(502, &body).class(),
                    class,
                    "signature {signature:?} no longer classifies as {class:?}",
                );
            }
        }
    }

    #[test]
    fn a_guardrail_rejection_is_policy_not_a_malformed_request() {
        // The exact body OpenRouter returns when its ZDR guardrail excludes the
        // only endpoint that could serve the request.
        let body = "API Error: 400 0 endpoints out of 1 requested are available \
                    matching your guardrail restrictions and data policy. \
                    ZDR violation (guardrail)";
        let failure = failure_from(400, body);
        assert_eq!(failure.class(), FailureClass::PolicyRejected);

        // Not retryable: a second attempt names the same excluded endpoint, so
        // the only way to succeed is a different route, not a fresher try.
        assert_eq!(failure.max_attempts(&fast_config()), 1);
        assert!(!UpstreamError::Status(failure).is_retryable());

        // The carve-out is narrow. An ordinary 400 is still Permanent, so it
        // still never descends the ladder and never spends a second account.
        let ordinary = failure_from(400, r#"{"error":{"message":"bad shape"}}"#);
        assert_eq!(ordinary.class(), FailureClass::Permanent);

        // And the signature wins over the status even at 5xx, where the
        // fallthrough would otherwise have called it an outage.
        assert_eq!(
            failure_from(502, body).class(),
            FailureClass::PolicyRejected
        );
    }

    #[test]
    fn retry_classification_matches_the_documented_policy() {
        // Recognised outages stay retryable.
        for status in [408, 429, 500, 502, 503] {
            let failure = failure_from(status, r#"{"error":{"message":"try again"}}"#);
            assert_eq!(failure.class(), FailureClass::Transient, "{status}");
            assert!(UpstreamError::Status(failure).is_retryable(), "{status}");
        }
        // Statuses the bridge passes through are never retried.
        for status in [400, 401, 403, 404, 422] {
            let failure = failure_from(status, "");
            assert_eq!(failure.class(), FailureClass::Permanent, "{status}");
            assert!(!UpstreamError::Status(failure).is_retryable(), "{status}");
        }
        assert!(UpstreamError::Transport("connection failed").is_retryable());
        for error in [
            UpstreamError::Credentials("x"),
            UpstreamError::TooLarge,
            UpstreamError::Cancelled,
            UpstreamError::Downstream("x"),
        ] {
            assert!(!error.is_retryable(), "{error:?}");
        }
    }

    /// The reversal this change exists for: a 502 can carry a *permanent*
    /// rejection, and retrying it can never succeed.
    ///
    /// Regression guard for the sub2api#4020 shape, where `gpt-5.6-sol` is
    /// refused with a version-gate message wrapped in a 502.
    #[test]
    fn a_permanent_rejection_wrapped_in_a_502_is_attempted_exactly_once() {
        let body = r#"{"error":{"message":"The 'gpt-5.6-sol' model requires a newer version of Codex. Please upgrade."}}"#;
        let server = FakeUpstream::start(vec![body_response(502, body, &[])]);
        let client = client(&server.base_url, fast_config());

        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err();

        let failure = error.failure().expect("a status failure");
        assert_eq!(failure.status(), 502);
        assert_eq!(failure.class(), FailureClass::Permanent);
        assert_eq!(
            server.hits(),
            1,
            "a permanent rejection must not be retried, however 5xx it looks"
        );
    }

    /// A 5xx we cannot read gets a reduced budget rather than the full ladder:
    /// treating every unrecognised 5xx as fully retryable is what produced the
    /// credential-burning cascade this policy is designed to avoid.
    #[test]
    fn an_unrecognised_5xx_retries_on_the_reduced_budget() {
        let body = r#"{"error":{"message":"something we have never seen"}}"#;
        assert_eq!(failure_from(500, body).class(), FailureClass::Unknown);

        let server = FakeUpstream::start(vec![body_response(500, body, &[])]);
        let client = client(
            &server.base_url,
            UpstreamConfig {
                max_attempts: 6,
                unknown_max_attempts: 2,
                ..fast_config()
            },
        );
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err();

        assert_eq!(
            error.failure().map(UpstreamFailure::class),
            Some(FailureClass::Unknown)
        );
        assert_eq!(
            server.hits(),
            2,
            "an unrecognised 5xx must use the reduced budget, not max_attempts"
        );
    }

    #[test]
    fn correlation_identifiers_are_captured_from_the_error_response() {
        let body = r#"{"error":{"code":"server_error","message":"bad gateway"}}"#;
        let server = FakeUpstream::start(vec![body_response(
            502,
            body,
            &[
                ("cf-ray", "9a1b2c3d4e5f-HKG"),
                ("x-request-id", "req_abc123"),
            ],
        )]);
        let client = client(
            &server.base_url,
            UpstreamConfig {
                max_attempts: 1,
                ..fast_config()
            },
        );
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err();

        let failure = error.failure().expect("a status failure");
        assert_eq!(failure.cf_ray(), Some("9a1b2c3d4e5f-HKG"));
        assert_eq!(failure.request_id(), Some("req_abc123"));
        // Both must reach the operator log, which is the whole point: a cf-ray
        // is what identifies a failure as edge-side rather than provider-side.
        let diagnostic = failure.diagnostic();
        assert!(diagnostic.contains("9a1b2c3d4e5f-HKG"), "{diagnostic}");
        assert!(diagnostic.contains("req_abc123"), "{diagnostic}");
        assert!(diagnostic.contains("class=Transient"), "{diagnostic}");
    }

    #[test]
    fn a_retry_after_header_is_honoured_and_clamped() {
        // Far larger than the ceiling: a generous hint must not pin the turn.
        let server = FakeUpstream::start(vec![
            body_response(503, "unavailable", &[("retry-after", "3600")]),
            sse_response("data: ok\n\n"),
        ]);
        let config = UpstreamConfig {
            max_retry_delay: Duration::from_millis(80),
            ..fast_config()
        };
        let client = client(&server.base_url, config);

        let started = Instant::now();
        let outcome = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(outcome.attempts, 2);
        assert!(
            elapsed < Duration::from_secs(5),
            "a 3600s Retry-After was not clamped: waited {elapsed:?}"
        );
    }

    /// A 429 whose body says the plan is exhausted is **not** a throttle.
    ///
    /// This assertion used to read `Transient`, which meant a multi-day
    /// exhaustion burned three attempts in ~750ms and was reported as a bare
    /// "status 429". Status alone cannot tell the two apart -- only the body
    /// can, which is why it is classified before the 408/429 rule runs.
    #[test]
    fn an_exhausted_plan_is_not_retried_however_it_is_spelled() {
        let body = r#"{"error":{"code":"usage_limit_reached","resets_in_seconds":529498}}"#;
        let failure = failure_from(429, body);
        assert_eq!(failure.class(), FailureClass::Exhausted);
        assert_eq!(failure.resets_in(), Some(Duration::from_secs(529_498)));
        assert_eq!(
            failure.max_attempts(&UpstreamConfig::default()),
            1,
            "an exhausted plan must be attempted exactly once"
        );

        for spelling in [
            r#"{"error":{"code":"insufficient_quota"}}"#,
            r#"{"error":{"type":"quota_exceeded"}}"#,
            r#"{"error":{"message":"You are out of credits"}}"#,
        ] {
            assert_eq!(
                failure_from(429, spelling).class(),
                FailureClass::Exhausted,
                "{spelling}"
            );
        }
    }

    /// A period-scoped quota is exhaustion, not a throttle (#968).
    ///
    /// OpenRouter reports a spent daily allowance as
    /// `429 free-models-per-day-...` and a spent balance as
    /// `402 requires more credits`. Neither carries any of the signatures this
    /// list already knew, so both classified as ordinary throttles and were
    /// retried against an account that could not serve them for hours.
    #[test]
    fn a_spent_period_allowance_is_exhaustion_not_a_throttle() {
        for body in [
            r#"{"error":{"message":"Rate limit exceeded: free-models-per-day-stealth"}}"#,
            r#"{"error":{"message":"You have hit your daily limit"}}"#,
            r#"{"error":{"message":"daily quota reached for this key"}}"#,
        ] {
            assert_eq!(
                failure_from(429, body).class(),
                FailureClass::Exhausted,
                "{body}"
            );
        }
        let drained = failure_from(
            402,
            r#"{"error":{"message":"This request requires more credits, or fewer max_tokens"}}"#,
        );
        assert_eq!(drained.class(), FailureClass::Exhausted);
        assert_eq!(
            drained.max_attempts(&UpstreamConfig::default()),
            1,
            "a spent balance must be attempted exactly once"
        );
    }

    /// A 429 with no exhaustion signature stays retryable.
    #[test]
    fn an_ordinary_throttle_is_still_transient() {
        let failure = failure_from(429, r#"{"error":{"code":"rate_limit_exceeded"}}"#);
        assert_eq!(failure.class(), FailureClass::Transient);
        assert!(failure.max_attempts(&UpstreamConfig::default()) > 1);
    }

    #[test]
    fn backoff_grows_exponentially_is_capped_and_is_jittered() {
        let config = UpstreamConfig {
            retry_delay: Duration::from_millis(100),
            max_retry_delay: Duration::from_millis(1000),
            ..UpstreamConfig::default()
        };
        // Without jitter the ladder doubles until it hits the ceiling.
        let ladder: Vec<Duration> = (1..=6)
            .map(|attempt| backoff_delay(attempt, &config, 0.0))
            .collect();
        assert_eq!(ladder[0], Duration::from_millis(100));
        assert_eq!(ladder[1], Duration::from_millis(200));
        assert_eq!(ladder[2], Duration::from_millis(400));
        assert_eq!(ladder[3], Duration::from_millis(800));
        for delay in &ladder {
            assert!(
                *delay <= config.max_retry_delay,
                "{delay:?} exceeded the cap"
            );
        }
        assert_eq!(
            ladder[4], config.max_retry_delay,
            "the ladder must saturate"
        );
        assert_eq!(ladder[5], config.max_retry_delay);

        // Jitter spreads each step over the lower half of its window, so two
        // clients that fail together do not retry in lockstep.
        let spread: Vec<Duration> = [0.0, 0.25, 0.5, 0.75, 1.0]
            .into_iter()
            .map(|fraction| backoff_delay(3, &config, fraction))
            .collect();
        assert!(
            spread.windows(2).all(|pair| pair[0] > pair[1]),
            "jitter produced no variation: {spread:?}"
        );
        assert_eq!(spread[0], Duration::from_millis(400));
        assert_eq!(spread[4], Duration::from_millis(200));
        for delay in &spread {
            assert!(*delay >= Duration::from_millis(200) && *delay <= Duration::from_millis(400));
        }
        // An out-of-range fraction must not escape the window.
        assert_eq!(backoff_delay(3, &config, 5.0), Duration::from_millis(200));
        assert_eq!(backoff_delay(3, &config, -5.0), Duration::from_millis(400));
    }

    #[test]
    fn the_total_retry_sleep_is_bounded() {
        let server = FakeUpstream::start(vec![body_response(503, "service unavailable", &[])]);
        let client = client(
            &server.base_url,
            UpstreamConfig {
                max_attempts: 20,
                retry_delay: Duration::from_millis(40),
                max_retry_delay: Duration::from_millis(80),
                max_retry_elapsed: Duration::from_millis(200),
                ..fast_config()
            },
        );
        let started = Instant::now();
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(error.failure().map(UpstreamFailure::status), Some(503));
        assert!(
            elapsed < Duration::from_secs(5),
            "the elapsed-sleep budget did not bound the loop: {elapsed:?}"
        );
        assert!(
            server.hits() < 20,
            "the budget must stop the loop before max_attempts"
        );
    }

    #[test]
    fn the_scrubber_redacts_token_shaped_runs() {
        let scrubbed = scrub("key sk-secret-abc for org org_9 and AKIAIOSFODNN7EXAMPLEXYZ ok");
        assert!(!scrubbed.contains("sk-secret-abc"), "{scrubbed}");
        assert!(!scrubbed.contains("org_9"), "{scrubbed}");
        assert!(!scrubbed.contains("AKIAIOSFODNN7EXAMPLEXYZ"), "{scrubbed}");
        // Ordinary words survive, or the detail would be useless.
        assert!(scrubbed.contains("key"), "{scrubbed}");
        assert!(scrubbed.contains("for"), "{scrubbed}");
        assert!(scrubbed.contains("ok"), "{scrubbed}");
    }

    #[test]
    fn chatgpt_top_level_detail_is_scrubbed_for_the_operator_log() {
        let failure = failure_from(
            400,
            r#"{"detail":"The 'gpt-6-sol' model is not supported when using Codex with a ChatGPT account. key sk-secret-abc"}"#,
        );
        let detail = failure.detail().expect("ChatGPT detail should be visible");
        assert!(detail.contains("gpt-6-sol"), "{detail}");
        assert!(detail.contains("not supported"), "{detail}");
        assert!(!detail.contains("sk-secret-abc"), "{detail}");
    }

    #[test]
    fn an_expired_codex_login_is_refused_with_an_actionable_message() {
        let expired = auth_json_with_expiry(1_000_000_000);
        let error = CodexCliCredentials::from_auth_json(expired.as_bytes()).unwrap_err();
        assert_eq!(
            error,
            UpstreamError::Credentials("the Codex login has expired -- run `codex login`")
        );

        // A live token still resolves.
        let live_expiry = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 86_400;
        let live = auth_json_with_expiry(live_expiry);
        assert!(CodexCliCredentials::from_auth_json(live.as_bytes()).is_ok());

        // An opaque (non-JWT) bearer is legitimate and must keep working.
        let opaque = r#"{"tokens":{"access_token":"opaque-token","refresh_token":"refresh","account_id":"acct"}}"#;
        assert!(CodexCliCredentials::from_auth_json(opaque.as_bytes()).is_ok());
    }

    /// Build an `auth.json` whose access token is an unsigned JWT with `exp`.
    fn auth_json_with_expiry(exp: u64) -> String {
        let claims = base64url_encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
        let header = base64url_encode(br#"{"alg":"none"}"#);
        format!(
            r#"{{"tokens":{{"access_token":"{header}.{claims}.sig","refresh_token":"refresh","account_id":"acct"}}}}"#
        )
    }

    fn base64url_encode(input: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let mut buffer = 0_u32;
            for (index, byte) in chunk.iter().enumerate() {
                buffer |= u32::from(*byte) << (16 - 8 * index);
            }
            let characters = chunk.len() + 1;
            for index in 0..characters {
                let value = (buffer >> (18 - 6 * index)) & 0x3F;
                out.push(ALPHABET[value as usize] as char);
            }
        }
        out
    }

    #[test]
    fn base64url_round_trips() {
        let samples: [&[u8]; 6] = [b"", b"a", b"ab", b"abc", b"abcd", br#"{"exp":123}"#];
        for sample in samples {
            let encoded = base64url_encode(sample);
            assert_eq!(
                base64url_decode(&encoded).as_deref(),
                Some(sample),
                "sample {sample:?} encoded as {encoded}"
            );
        }
    }

    /// A connection refused before any byte is a transport failure, and is
    /// retried up to the cap rather than surfacing on the first try.
    #[test]
    fn unreachable_upstream_is_a_classified_transport_error() {
        // Bind and drop to obtain a port nothing is listening on.
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        let client = client(
            &format!("http://127.0.0.1:{port}"),
            UpstreamConfig {
                max_attempts: 2,
                retry_delay: Duration::from_millis(5),
                connect_timeout: Duration::from_millis(500),
                ..fast_config()
            },
        );
        let error = client
            .stream(b"{}", &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err();
        assert_eq!(error, UpstreamError::Transport("connection failed"));
    }

    #[test]
    fn model_override_travels_with_the_target() {
        let target = UpstreamTarget::new("https://gw.test", "Bearer k")
            .with_model_override(Some(ModelSpec::parse("terra@high").unwrap()));
        assert_eq!(
            target.model_override().map(ModelSpec::display),
            Some("gpt-5.6-terra@high".to_string())
        );
        assert_eq!(
            UpstreamTarget::new("https://gw.test", "Bearer k").model_override(),
            None
        );
    }
}

/// Manual, network-touching validation against a real account.
///
/// Ignored by default so CI never runs it. #631 needs a way to re-validate
/// against a live subscription without inventing a second harness:
/// `cargo test -p clud --lib live_probe -- --ignored --nocapture`.
#[cfg(test)]
mod live_probe {
    use super::*;
    use std::sync::atomic::AtomicBool;

    const MAX_INPUT_BYTES: usize = 32 * 1024;
    const LONGEST_SUFFIX: &str = "second";

    fn cache_canary_config() -> UpstreamConfig {
        UpstreamConfig {
            // The request-count ceiling must be enforced before the request,
            // not merely observed from StreamOutcome after a retry happened.
            max_attempts: 1,
            unknown_max_attempts: 1,
            ..UpstreamConfig::default()
        }
    }

    fn cache_canary_body(prefix: &str, suffix: &str, cache_key: &str) -> serde_json::Value {
        serde_json::json!({
            "model": "gpt-5.6-terra",
            "input": [{"type": "message", "role": "user", "content": [{
                "type": "input_text", "text": format!("{prefix}{suffix}"),
            }]}],
            "stream": true,
            "store": false,
            "prompt_cache_key": cache_key,
            // The ChatGPT subscription endpoint rejects the Responses API's
            // `max_output_tokens` field. The live probe is explicitly gated
            // and user-authorized instead; `medium` is a supported 5.6 effort.
            "reasoning": {"effort": "medium"},
        })
    }

    fn new_cache_canary_key() -> Result<String, getrandom::Error> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)?;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(format!("clud-live-cache-canary-{hex}"))
    }

    fn cache_canary_prefix(cache_key: &str) -> String {
        let unit = format!("cache canary {cache_key} prefix ");
        let repeat_count = (MAX_INPUT_BYTES - LONGEST_SUFFIX.len()) / unit.len();
        let prefix = unit.repeat(repeat_count);
        assert!(
            prefix.len() + LONGEST_SUFFIX.len() <= MAX_INPUT_BYTES,
            "canary input exceeded its hard input-size ceiling"
        );
        prefix
    }

    #[test]
    fn cache_canary_disables_all_automatic_retries() {
        let config = cache_canary_config();
        assert_eq!(config.max_attempts, 1);
        assert_eq!(config.unknown_max_attempts, 1);
    }

    #[test]
    fn cache_canary_payload_uses_subscription_supported_fields() {
        let first = cache_canary_body("prefix", "first", "canary-key");
        let second = cache_canary_body("prefix", "second", "canary-key");
        assert_eq!(first["reasoning"]["effort"], "medium");
        assert!(first.get("max_output_tokens").is_none());
        assert_eq!(first["prompt_cache_key"], "canary-key");
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
    }

    #[test]
    fn cache_canary_key_uses_os_entropy() {
        let key = new_cache_canary_key().expect("OS entropy available in test");
        assert!(key.starts_with("clud-live-cache-canary-"));
        assert_ne!(
            key,
            "clud-live-cache-canary-00000000000000000000000000000000"
        );
    }

    #[test]
    fn cache_canary_longest_request_stays_within_input_ceiling() {
        let key = "clud-live-cache-canary-12345678901234567890123456789012";
        let body = cache_canary_body(&cache_canary_prefix(key), LONGEST_SUFFIX, key);
        let text = body["input"][0]["content"][0]["text"]
            .as_str()
            .expect("canary input text");
        assert!(text.len() <= MAX_INPUT_BYTES);
    }

    #[test]
    #[ignore = "requires real credentials and network access"]
    fn a_real_upstream_request_streams_back() {
        let credentials = ResolvedCredentials::resolve_default().expect("credentials");
        eprintln!("credential source = {}", credentials.describe());
        let target = credentials.resolve().expect("target");
        assert!(target.responses_url().ends_with("/responses"));

        let client = UpstreamClient::new(credentials, UpstreamConfig::default());
        let body = br#"{"model":"gpt-5.6-terra","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Say BRIDGED"}]}],"stream":true,"store":false,"include":["reasoning.encrypted_content"],"reasoning":{"effort":"medium"}}"#;
        let mut received = String::new();
        let outcome = client
            .stream(body, &AtomicBool::new(false), &mut |chunk| {
                received.push_str(&String::from_utf8_lossy(chunk));
                Ok(())
            })
            .expect("a real upstream request must succeed");

        assert_eq!(outcome.attempts, 1);
        assert!(received.contains("response.created"), "{received:.200}");
        assert!(received.contains("response.completed"));
    }

    /// A deliberately narrow live cache contract check for #1226.
    ///
    /// This is both ignored and explicitly gated so an ordinary `--ignored`
    /// invocation cannot charge a user's account. It makes exactly two serial
    /// requests, with no tools or retries, and prints token counts only.
    #[test]
    #[ignore = "set CLUD_LIVE_CODEX_CACHE_TESTS=1; uses real credentials and two requests"]
    fn cache_credit_is_reused_for_a_stable_conversation_prefix() {
        assert_eq!(
            std::env::var("CLUD_LIVE_CODEX_CACHE_TESTS").as_deref(),
            Ok("1"),
            "refusing live cache canary without CLUD_LIVE_CODEX_CACHE_TESTS=1"
        );
        let cache_key =
            new_cache_canary_key().expect("OS entropy required for a live cache canary");
        let credentials = ResolvedCredentials::resolve_default().expect("credentials");
        let client = UpstreamClient::new(credentials, cache_canary_config())
            .with_session_id(cache_key.clone());
        // Large enough for provider prompt-cache eligibility while remaining
        // below the strict input-size ceiling.
        const MAX_REQUESTS: usize = 2;
        let prefix = cache_canary_prefix(&cache_key);
        let mut usage = Vec::new();

        for suffix in ["first", "second"] {
            let body = cache_canary_body(&prefix, suffix, &cache_key);
            let mut received = String::new();
            let outcome = client
                .stream(
                    &serde_json::to_vec(&body).expect("canary request JSON"),
                    &AtomicBool::new(false),
                    &mut |chunk| {
                        received.push_str(&String::from_utf8_lossy(chunk));
                        Ok(())
                    },
                )
                .expect("canary request must succeed without retry");
            assert_eq!(outcome.attempts, 1, "the canary must not retry");
            let terminal = received
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|event| event["type"] == "response.completed")
                .expect("terminal usage event");
            let input = terminal["response"]["usage"]["input_tokens"]
                .as_u64()
                .expect("input token count");
            let cached = terminal["response"]["usage"]["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0);
            usage.push((input, cached));
        }
        assert_eq!(usage.len(), MAX_REQUESTS, "canary request-count ceiling");

        eprintln!(
            "live cache canary: first input={} cached={}; second input={} cached={}",
            usage[0].0, usage[0].1, usage[1].0, usage[1].1
        );
        assert!(
            usage[0].1 == 0,
            "the unique first request must have no cache credit"
        );
        assert!(
            usage[1].1.saturating_mul(100) >= usage[1].0.saturating_mul(80),
            "the second request must cache at least 80% of its fixed prefix"
        );
    }
}

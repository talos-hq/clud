//! Kimi (Moonshot) OAuth credential acquisition: the RFC 8628 device
//! authorization grant for a Kimi Code plan, its polling state machine, the
//! refresh exchange and OS-vault persistence.
//!
//! Ports CLIProxyAPI `internal/auth/kimi/{kimi,token}.go`. Additive and
//! deliberately NOT wired to a provider: clud reaches Kimi today as an
//! API-key Anthropic-compatible endpoint (`https://api.moonshot.ai/anthropic`,
//! see `provider_registry.rs`), while a Kimi Code plan is a bearer token
//! against `https://api.kimi.com/coding`. Changing the descriptor's credential
//! *kind* moves the launch preflight, the base URL and the probe URL, so that
//! is a separate slice. This module is the credential half; the hidden
//! `__kimi-device-login` seam is the only thing that reaches it today.
//!
//! Nothing here prints or serializes a secret: [`KimiTokens`] renders redacted,
//! and the seam reports only the user code and non-secret identity metadata.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::provider_auth::{NativeSecretStore, SecretStore};

/// Kimi Code's public OAuth client id (upstream `kimiClientID`).
pub const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
/// OAuth host for the `.com` domain (upstream `KimiOAuthHost`).
pub const KIMI_OAUTH_HOST_COM: &str = "https://auth.kimi.com";
/// OAuth host for the `.ai` domain (upstream `KimiAIOAuthHost`).
pub const KIMI_OAUTH_HOST_AI: &str = "https://auth.kimi.ai";
/// Device-authorization path on both hosts.
pub const KIMI_DEVICE_CODE_PATH: &str = "/api/oauth/device_authorization";
/// Token path on both hosts.
pub const KIMI_TOKEN_PATH: &str = "/api/oauth/token";
/// API base a `.com` Kimi Code credential authenticates against.
pub const KIMI_API_BASE_COM: &str = "https://api.kimi.com/coding";
/// API base a `.ai` Kimi Code credential authenticates against.
pub const KIMI_API_BASE_AI: &str = "https://api.kimi.ai/coding";
/// Vault account for the device-grant record. Distinct from the provider's
/// `api-key-v1` account so an OAuth login can never overwrite a stored key.
pub const KIMI_OAUTH_VAULT_ACCOUNT: &str = "oauth-device-v1";
/// Default poll interval when the device response omits `interval`.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
/// Upper bound on the whole device-code wait (upstream `maxPollDuration`).
pub const MAX_POLL_SECONDS: u64 = 15 * 60;
/// Refresh this many seconds before the recorded expiry
/// (upstream `refreshThresholdSeconds`).
pub const REFRESH_LEAD_SECONDS: u64 = 300;
const HTTP_TIMEOUT_SECONDS: u64 = 30;
const REDACTED: &str = "[redacted]";

/// Which Kimi domain a credential belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiDomain {
    /// `kimi.com` (upstream `KimiDefaultDomain`).
    Com,
    /// `kimi.ai` (upstream `KimiAIDomain`).
    Ai,
}

impl KimiDomain {
    /// Parses a `--domain` value. Accepts the bare TLD (`com`/`ai`) and the
    /// full domain (`kimi.com`/`kimi.ai`).
    pub fn parse(raw: &str) -> Result<Self, KimiAuthError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "com" | "kimi.com" => Ok(Self::Com),
            "ai" | "kimi.ai" => Ok(Self::Ai),
            other => Err(KimiAuthError::new(format!(
                "kimi domain must be com or ai, not {other:?}"
            ))),
        }
    }

    /// The domain as upstream records it in the credential.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Com => "kimi.com",
            Self::Ai => "kimi.ai",
        }
    }

    /// The OAuth host for this domain.
    pub fn oauth_host(self) -> &'static str {
        match self {
            Self::Com => KIMI_OAUTH_HOST_COM,
            Self::Ai => KIMI_OAUTH_HOST_AI,
        }
    }

    /// The API base a credential for this domain authenticates against.
    pub fn api_base_url(self) -> &'static str {
        match self {
            Self::Com => KIMI_API_BASE_COM,
            Self::Ai => KIMI_API_BASE_AI,
        }
    }
}

/// A non-secret credential-acquisition failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiAuthError(String);

impl KimiAuthError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The client-safe message.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KimiAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for KimiAuthError {}

/// The endpoints one device flow talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiEndpoints {
    pub domain: KimiDomain,
    pub device_authorization: String,
    pub token: String,
    pub api_base_url: String,
}

impl KimiEndpoints {
    /// The compiled-in endpoints for `domain`.
    pub fn for_domain(domain: KimiDomain) -> Self {
        Self {
            domain,
            device_authorization: format!("{}{KIMI_DEVICE_CODE_PATH}", domain.oauth_host()),
            token: format!("{}{KIMI_TOKEN_PATH}", domain.oauth_host()),
            api_base_url: domain.api_base_url().to_string(),
        }
    }
}

/// A device-authorization response (RFC 8628 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: Option<u64>,
    pub interval: Option<u64>,
}

/// A persisted Kimi OAuth credential bundle. `Debug` is redacted on purpose.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KimiTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    /// Absolute expiry, unix seconds. `None` when the server omitted a lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<u64>,
    /// Stable device identifier sent with the login (upstream `device_id`).
    pub device_id: String,
    pub domain: String,
    pub api_base_url: String,
    pub token_endpoint: String,
}

impl fmt::Debug for KimiTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KimiTokens")
            .field("access_token", &REDACTED)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| REDACTED),
            )
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("expires_in", &self.expires_in)
            .field("expires_at_unix", &self.expires_at_unix)
            .field("device_id", &self.device_id)
            .field("domain", &self.domain)
            .field("api_base_url", &self.api_base_url)
            .field("token_endpoint", &self.token_endpoint)
            .finish()
    }
}

impl KimiTokens {
    /// True when the access token is expired or inside `skew` seconds of it.
    pub fn is_expired(&self, now_ms: u64, skew: u64) -> bool {
        match self.expires_at_unix {
            Some(expires_at) => {
                now_ms.saturating_add(skew.saturating_mul(1_000))
                    >= expires_at.saturating_mul(1_000)
            }
            None => false,
        }
    }

    /// True when this bundle can be renewed without a fresh device login.
    pub fn can_refresh(&self) -> bool {
        self.refresh_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
    }

    /// Non-secret metadata for the seam's success event.
    pub fn summary(&self) -> Value {
        json!({
            "domain": self.domain,
            "api_base_url": self.api_base_url,
            "expires_in": self.expires_in,
            "has_refresh_token": self.can_refresh(),
            "device_id": self.device_id,
        })
    }
}

/// The raw reply a transport hands back. Error mapping stays pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub body: String,
}

/// The one HTTP boundary this module needs, injectable for tests.
pub trait OAuthTransport {
    fn post_form(
        &mut self,
        url: &str,
        form: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<HttpReply, KimiAuthError>;
}

/// Time and sleeping, so the poll loop is testable without waiting.
pub trait OAuthClock {
    fn now_ms(&self) -> u64;
    fn sleep(&mut self, millis: u64);
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl OAuthClock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default()
    }

    fn sleep(&mut self, millis: u64) {
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis));
        }
    }
}

/// `ureq`-backed transport: one form-encoded POST with a 30s timeout.
#[derive(Debug, Clone, Copy, Default)]
pub struct UreqTransport;

impl OAuthTransport for UreqTransport {
    fn post_form(
        &mut self,
        url: &str,
        form: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<HttpReply, KimiAuthError> {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS))
            .build();
        let body = form_encode(form);
        let mut request = agent
            .post(url)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .set("Accept", "application/json");
        for (name, value) in headers {
            request = request.set(name, value);
        }
        match request.send_string(&body) {
            Ok(response) => read_reply(response),
            // Kimi answers 200 with an `error` body while pending, but a 400
            // with the same shape is also valid RFC 8628; keep the body.
            Err(ureq::Error::Status(status, response)) => Ok(HttpReply {
                status,
                body: response.into_string().unwrap_or_default(),
            }),
            Err(ureq::Error::Transport(transport)) => Err(KimiAuthError::new(format!(
                "kimi oauth request failed: {transport}"
            ))),
        }
    }
}

fn read_reply(response: ureq::Response) -> Result<HttpReply, KimiAuthError> {
    let status = response.status();
    let body = response
        .into_string()
        .map_err(|error| KimiAuthError::new(format!("kimi oauth: read response: {error}")))?;
    Ok(HttpReply { status, body })
}

/// Percent-encoding for `application/x-www-form-urlencoded` (§5.1).
pub fn form_encode(fields: &[(&str, &str)]) -> String {
    let mut encoded = String::new();
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            encoded.push('&');
        }
        encode_component(name, &mut encoded);
        encoded.push('=');
        encode_component(value, &mut encoded);
    }
    encoded
}

fn encode_component(value: &str, out: &mut String) {
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
}

/// The lowercased host of a URL with the scheme already stripped.
fn host_of(rest: &str) -> String {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    host_port
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Whether `raw` is an https URL on `kimi.com`/`kimi.ai` or a subdomain of one
/// (upstream `IsKimiComHost` / `IsKimiAIHost`).
fn is_kimi_https(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix("https://") else {
        return false;
    };
    let host = host_of(rest);
    ["kimi.com", "kimi.ai"]
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// Whether `raw` is a loopback http URL a local test double can serve.
fn is_loopback_http(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix("http://") else {
        return false;
    };
    let host = host_of(rest);
    host == "127.0.0.1" || host == "localhost"
}

/// Validates an endpoint: https on a Kimi host, or a loopback http URL.
///
/// The loopback allowance is a deliberate addition over upstream, and it is
/// the only way the seam and the Python suite can drive the real state machine
/// against a local server. Production endpoints are compiled in, so nothing
/// remote can downgrade this decision.
pub fn validate_endpoint(raw: &str, field: &str) -> Result<String, KimiAuthError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(KimiAuthError::new(format!(
            "kimi endpoint {field} is empty"
        )));
    }
    if is_kimi_https(raw) || is_loopback_http(raw) {
        return Ok(raw.to_string());
    }
    Err(KimiAuthError::new(format!(
        "kimi endpoint {field} must be an https kimi.com/kimi.ai endpoint: {raw:?}"
    )))
}

/// Hostname, for the `X-Msh-Device-Name` header. Never fails.
pub fn device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "clud".to_string())
}

/// OS/arch string, for the `X-Msh-Device-Model` header.
pub fn device_model() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A stable device id. Upstream persists a random UUID; clud deliberately
/// derives it from the host instead of writing a new state file, so repeated
/// logins from one machine look like one device without another credential
/// store to protect.
pub fn device_id() -> String {
    if let Ok(explicit) = std::env::var("CLUD_KIMI_DEVICE_ID") {
        let explicit = explicit.trim().to_string();
        if !explicit.is_empty() {
            return explicit;
        }
    }
    let seed = format!("{}|{}", device_name(), device_model());
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The headers upstream sends on every Kimi OAuth request.
pub fn device_headers(device_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("X-Msh-Platform", "CLUD".to_string()),
        ("X-Msh-Version", env!("CARGO_PKG_VERSION").to_string()),
        ("X-Msh-Device-Name", device_name()),
        ("X-Msh-Device-Model", device_model()),
        ("X-Msh-Device-Id", device_id.to_string()),
    ]
}

fn header_pairs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect()
}

/// Parses a device-authorization response, refusing a record the flow cannot use.
pub fn parse_device_code(body: &str) -> Result<DeviceCode, KimiAuthError> {
    let payload: Value = serde_json::from_str(body).map_err(|error| {
        KimiAuthError::new(format!("kimi device code: parse response: {error}"))
    })?;
    let text = |name: &str| {
        payload
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let number = |name: &str| {
        payload
            .get(name)
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
    };
    let device_code = text("device_code");
    if device_code.is_empty() {
        return Err(KimiAuthError::new(
            "kimi device code: response has no device_code",
        ));
    }
    let user_code = text("user_code");
    if user_code.is_empty() {
        return Err(KimiAuthError::new(
            "kimi device code: response has no user_code",
        ));
    }
    let verification_uri = text("verification_uri");
    if verification_uri.is_empty() {
        return Err(KimiAuthError::new(
            "kimi device code: response has no verification_uri",
        ));
    }
    let complete = text("verification_uri_complete");
    Ok(DeviceCode {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: (!complete.is_empty()).then_some(complete),
        expires_in: number("expires_in"),
        interval: number("interval"),
    })
}

/// Requests a device code and user code (RFC 8628 §3.1).
pub fn request_device_code(
    transport: &mut impl OAuthTransport,
    endpoints: &KimiEndpoints,
    device_id: &str,
) -> Result<DeviceCode, KimiAuthError> {
    let headers = device_headers(device_id);
    let reply = transport.post_form(
        &endpoints.device_authorization,
        &[("client_id", KIMI_CLIENT_ID)],
        &header_pairs(&headers),
    )?;
    if reply.status != 200 {
        return Err(KimiAuthError::new(format!(
            "kimi device code request failed with status {}: {}",
            reply.status,
            reply.body.trim()
        )));
    }
    parse_device_code(&reply.body)
}

/// One poll attempt's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenPoll {
    Authorized(Box<KimiTokens>),
    Pending,
    SlowDown,
}

/// The OAuth `error` code of a token reply, if it carries one.
fn token_error(payload: &Value) -> Option<(String, String)> {
    let code = payload.get("error").and_then(Value::as_str)?.trim();
    if code.is_empty() {
        return None;
    }
    let description = payload
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    Some((code.to_string(), description))
}

fn token_from_payload(
    payload: &Value,
    endpoints: &KimiEndpoints,
    device_id: &str,
    now_ms: u64,
) -> Result<KimiTokens, KimiAuthError> {
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if access_token.is_empty() {
        return Err(KimiAuthError::new(
            "kimi token response carries no access token",
        ));
    }
    let text = |name: &str| {
        payload
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| *value > 0.0)
        .map(|value| value as u64);
    Ok(KimiTokens {
        access_token,
        refresh_token: text("refresh_token"),
        token_type: text("token_type"),
        scope: text("scope"),
        expires_in,
        expires_at_unix: expires_in.map(|seconds| now_ms / 1_000 + seconds),
        device_id: device_id.to_string(),
        domain: endpoints.domain.as_str().to_string(),
        api_base_url: endpoints.api_base_url.clone(),
        token_endpoint: endpoints.token.clone(),
    })
}

/// Classifies one device-code poll reply. Kimi answers **200** for pending
/// states, so the body's `error` field decides, not the status.
fn classify_token_reply(
    reply: &HttpReply,
    endpoints: &KimiEndpoints,
    device_id: &str,
    now_ms: u64,
) -> Result<TokenPoll, KimiAuthError> {
    let payload: Value = serde_json::from_str(&reply.body).map_err(|error| {
        KimiAuthError::new(format!(
            "kimi token response: parse body (status {}): {error}",
            reply.status
        ))
    })?;
    if let Some((code, description)) = token_error(&payload) {
        return match code.as_str() {
            "authorization_pending" => Ok(TokenPoll::Pending),
            "slow_down" => Ok(TokenPoll::SlowDown),
            "expired_token" => Err(KimiAuthError::new("kimi device code expired")),
            "access_denied" => Err(KimiAuthError::new("kimi device authorization denied")),
            _ => Err(KimiAuthError::new(format!(
                "kimi oauth error {code}: {description}"
            ))),
        };
    }
    if reply.status == 401 || reply.status == 403 {
        return Err(KimiAuthError::new(format!(
            "kimi token request rejected (status {})",
            reply.status
        )));
    }
    if reply.status != 200 {
        return Err(KimiAuthError::new(format!(
            "kimi token request failed with status {}: {}",
            reply.status,
            reply.body.trim()
        )));
    }
    token_from_payload(&payload, endpoints, device_id, now_ms)
        .map(|tokens| TokenPoll::Authorized(Box::new(tokens)))
}

/// Poll configuration for one device-code wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollConfig {
    /// Floor for the interval, in milliseconds (tests shorten this).
    pub min_interval_ms: u64,
    /// Cap on the whole wait, in milliseconds.
    pub max_poll_ms: u64,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            min_interval_ms: DEFAULT_POLL_INTERVAL_SECONDS * 1_000,
            max_poll_ms: MAX_POLL_SECONDS * 1_000,
        }
    }
}

/// Polls the token endpoint until the user authorizes, refuses, or the code
/// expires. Like upstream, the first poll happens after one interval.
pub fn poll_for_token(
    transport: &mut impl OAuthTransport,
    clock: &mut impl OAuthClock,
    endpoints: &KimiEndpoints,
    device: &DeviceCode,
    config: &PollConfig,
    device_id: &str,
) -> Result<KimiTokens, KimiAuthError> {
    let mut interval_ms = device
        .interval
        .map(|seconds| seconds.saturating_mul(1_000))
        .unwrap_or(config.min_interval_ms)
        .max(config.min_interval_ms);
    let mut deadline = clock.now_ms().saturating_add(config.max_poll_ms);
    if let Some(expires_in) = device.expires_in {
        deadline = deadline.min(
            clock
                .now_ms()
                .saturating_add(expires_in.saturating_mul(1_000)),
        );
    }
    let headers = device_headers(device_id);
    loop {
        clock.sleep(interval_ms);
        if clock.now_ms() > deadline {
            return Err(KimiAuthError::new("kimi device code expired"));
        }
        let reply = transport.post_form(
            &endpoints.token,
            &[
                ("client_id", KIMI_CLIENT_ID),
                ("device_code", &device.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
            &header_pairs(&headers),
        )?;
        match classify_token_reply(&reply, endpoints, device_id, clock.now_ms())? {
            TokenPoll::Authorized(tokens) => return Ok(*tokens),
            TokenPoll::Pending => {}
            TokenPoll::SlowDown => {
                interval_ms = interval_ms.saturating_add(config.min_interval_ms);
            }
        }
    }
}

/// Exchanges a refresh token for a new access token.
pub fn refresh_tokens(
    transport: &mut impl OAuthTransport,
    clock: &mut impl OAuthClock,
    endpoints: &KimiEndpoints,
    refresh_token: &str,
    device_id: &str,
) -> Result<KimiTokens, KimiAuthError> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(KimiAuthError::new(
            "kimi token refresh: refresh token is required",
        ));
    }
    let headers = device_headers(device_id);
    let reply = transport.post_form(
        &endpoints.token,
        &[
            ("client_id", KIMI_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
        &header_pairs(&headers),
    )?;
    if reply.status == 401 || reply.status == 403 {
        return Err(KimiAuthError::new(format!(
            "kimi refresh token rejected (status {})",
            reply.status
        )));
    }
    if reply.status != 200 {
        return Err(KimiAuthError::new(format!(
            "kimi token refresh failed with status {}: {}",
            reply.status,
            reply.body.trim()
        )));
    }
    let payload: Value = serde_json::from_str(&reply.body).map_err(|error| {
        KimiAuthError::new(format!("kimi refresh response: parse body: {error}"))
    })?;
    token_from_payload(&payload, endpoints, device_id, clock.now_ms())
}

/// Persist a bundle in the OS vault (JSON), replacing any earlier login.
pub fn store_tokens(store: &dyn SecretStore, tokens: &KimiTokens) -> Result<(), String> {
    let encoded = serde_json::to_string(tokens)
        .map_err(|error| format!("kimi credential encode: {error}"))?;
    store
        .set(&encoded)
        .map_err(|error| format!("kimi credential store: {error}"))
}

/// Load a stored bundle, if the vault holds one.
pub fn load_tokens(store: &dyn SecretStore) -> Result<Option<KimiTokens>, String> {
    let Some(raw) = store
        .get()
        .map_err(|error| format!("kimi credential store: {error}"))?
    else {
        return Ok(None);
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| format!("kimi credential store: unreadable entry: {error}"))
}

/// The OS-vault-backed store for Kimi OAuth tokens, in the provider's frozen
/// service with its own account, so an OAuth login never touches the API key.
pub fn vault_store() -> Result<NativeSecretStore, String> {
    NativeSecretStore::new_for(
        crate::provider_auth::KIMI_VAULT_SERVICE,
        KIMI_OAUTH_VAULT_ACCOUNT,
    )
    .map_err(|error| format!("kimi credential store: {error}"))
}

/// Arguments for the hidden test/automation seam. No production path calls it.
#[derive(Debug, Clone, Default)]
pub struct SeamArgs<'a> {
    pub domain: Option<&'a str>,
    pub device_endpoint: Option<&'a str>,
    pub token_endpoint: Option<&'a str>,
    pub interval_ms: Option<u64>,
    pub max_poll_ms: Option<u64>,
    pub refresh_stored: bool,
}

impl SeamArgs<'_> {
    fn poll_config(&self) -> PollConfig {
        PollConfig {
            min_interval_ms: self
                .interval_ms
                .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS * 1_000),
            max_poll_ms: self.max_poll_ms.unwrap_or(MAX_POLL_SECONDS * 1_000),
        }
    }
}

/// Runs the seam and returns a process exit code.
///
/// Prints one JSON object per line. The device code is echoed because the user
/// needs it; access and refresh tokens are never printed.
pub fn run_seam(args: &SeamArgs<'_>) -> i32 {
    match run_seam_inner(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("kimi device login: {error}");
            1
        }
    }
}

fn seam_endpoints(args: &SeamArgs<'_>) -> Result<KimiEndpoints, KimiAuthError> {
    let domain = match args.domain {
        Some(raw) => KimiDomain::parse(raw)?,
        None => KimiDomain::Com,
    };
    match (args.device_endpoint, args.token_endpoint) {
        (Some(device), Some(token)) => Ok(KimiEndpoints {
            domain,
            device_authorization: validate_endpoint(device, "device_authorization")?,
            token: validate_endpoint(token, "token")?,
            api_base_url: domain.api_base_url().to_string(),
        }),
        (None, None) => Ok(KimiEndpoints::for_domain(domain)),
        _ => Err(KimiAuthError::new(
            "both --device-endpoint and --token-endpoint are required",
        )),
    }
}

fn run_seam_inner(args: &SeamArgs<'_>) -> Result<(), KimiAuthError> {
    let endpoints = seam_endpoints(args)?;
    let mut transport = UreqTransport;
    let mut clock = SystemClock;
    let device_id = device_id();
    if args.refresh_stored {
        return refresh_stored(&endpoints, &mut transport, &mut clock, &device_id);
    }
    let device = request_device_code(&mut transport, &endpoints, &device_id)?;
    println!(
        "{}",
        json!({
            "event": "device_code",
            "domain": endpoints.domain.as_str(),
            "user_code": device.user_code,
            "verification_uri": device.verification_uri,
            "verification_uri_complete": device.verification_uri_complete,
            "expires_in": device.expires_in,
            "interval": device.interval,
        })
    );
    let tokens = poll_for_token(
        &mut transport,
        &mut clock,
        &endpoints,
        &device,
        &args.poll_config(),
        &device_id,
    )?;
    println!(
        "{}",
        json!({"event": "authorized", "details": tokens.summary()})
    );
    Ok(())
}

fn refresh_stored(
    endpoints: &KimiEndpoints,
    transport: &mut impl OAuthTransport,
    clock: &mut impl OAuthClock,
    device_id: &str,
) -> Result<(), KimiAuthError> {
    let store = vault_store().map_err(KimiAuthError::new)?;
    let stored = load_tokens(&store).map_err(KimiAuthError::new)?;
    let Some(stored) = stored else {
        return Err(KimiAuthError::new(
            "kimi credential store: no device-grant login to refresh",
        ));
    };
    let refresh_token = stored
        .refresh_token
        .as_deref()
        .ok_or_else(|| KimiAuthError::new("kimi stored login has no refresh token"))?;
    let tokens = refresh_tokens(transport, clock, endpoints, refresh_token, device_id)?;
    store_tokens(&store, &tokens).map_err(KimiAuthError::new)?;
    println!(
        "{}",
        json!({"event": "refreshed", "details": tokens.summary()})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_auth::SecretStoreError;
    use std::sync::{Arc, Mutex};

    type RecordedRequest = (String, String, Vec<(String, String)>);

    #[derive(Default, Clone)]
    struct ScriptedTransport {
        replies: Arc<Mutex<Vec<HttpReply>>>,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl ScriptedTransport {
        fn new(replies: Vec<HttpReply>) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies)),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl OAuthTransport for ScriptedTransport {
        fn post_form(
            &mut self,
            url: &str,
            form: &[(&str, &str)],
            headers: &[(&str, &str)],
        ) -> Result<HttpReply, KimiAuthError> {
            self.requests.lock().unwrap().push((
                url.to_string(),
                form_encode(form),
                headers
                    .iter()
                    .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                    .collect(),
            ));
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Err(KimiAuthError::new("test transport exhausted"));
            }
            Ok(replies.remove(0))
        }
    }

    #[derive(Clone)]
    struct FakeClock {
        now: Arc<Mutex<u64>>,
        sleeps: Arc<Mutex<Vec<u64>>>,
    }

    impl FakeClock {
        fn new(start: u64) -> Self {
            Self {
                now: Arc::new(Mutex::new(start)),
                sleeps: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn sleeps(&self) -> Vec<u64> {
            self.sleeps.lock().unwrap().clone()
        }
    }

    impl OAuthClock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.now.lock().unwrap()
        }

        fn sleep(&mut self, millis: u64) {
            self.sleeps.lock().unwrap().push(millis);
            *self.now.lock().unwrap() += millis;
        }
    }

    fn reply(status: u16, body: Value) -> HttpReply {
        HttpReply {
            status,
            body: body.to_string(),
        }
    }

    fn device() -> DeviceCode {
        DeviceCode {
            device_code: "device-abc".to_string(),
            user_code: "USER-123".to_string(),
            verification_uri: "https://kimi.com/device".to_string(),
            verification_uri_complete: None,
            expires_in: Some(60),
            interval: Some(1),
        }
    }

    #[test]
    fn domains_resolve_to_their_own_hosts_and_api_bases() {
        let com = KimiEndpoints::for_domain(KimiDomain::Com);
        assert_eq!(
            com.device_authorization,
            "https://auth.kimi.com/api/oauth/device_authorization"
        );
        assert_eq!(com.token, "https://auth.kimi.com/api/oauth/token");
        assert_eq!(com.api_base_url, "https://api.kimi.com/coding");
        let ai = KimiEndpoints::for_domain(KimiDomain::Ai);
        assert_eq!(ai.token, "https://auth.kimi.ai/api/oauth/token");
        assert_eq!(ai.api_base_url, "https://api.kimi.ai/coding");
        assert_eq!(KimiDomain::parse("kimi.ai").unwrap(), KimiDomain::Ai);
        assert!(KimiDomain::parse("evil.example").is_err());
    }

    #[test]
    fn endpoints_must_be_https_on_kimi_or_loopback() {
        assert_eq!(
            validate_endpoint("https://auth.kimi.com/api/oauth/token", "token").unwrap(),
            "https://auth.kimi.com/api/oauth/token"
        );
        assert!(validate_endpoint("https://kimi.com.evil.example/token", "token").is_err());
        assert!(validate_endpoint("http://auth.kimi.com/token", "token").is_err());
        assert!(validate_endpoint("https://auth.kimi.ai/token", "token").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:8080/token", "token").is_ok());
        assert!(validate_endpoint("https://evil.example/token", "token").is_err());
        assert!(validate_endpoint("  ", "token").is_err());
    }

    #[test]
    fn device_code_headers_carry_the_msh_device_identity() {
        let headers = device_headers("device-id-1");
        let pairs = header_pairs(&headers);
        assert!(pairs.contains(&("X-Msh-Device-Id", "device-id-1")));
        assert!(pairs.iter().any(|(name, _)| *name == "X-Msh-Version"));
        assert!(pairs.iter().any(|(name, _)| *name == "X-Msh-Device-Model"));
        assert!(!device_id().is_empty());
    }

    #[test]
    fn parse_device_code_requires_the_fields_the_flow_uses() {
        let parsed = parse_device_code(
            &json!({
                "device_code": "d",
                "user_code": "u",
                "verification_uri": "https://kimi.com/device",
                "verification_uri_complete": "https://kimi.com/device?code=u",
                "expires_in": 60,
                "interval": 5
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(parsed.device_code, "d");
        assert_eq!(parsed.expires_in, Some(60));
        assert_eq!(parsed.interval, Some(5));
        assert!(parsed.verification_uri_complete.is_some());
        let missing = parse_device_code(&json!({"user_code": "u"}).to_string());
        assert!(missing
            .expect_err("no device_code")
            .message()
            .contains("device_code"));
        let no_uri = parse_device_code(&json!({"device_code": "d", "user_code": "u"}).to_string());
        assert!(no_uri
            .expect_err("no verification_uri")
            .message()
            .contains("verification_uri"));
    }

    #[test]
    fn request_device_code_sends_only_the_client_id_and_headers() {
        let mut transport = ScriptedTransport::new(vec![reply(
            200,
            json!({
                "device_code": "d",
                "user_code": "u",
                "verification_uri": "https://kimi.com/device",
                "expires_in": 60,
                "interval": 5
            }),
        )]);
        let endpoints = KimiEndpoints::for_domain(KimiDomain::Com);
        let code = request_device_code(&mut transport, &endpoints, "dev-1").unwrap();
        assert_eq!(code.user_code, "u");
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].0,
            "https://auth.kimi.com/api/oauth/device_authorization"
        );
        assert_eq!(requests[0].1, format!("client_id={KIMI_CLIENT_ID}"));
        assert!(requests[0]
            .2
            .iter()
            .any(|(name, value)| name == "X-Msh-Device-Id" && value == "dev-1"));
    }

    #[test]
    fn a_rejected_device_code_request_names_the_status() {
        let mut transport = ScriptedTransport::new(vec![reply(403, json!({"error": "forbidden"}))]);
        let endpoints = KimiEndpoints::for_domain(KimiDomain::Com);
        let error =
            request_device_code(&mut transport, &endpoints, "dev-1").expect_err("403 must fail");
        assert!(error.message().contains("403"));
    }

    #[test]
    fn pending_then_slow_down_then_authorized_widens_the_interval() {
        let mut transport = ScriptedTransport::new(vec![
            reply(200, json!({"error": "authorization_pending"})),
            reply(200, json!({"error": "slow_down"})),
            reply(
                200,
                json!({
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "coding"
                }),
            ),
        ]);
        let mut clock = FakeClock::new(1_000_000);
        let endpoints = KimiEndpoints::for_domain(KimiDomain::Ai);
        let config = PollConfig {
            min_interval_ms: 10,
            max_poll_ms: 60_000,
        };
        let tokens = poll_for_token(
            &mut transport,
            &mut clock,
            &endpoints,
            &device(),
            &config,
            "dev-1",
        )
        .unwrap();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.domain, "kimi.ai");
        assert_eq!(tokens.api_base_url, "https://api.kimi.ai/coding");
        assert_eq!(tokens.expires_at_unix, Some(clock.now_ms() / 1_000 + 3600));
        assert!(tokens.can_refresh());
        // device.interval is 1s, which is the floor; slow_down adds one floor (10ms).
        assert_eq!(clock.sleeps(), vec![1_000, 1_000, 1_010]);
        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[0]
            .1
            .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
        assert!(requests[0].1.contains("device_code=device-abc"));
    }

    #[test]
    fn slow_down_widens_by_the_configured_floor() {
        let mut transport = ScriptedTransport::new(vec![
            reply(200, json!({"error": "slow_down"})),
            reply(200, json!({"access_token": "a", "expires_in": 60})),
        ]);
        let mut clock = FakeClock::new(0);
        let config = PollConfig {
            min_interval_ms: 5,
            max_poll_ms: 60_000,
        };
        let mut short = device();
        short.interval = None;
        poll_for_token(
            &mut transport,
            &mut clock,
            &KimiEndpoints::for_domain(KimiDomain::Com),
            &short,
            &config,
            "dev-1",
        )
        .unwrap();
        assert_eq!(clock.sleeps(), vec![5, 10]);
    }

    #[test]
    fn expired_and_denied_codes_fail_by_name() {
        for (code, expected) in [
            ("expired_token", "kimi device code expired"),
            ("access_denied", "kimi device authorization denied"),
            ("invalid_grant", "kimi oauth error invalid_grant"),
        ] {
            let mut transport = ScriptedTransport::new(vec![reply(
                200,
                json!({"error": code, "error_description": "x"}),
            )]);
            let mut clock = FakeClock::new(0);
            let error = poll_for_token(
                &mut transport,
                &mut clock,
                &KimiEndpoints::for_domain(KimiDomain::Com),
                &device(),
                &PollConfig {
                    min_interval_ms: 1,
                    max_poll_ms: 60_000,
                },
                "dev-1",
            )
            .expect_err("must fail");
            assert!(error.message().contains(expected), "{code}: {error}");
        }
    }

    #[test]
    fn polling_is_bounded_by_the_code_lifetime() {
        let mut transport = ScriptedTransport::new(vec![
            reply(200, json!({"error": "authorization_pending"})),
            reply(200, json!({"error": "authorization_pending"})),
            reply(200, json!({"error": "authorization_pending"})),
        ]);
        let mut clock = FakeClock::new(0);
        let mut short = device();
        short.expires_in = Some(2);
        short.interval = Some(1);
        let error = poll_for_token(
            &mut transport,
            &mut clock,
            &KimiEndpoints::for_domain(KimiDomain::Com),
            &short,
            &PollConfig {
                min_interval_ms: 1,
                max_poll_ms: 600_000,
            },
            "dev-1",
        )
        .expect_err("a code must not outlive expires_in");
        assert_eq!(error.message(), "kimi device code expired");
        assert!(clock.sleeps().len() <= 3);
    }

    #[test]
    fn refresh_rotates_and_reports_a_rejected_token() {
        let endpoints = KimiEndpoints::for_domain(KimiDomain::Com);
        let mut transport = ScriptedTransport::new(vec![reply(
            200,
            json!({
                "access_token": "access-2",
                "refresh_token": "refresh-2",
                "expires_in": 1800
            }),
        )]);
        let mut clock = FakeClock::new(500_000);
        let tokens = refresh_tokens(
            &mut transport,
            &mut clock,
            &endpoints,
            " refresh-1 ",
            "dev-1",
        )
        .unwrap();
        assert_eq!(tokens.access_token, "access-2");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-2"));
        assert_eq!(tokens.expires_at_unix, Some(500 + 1800));
        let requests = transport.requests();
        assert!(requests[0].1.contains("grant_type=refresh_token"));
        assert!(requests[0].1.contains("refresh_token=refresh-1"));

        let mut rejected = ScriptedTransport::new(vec![reply(401, json!({}))]);
        let error = refresh_tokens(&mut rejected, &mut clock, &endpoints, "refresh-1", "dev-1")
            .expect_err("401 must fail");
        assert!(error.message().contains("rejected"));
        let mut empty = ScriptedTransport::new(vec![]);
        assert!(refresh_tokens(&mut empty, &mut clock, &endpoints, "  ", "dev-1").is_err());
    }

    #[test]
    fn expiry_math_uses_the_recorded_lifetime() {
        let tokens = KimiTokens {
            access_token: "a".to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_in: Some(100),
            expires_at_unix: Some(2_000),
            device_id: "d".to_string(),
            domain: "kimi.com".to_string(),
            api_base_url: KIMI_API_BASE_COM.to_string(),
            token_endpoint: "https://auth.kimi.com/api/oauth/token".to_string(),
        };
        assert!(tokens.is_expired(1_800_000, REFRESH_LEAD_SECONDS));
        assert!(!tokens.is_expired(1_000_000, REFRESH_LEAD_SECONDS));
        assert!(!tokens.can_refresh());
        assert_eq!(tokens.summary()["has_refresh_token"], json!(false));
        let no_lifetime = KimiTokens {
            expires_at_unix: None,
            ..tokens
        };
        assert!(!no_lifetime.is_expired(u64::MAX, 0));
    }

    #[test]
    fn debug_never_renders_a_secret() {
        let tokens = KimiTokens {
            access_token: "secret-access".to_string(),
            refresh_token: Some("secret-refresh".to_string()),
            token_type: Some("Bearer".to_string()),
            scope: None,
            expires_in: Some(60),
            expires_at_unix: Some(60),
            device_id: "d".to_string(),
            domain: "kimi.com".to_string(),
            api_base_url: KIMI_API_BASE_COM.to_string(),
            token_endpoint: "https://auth.kimi.com/api/oauth/token".to_string(),
        };
        let rendered = format!("{tokens:?}");
        assert!(!rendered.contains("secret-access"));
        assert!(!rendered.contains("secret-refresh"));
        assert!(rendered.contains(REDACTED));
    }

    struct MemoryStore(Mutex<Option<String>>);

    impl SecretStore for MemoryStore {
        fn get(&self) -> Result<Option<String>, SecretStoreError> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn set(&self, secret: &str) -> Result<(), SecretStoreError> {
            *self.0.lock().unwrap() = Some(secret.to_string());
            Ok(())
        }

        fn delete(&self) -> Result<(), SecretStoreError> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    #[test]
    fn vault_bundle_round_trips_and_reports_a_corrupt_entry() {
        let store = MemoryStore(Mutex::new(None));
        assert!(load_tokens(&store).unwrap().is_none());
        let tokens = KimiTokens {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            token_type: Some("Bearer".to_string()),
            scope: Some("coding".to_string()),
            expires_in: Some(60),
            expires_at_unix: Some(120),
            device_id: "d".to_string(),
            domain: "kimi.com".to_string(),
            api_base_url: KIMI_API_BASE_COM.to_string(),
            token_endpoint: "https://auth.kimi.com/api/oauth/token".to_string(),
        };
        store_tokens(&store, &tokens).unwrap();
        assert_eq!(load_tokens(&store).unwrap(), Some(tokens));
        store.set("not json").unwrap();
        assert!(load_tokens(&store)
            .unwrap_err()
            .starts_with("kimi credential store: unreadable entry"));
    }
}

//! Meta Muse OAuth credential acquisition: the RFC 8628 device authorization
//! grant against `auth.meta.com`, its polling state machine, the API-key mint
//! exchange (`api.meta.ai/muse-code/key`) and OS-vault persistence.
//!
//! Ports CLIProxyAPI `internal/auth/meta/meta.go` (and the storage half of
//! `MetaTokenStorage`/`CredentialFileName`). Additive and deliberately NOT
//! wired to a provider: clud's `ModelProvider` is a closed enum and a Meta
//! Muse account would need a descriptor, catalog rows and an upstream lane
//! (the executor speaks Meta's own Muse wire), so that is a separate slice.
//! This module is the credential half; the hidden `__meta-device-login` seam
//! is the only thing that reaches it today.
//!
//! Two credential shapes come out of a Meta login: the short-lived *device
//! client access* token (`dca:...`) and, once minted, a long-lived **API key**
//! which is the credential an LLM request actually uses. Upstream treats a
//! failed mint as non-fatal (the dca token still works until its deadline), and
//! so does this port.
//!
//! Nothing here prints or serializes a secret to stdout: [`MetaToken`] and
//! [`MintedKey`] render redacted, and the seam reports only the user code and
//! non-secret identity metadata.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::provider_auth::{NativeSecretStore, SecretStore};

/// Meta's public Muse Code OAuth client id (upstream `ClientID`).
pub const META_CLIENT_ID: &str = "1031625952748946";
/// Meta account authorization server.
pub const META_AUTH_HOST: &str = "https://auth.meta.com";
/// Device authorization endpoint (upstream `DeviceAuthorizationEndpoint`).
pub const META_DEVICE_AUTHORIZATION_ENDPOINT: &str =
    "https://auth.meta.com/oidc/device/authorization/";
/// Token endpoint (upstream `TokenEndpoint`).
pub const META_TOKEN_ENDPOINT: &str = "https://auth.meta.com/oidc/device/token/";
/// Default model API base (upstream `DefaultAPIBaseURL`); a mint response may
/// carry a different one.
pub const META_API_BASE_URL: &str = "https://api.meta.ai/v1";
/// API-key mint endpoint (upstream's `META_MINT_URL` fallback).
pub const META_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
/// RFC 8628 device-code grant type.
pub const META_DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// The CLI identity Meta expects on its own OAuth endpoints.
pub const META_USER_AGENT: &str = "muse-code/1.0.2";
/// Vault service for Meta credentials.
pub const META_VAULT_SERVICE: &str = "clud.meta";
/// Vault account for the device-grant record. A Meta *API key* would be its own
/// account, so an OAuth login can never overwrite a stored key.
pub const META_OAUTH_VAULT_ACCOUNT: &str = "oauth-device-v1";
/// Default poll interval when the device response omits `interval`.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
/// Upper bound on the whole device-code wait (upstream `MaxPollDuration`).
pub const MAX_POLL_SECONDS: u64 = 15 * 60;
/// How much a `slow_down` reply widens the poll interval (RFC 8628 §3.5).
pub const SLOW_DOWN_STEP_SECONDS: u64 = 5;
const HTTP_TIMEOUT_SECONDS: u64 = 30;
const REDACTED: &str = "[redacted]";

/// A Meta OAuth failure with the reason the operator needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaAuthError(String);

impl MetaAuthError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for MetaAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MetaAuthError {}

/// The three endpoints this module talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaEndpoints {
    pub device_authorization: String,
    pub token: String,
    pub mint: String,
}

impl Default for MetaEndpoints {
    fn default() -> Self {
        Self {
            device_authorization: META_DEVICE_AUTHORIZATION_ENDPOINT.to_string(),
            token: META_TOKEN_ENDPOINT.to_string(),
            mint: META_MINT_URL.to_string(),
        }
    }
}

/// A device-authorization response (RFC 8628 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub expires_in: Option<u64>,
    pub interval: Option<u64>,
}

/// The device client access token (`dca:...`).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetaToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<u64>,
}

impl fmt::Debug for MetaToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MetaToken")
            .field("access_token", &REDACTED)
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("expires_at_unix", &self.expires_at_unix)
            .finish()
    }
}

impl MetaToken {
    /// True when the recorded deadline has passed (with `skew_seconds` of slack).
    pub fn is_expired(&self, now_unix: u64, skew_seconds: u64) -> bool {
        self.expires_at_unix
            .is_some_and(|deadline| now_unix.saturating_add(skew_seconds) >= deadline)
    }
}

/// A minted Meta Model API key plus the account metadata that comes with it.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MintedKey {
    pub api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_full_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subs_tier_name: Option<String>,
    #[serde(default)]
    pub is_subs_active: bool,
    #[serde(default)]
    pub has_payment_method: bool,
    #[serde(default)]
    pub require_payment: bool,
    #[serde(default)]
    pub can_subscribe: bool,
}

impl fmt::Debug for MintedKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MintedKey")
            .field("api_key", &REDACTED)
            .field("base_url", &self.base_url)
            .field("user_email", &self.user_email)
            .field("subs_tier_name", &self.subs_tier_name)
            .field("is_subs_active", &self.is_subs_active)
            .finish()
    }
}

impl MintedKey {
    /// The base URL a request should use: the minted one when present.
    pub fn resolved_base_url(&self) -> String {
        self.base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .unwrap_or(META_API_BASE_URL)
            .to_string()
    }

    /// Non-secret account metadata for the seam's success event.
    pub fn summary(&self) -> Value {
        json!({
            "base_url": self.resolved_base_url(),
            "email": self.user_email,
            "name": self.user_full_name,
            "subs_tier_name": self.subs_tier_name,
            "subs_active": self.is_subs_active,
            "has_payment_method": self.has_payment_method,
            "require_payment": self.require_payment,
            "can_subscribe": self.can_subscribe,
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
    ) -> Result<HttpReply, MetaAuthError>;

    fn post_json(
        &mut self,
        url: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpReply, MetaAuthError>;
}

/// Time and sleeping, so the poll loop is testable without waiting.
pub trait OAuthClock {
    fn now_unix(&self) -> u64;
    fn sleep(&mut self, millis: u64);
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl OAuthClock for SystemClock {
    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default()
    }

    fn sleep(&mut self, millis: u64) {
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis));
        }
    }
}

/// `ureq`-backed transport: one POST with a 30s timeout.
#[derive(Debug, Clone, Copy, Default)]
pub struct UreqTransport;

impl UreqTransport {
    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS))
            .build()
    }
}

fn read_reply(response: ureq::Response) -> Result<HttpReply, MetaAuthError> {
    let status = response.status();
    let body = response
        .into_string()
        .map_err(|error| MetaAuthError::new(format!("meta oauth: read response: {error}")))?;
    Ok(HttpReply { status, body })
}

fn send(request: Result<ureq::Response, ureq::Error>) -> Result<HttpReply, MetaAuthError> {
    match request {
        Ok(response) => read_reply(response),
        // A non-2xx answer is data, not a transport failure: the body carries
        // the OAuth error code the state machine switches on.
        Err(ureq::Error::Status(status, response)) => Ok(HttpReply {
            status,
            body: response.into_string().unwrap_or_default(),
        }),
        Err(ureq::Error::Transport(transport)) => Err(MetaAuthError::new(format!(
            "meta oauth request failed: {transport}"
        ))),
    }
}

impl OAuthTransport for UreqTransport {
    fn post_form(
        &mut self,
        url: &str,
        form: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<HttpReply, MetaAuthError> {
        let mut request = Self::agent()
            .post(url)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .set("Accept", "application/json");
        for (name, value) in headers {
            request = request.set(name, value);
        }
        send(request.send_string(&form_encode(form)))
    }

    fn post_json(
        &mut self,
        url: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpReply, MetaAuthError> {
        let mut request = Self::agent()
            .post(url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
        for (name, value) in headers {
            request = request.set(name, value);
        }
        send(request.send_string(body))
    }
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

/// Accepts an absolute `https` URL, or `http` on loopback so tests can drive
/// the real state machine against a local server.
pub fn validate_endpoint(raw: &str, field: &str) -> Result<String, MetaAuthError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(MetaAuthError::new(format!(
            "meta {field} endpoint is empty"
        )));
    }
    let lower = trimmed.to_ascii_lowercase();
    let loopback = lower.starts_with("http://127.0.0.1:")
        || lower.starts_with("http://[::1]:")
        || lower.starts_with("http://localhost:");
    if lower.starts_with("https://") || loopback {
        return Ok(trimmed.to_string());
    }
    Err(MetaAuthError::new(format!(
        "meta {field} endpoint must be an https URL (loopback http is allowed for tests): {trimmed}"
    )))
}

fn user_agent_headers() -> [(&'static str, &'static str); 1] {
    [("User-Agent", META_USER_AGENT)]
}

fn text_field(payload: &Value, name: &str) -> Option<String> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn number_field(payload: &Value, name: &str) -> Option<u64> {
    payload
        .get(name)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
}

/// Parses a device-authorization response, refusing a record the flow cannot use.
pub fn parse_device_code(body: &str) -> Result<DeviceCode, MetaAuthError> {
    let payload: Value = serde_json::from_str(body).map_err(|error| {
        MetaAuthError::new(format!("meta device code: parse response: {error}"))
    })?;
    let device_code = text_field(&payload, "device_code")
        .ok_or_else(|| MetaAuthError::new("meta device code: response has no device_code"))?;
    let user_code = text_field(&payload, "user_code")
        .ok_or_else(|| MetaAuthError::new("meta device code: response has no user_code"))?;
    Ok(DeviceCode {
        device_code,
        user_code,
        verification_uri: text_field(&payload, "verification_uri"),
        verification_uri_complete: text_field(&payload, "verification_uri_complete"),
        expires_in: number_field(&payload, "expires_in"),
        interval: number_field(&payload, "interval"),
    })
}

/// Requests a device code and user code (RFC 8628 §3.1).
pub fn request_device_code(
    transport: &mut impl OAuthTransport,
    endpoints: &MetaEndpoints,
) -> Result<DeviceCode, MetaAuthError> {
    let reply = transport.post_form(
        &endpoints.device_authorization,
        &[("client_id", META_CLIENT_ID)],
        &user_agent_headers(),
    )?;
    if reply.status != 200 {
        return Err(MetaAuthError::new(format!(
            "meta device code request failed with status {}: {}",
            reply.status,
            reply.body.trim()
        )));
    }
    parse_device_code(&reply.body)
}

/// One poll attempt's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenPoll {
    Authorized(Box<MetaToken>),
    Pending,
    SlowDown,
}

fn token_from_payload(payload: &Value, now_unix: u64) -> Result<MetaToken, MetaAuthError> {
    let access_token = text_field(payload, "access_token")
        .ok_or_else(|| MetaAuthError::new("meta token response carries no access token"))?;
    let expires_in = number_field(payload, "expires_in");
    Ok(MetaToken {
        access_token,
        token_type: text_field(payload, "token_type"),
        expires_in,
        expires_at_unix: expires_in.map(|seconds| now_unix.saturating_add(seconds)),
    })
}

fn classify_token_reply(reply: &HttpReply, now_unix: u64) -> Result<TokenPoll, MetaAuthError> {
    let payload: Value = serde_json::from_str(&reply.body).map_err(|error| {
        MetaAuthError::new(format!(
            "meta token response: parse body (status {}): {error}",
            reply.status
        ))
    })?;
    if reply.status == 200 {
        // Meta answers 200 only once the device is authorized; a 200 carrying an
        // `error` field is still an error, so it is classified below.
        if text_field(&payload, "error").is_none() {
            return Ok(TokenPoll::Authorized(Box::new(token_from_payload(
                &payload, now_unix,
            )?)));
        }
    }
    let code = text_field(&payload, "error").unwrap_or_default();
    let description = text_field(&payload, "error_description").unwrap_or_default();
    match code.as_str() {
        "authorization_pending" => Ok(TokenPoll::Pending),
        "slow_down" => Ok(TokenPoll::SlowDown),
        "access_denied" => Err(MetaAuthError::new("meta device authorization denied")),
        "expired_token" => Err(MetaAuthError::new("meta device code expired")),
        "" => Err(MetaAuthError::new(format!(
            "meta token endpoint answered status {} without an OAuth error: {}",
            reply.status,
            reply.body.trim()
        ))),
        other => Err(MetaAuthError::new(format!(
            "meta authorization server error: {other}: {description}"
        ))),
    }
}

/// Poll bounds for the device grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollConfig {
    pub min_interval_ms: u64,
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

/// Polls the token endpoint until the user approves the device code.
///
/// The wait is bounded by the shorter of the code's own `expires_in` and
/// [`PollConfig::max_poll_ms`], so a code that was already dead cannot keep a
/// process polling.
pub fn poll_for_authorization(
    transport: &mut impl OAuthTransport,
    clock: &mut impl OAuthClock,
    endpoints: &MetaEndpoints,
    device: &DeviceCode,
    config: &PollConfig,
) -> Result<MetaToken, MetaAuthError> {
    let mut interval_ms = device
        .interval
        .map(|seconds| seconds.saturating_mul(1_000))
        .unwrap_or(config.min_interval_ms)
        .max(config.min_interval_ms);
    let mut max_poll_ms = config.max_poll_ms;
    if let Some(expires_in) = device.expires_in {
        max_poll_ms = max_poll_ms.min(expires_in.saturating_mul(1_000));
    }
    let deadline = clock
        .now_unix()
        .saturating_mul(1_000)
        .saturating_add(max_poll_ms);
    loop {
        if clock.now_unix().saturating_mul(1_000) >= deadline {
            return Err(MetaAuthError::new(
                "meta device authorization timed out before approval",
            ));
        }
        clock.sleep(interval_ms);
        let reply = transport.post_form(
            &endpoints.token,
            &[
                ("grant_type", META_DEVICE_GRANT_TYPE),
                ("device_code", device.device_code.as_str()),
                ("client_id", META_CLIENT_ID),
            ],
            &user_agent_headers(),
        )?;
        match classify_token_reply(&reply, clock.now_unix())? {
            TokenPoll::Authorized(token) => return Ok(*token),
            TokenPoll::Pending => {}
            TokenPoll::SlowDown => {
                interval_ms = interval_ms.saturating_add(SLOW_DOWN_STEP_SECONDS * 1_000);
            }
        }
    }
}

/// Exchanges a device client access token for a Meta Model API key.
pub fn mint_api_key(
    transport: &mut impl OAuthTransport,
    endpoints: &MetaEndpoints,
    dca_token: &str,
) -> Result<MintedKey, MetaAuthError> {
    let token = dca_token.trim();
    if token.is_empty() {
        return Err(MetaAuthError::new("meta mint: missing dca token"));
    }
    let body = json!({ "dca_token": token }).to_string();
    let authorization = format!("Bearer {token}");
    let reply = transport.post_json(
        &endpoints.mint,
        &body,
        &[
            ("Authorization", authorization.as_str()),
            ("User-Agent", META_USER_AGENT),
        ],
    )?;
    if reply.status < 200 || reply.status >= 300 {
        return Err(MetaAuthError::new(format!(
            "meta mint key failed (HTTP {}): {}",
            reply.status,
            reply.body.trim()
        )));
    }
    parse_minted_key(&reply.body)
}

/// Parses a mint response, refusing one without a usable key.
pub fn parse_minted_key(body: &str) -> Result<MintedKey, MetaAuthError> {
    let payload: Value = serde_json::from_str(body)
        .map_err(|error| MetaAuthError::new(format!("meta mint: parse response: {error}")))?;
    let api_key = text_field(&payload, "api_key")
        .ok_or_else(|| MetaAuthError::new("meta mint response has no api_key"))?;
    let flag = |name: &str| payload.get(name).and_then(Value::as_bool).unwrap_or(false);
    Ok(MintedKey {
        api_key,
        base_url: text_field(&payload, "base_url"),
        user_email: text_field(&payload, "user_email"),
        user_full_name: text_field(&payload, "user_full_name"),
        subs_tier_name: text_field(&payload, "subs_tier_name"),
        is_subs_active: flag("is_subs_active"),
        has_payment_method: flag("has_payment_method"),
        require_payment: flag("require_payment"),
        can_subscribe: flag("can_subscribe"),
    })
}

/// A completed Meta login: the device token and, when the mint succeeded, the
/// API key that request paths should actually use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaLogin {
    pub token: MetaToken,
    pub minted: Option<MintedKey>,
    /// Why the mint was skipped or failed; a failed mint is non-fatal upstream.
    pub mint_problem: Option<String>,
}

impl MetaLogin {
    /// Non-secret metadata for the seam's success event.
    pub fn summary(&self) -> Value {
        let mut summary = json!({
            "has_api_key": self.minted.is_some(),
            "token_type": self.token.token_type,
            "expires_at_unix": self.token.expires_at_unix,
        });
        if let Some(minted) = &self.minted {
            summary["account"] = minted.summary();
        }
        if let Some(problem) = &self.mint_problem {
            summary["mint_problem"] = json!(problem);
        }
        summary
    }
}

/// The persisted Meta credential: one JSON record in the OS vault.
///
/// `access_token` is the credential a request path uses — the minted API key
/// when there is one, else the still-valid dca token (upstream's
/// `CreateTokenStorage` precedence).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaCredential {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dca_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The dca token's deadline; unset once an API key exists, because a key
    /// does not expire on the device-code timer (upstream clears `expired`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dca_expires_at_unix: Option<u64>,
    pub last_login_unix: u64,
}

impl MetaCredential {
    /// True when the only credential held is an expired dca token.
    pub fn is_expired(&self, now_unix: u64, skew_seconds: u64) -> bool {
        match self.api_key.as_deref() {
            Some(key) if !key.trim().is_empty() => false,
            _ => self
                .dca_expires_at_unix
                .is_some_and(|deadline| now_unix.saturating_add(skew_seconds) >= deadline),
        }
    }

    /// Non-secret metadata; never the api key or dca token.
    pub fn summary(&self) -> Value {
        json!({
            "base_url": self.base_url,
            "email": self.email,
            "name": self.name,
            "has_api_key": self.api_key.is_some(),
            "dca_expires_at_unix": self.dca_expires_at_unix,
            "last_login_unix": self.last_login_unix,
        })
    }
}

/// Builds the persisted record from a completed login.
pub fn credential_from(login: &MetaLogin, now_unix: u64) -> MetaCredential {
    let api_key = login
        .minted
        .as_ref()
        .map(|minted| minted.api_key.clone())
        .filter(|key| !key.trim().is_empty());
    let base_url = login
        .minted
        .as_ref()
        .map(MintedKey::resolved_base_url)
        .unwrap_or_else(|| META_API_BASE_URL.to_string());
    MetaCredential {
        access_token: api_key
            .clone()
            .unwrap_or_else(|| login.token.access_token.clone()),
        dca_token: Some(login.token.access_token.clone()),
        dca_expires_at_unix: api_key
            .is_none()
            .then_some(login.token.expires_at_unix)
            .flatten(),
        api_key,
        token_type: login.token.token_type.clone(),
        base_url,
        email: login
            .minted
            .as_ref()
            .and_then(|minted| minted.user_email.clone()),
        name: login
            .minted
            .as_ref()
            .and_then(|minted| minted.user_full_name.clone()),
        last_login_unix: now_unix,
    }
}

/// Store a credential bundle in the vault.
pub fn store_credential(
    store: &dyn SecretStore,
    credential: &MetaCredential,
) -> Result<(), String> {
    let encoded = serde_json::to_string(credential)
        .map_err(|error| format!("meta credential encode: {error}"))?;
    store
        .set(&encoded)
        .map_err(|error| format!("meta credential store: {error}"))
}

/// Load a stored credential, if the vault holds one.
pub fn load_credential(store: &dyn SecretStore) -> Result<Option<MetaCredential>, String> {
    let Some(raw) = store
        .get()
        .map_err(|error| format!("meta credential store: {error}"))?
    else {
        return Ok(None);
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| format!("meta credential store: unreadable entry: {error}"))
}

/// The OS-vault-backed store for Meta credentials.
pub fn vault_store() -> Result<NativeSecretStore, String> {
    NativeSecretStore::new_for(META_VAULT_SERVICE, META_OAUTH_VAULT_ACCOUNT)
        .map_err(|error| format!("meta credential store: {error}"))
}

/// Arguments for the hidden test/automation seam. No production path calls it.
#[derive(Debug, Clone, Default)]
pub struct SeamArgs<'a> {
    pub device_endpoint: Option<&'a str>,
    pub token_endpoint: Option<&'a str>,
    pub mint_url: Option<&'a str>,
    pub interval_ms: Option<u64>,
    pub max_poll_ms: Option<u64>,
    /// Print the stored credential summary and exit without any network call.
    pub status: bool,
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
/// Prints one JSON object per line. The device user code is echoed because the
/// operator needs it; no access token or API key is ever printed.
pub fn run_seam(args: &SeamArgs<'_>) -> i32 {
    match run_seam_inner(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("meta device login: {error}");
            1
        }
    }
}

fn seam_endpoints(args: &SeamArgs<'_>) -> Result<MetaEndpoints, MetaAuthError> {
    match (args.device_endpoint, args.token_endpoint) {
        (Some(device), Some(token)) => Ok(MetaEndpoints {
            device_authorization: validate_endpoint(device, "device_authorization")?,
            token: validate_endpoint(token, "token")?,
            mint: match args.mint_url {
                Some(mint) => validate_endpoint(mint, "mint")?,
                None => META_MINT_URL.to_string(),
            },
        }),
        (None, None) => Ok(MetaEndpoints {
            mint: match args.mint_url {
                Some(mint) => validate_endpoint(mint, "mint")?,
                None => META_MINT_URL.to_string(),
            },
            ..MetaEndpoints::default()
        }),
        _ => Err(MetaAuthError::new(
            "both --device-endpoint and --token-endpoint are required",
        )),
    }
}

fn run_seam_inner(args: &SeamArgs<'_>) -> Result<(), MetaAuthError> {
    let endpoints = seam_endpoints(args)?;
    let store = vault_store().map_err(MetaAuthError::new)?;
    if args.status {
        let stored = load_credential(&store).map_err(MetaAuthError::new)?;
        match stored {
            Some(credential) => println!(
                "{}",
                json!({"event": "status", "details": credential.summary()})
            ),
            None => println!("{}", json!({"event": "status", "details": null})),
        }
        return Ok(());
    }
    let mut transport = UreqTransport;
    let mut clock = SystemClock;
    let device = request_device_code(&mut transport, &endpoints)?;
    println!(
        "{}",
        json!({
            "event": "device_code",
            "user_code": device.user_code,
            "verification_uri": device.verification_uri,
            "verification_uri_complete": device.verification_uri_complete,
            "expires_in": device.expires_in,
            "interval": device.interval,
        })
    );
    let token = poll_for_authorization(
        &mut transport,
        &mut clock,
        &endpoints,
        &device,
        &args.poll_config(),
    )?;
    let (minted, mint_problem) = match mint_api_key(&mut transport, &endpoints, &token.access_token)
    {
        Ok(minted) => (Some(minted), None),
        // Upstream logs and continues: the dca token is still usable until its
        // own deadline, so a failed mint must not throw away a real login.
        Err(error) => (None, Some(error.to_string())),
    };
    let login = MetaLogin {
        token,
        minted,
        mint_problem,
    };
    let credential = credential_from(&login, clock.now_unix());
    store_credential(&store, &credential).map_err(MetaAuthError::new)?;
    println!(
        "{}",
        json!({"event": "authorized", "details": login.summary()})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_auth::SecretStoreError;
    use std::cell::RefCell;
    use std::rc::Rc;

    type RecordedRequest = (String, String, Vec<(String, String)>);

    #[derive(Default, Clone)]
    struct ScriptedTransport {
        replies: Rc<RefCell<Vec<HttpReply>>>,
        requests: Rc<RefCell<Vec<RecordedRequest>>>,
    }

    impl ScriptedTransport {
        fn new(replies: Vec<HttpReply>) -> Self {
            Self {
                replies: Rc::new(RefCell::new(replies)),
                requests: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn record(&self, url: &str, body: &str, headers: &[(&str, &str)]) {
            self.requests.borrow_mut().push((
                url.to_string(),
                body.to_string(),
                headers
                    .iter()
                    .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                    .collect(),
            ));
        }

        fn next_reply(&self) -> HttpReply {
            let mut replies = self.replies.borrow_mut();
            assert!(!replies.is_empty(), "scripted transport ran out of replies");
            replies.remove(0)
        }
    }

    impl OAuthTransport for ScriptedTransport {
        fn post_form(
            &mut self,
            url: &str,
            form: &[(&str, &str)],
            headers: &[(&str, &str)],
        ) -> Result<HttpReply, MetaAuthError> {
            self.record(url, &form_encode(form), headers);
            Ok(self.next_reply())
        }

        fn post_json(
            &mut self,
            url: &str,
            body: &str,
            headers: &[(&str, &str)],
        ) -> Result<HttpReply, MetaAuthError> {
            self.record(url, body, headers);
            Ok(self.next_reply())
        }
    }

    #[derive(Default, Clone)]
    struct FakeClock {
        now: Rc<RefCell<u64>>,
        slept: Rc<RefCell<Vec<u64>>>,
    }

    impl OAuthClock for FakeClock {
        fn now_unix(&self) -> u64 {
            *self.now.borrow()
        }

        fn sleep(&mut self, millis: u64) {
            self.slept.borrow_mut().push(millis);
            *self.now.borrow_mut() += millis / 1_000;
        }
    }

    #[derive(Default)]
    struct MemoryStore {
        value: RefCell<Option<String>>,
    }

    impl SecretStore for MemoryStore {
        fn get(&self) -> Result<Option<String>, SecretStoreError> {
            Ok(self.value.borrow().clone())
        }

        fn set(&self, secret: &str) -> Result<(), SecretStoreError> {
            *self.value.borrow_mut() = Some(secret.to_string());
            Ok(())
        }

        fn delete(&self) -> Result<(), SecretStoreError> {
            *self.value.borrow_mut() = None;
            Ok(())
        }
    }

    fn reply(status: u16, body: &str) -> HttpReply {
        HttpReply {
            status,
            body: body.to_string(),
        }
    }

    fn device() -> DeviceCode {
        DeviceCode {
            device_code: "device-abc".to_string(),
            user_code: "USER-123".to_string(),
            verification_uri: Some("https://meta.com/device".to_string()),
            verification_uri_complete: None,
            expires_in: Some(60),
            interval: Some(1),
        }
    }

    fn token_login() -> MetaLogin {
        MetaLogin {
            token: MetaToken {
                access_token: "dca:secret".to_string(),
                token_type: Some("Bearer".to_string()),
                expires_in: Some(3600),
                expires_at_unix: Some(1_000),
            },
            minted: Some(MintedKey {
                api_key: "mk-secret".to_string(),
                base_url: Some("https://api.meta.ai/v1".to_string()),
                user_email: Some("zach@example.com".to_string()),
                user_full_name: Some("Zach".to_string()),
                subs_tier_name: Some("Muse Code".to_string()),
                is_subs_active: true,
                has_payment_method: true,
                require_payment: false,
                can_subscribe: true,
            }),
            mint_problem: None,
        }
    }

    #[test]
    fn device_code_requires_the_codes_the_flow_needs() {
        let parsed = parse_device_code(
            r#"{"device_code":"d","user_code":"U","expires_in":30,"interval":2}"#,
        )
        .expect("valid device code");
        assert_eq!(parsed.user_code, "U");
        assert_eq!(parsed.expires_in, Some(30));
        assert!(parse_device_code(r#"{"user_code":"U"}"#).is_err());
        assert!(parse_device_code(r#"{"device_code":"d"}"#).is_err());
        assert!(parse_device_code("not json").is_err());
    }

    #[test]
    fn device_request_sends_the_client_id_and_user_agent() {
        let mut transport = ScriptedTransport::new(vec![reply(
            200,
            r#"{"device_code":"d","user_code":"U","verification_uri":"https://meta.com/d","expires_in":60,"interval":1}"#,
        )]);
        let endpoints = MetaEndpoints::default();
        let device = request_device_code(&mut transport, &endpoints).expect("device code");
        assert_eq!(device.device_code, "d");
        let requests = transport.requests.borrow();
        assert_eq!(requests[0].0, META_DEVICE_AUTHORIZATION_ENDPOINT);
        assert_eq!(requests[0].1, format!("client_id={META_CLIENT_ID}"));
        assert!(requests[0]
            .2
            .iter()
            .any(|(name, value)| name == "User-Agent" && value == META_USER_AGENT));
    }

    #[test]
    fn a_non_200_device_reply_names_the_status() {
        let mut transport = ScriptedTransport::new(vec![reply(400, "nope")]);
        let error = request_device_code(&mut transport, &MetaEndpoints::default())
            .expect_err("400 must fail");
        assert!(error.to_string().contains("status 400"));
    }

    #[test]
    fn polling_survives_pending_and_widens_on_slow_down() {
        let mut transport = ScriptedTransport::new(vec![
            reply(400, r#"{"error":"authorization_pending"}"#),
            reply(400, r#"{"error":"slow_down"}"#),
            reply(
                200,
                r#"{"access_token":"dca:1","token_type":"Bearer","expires_in":600}"#,
            ),
        ]);
        let mut clock = FakeClock::default();
        let token = poll_for_authorization(
            &mut transport,
            &mut clock,
            &MetaEndpoints::default(),
            &device(),
            &PollConfig {
                min_interval_ms: 1_000,
                max_poll_ms: 60_000,
            },
        )
        .expect("authorized");
        assert_eq!(token.access_token, "dca:1");
        // The deadline is measured from the moment the token arrived.
        assert_eq!(token.expires_at_unix, Some(clock.now_unix() + 600));
        // 1s for the pending poll, 1s again, then +5s after slow_down.
        assert_eq!(*clock.slept.borrow(), vec![1_000, 1_000, 6_000]);
    }

    #[test]
    fn a_200_carrying_an_error_is_not_an_authorization() {
        let mut transport = ScriptedTransport::new(vec![reply(
            200,
            r#"{"error":"access_denied","error_description":"no"}"#,
        )]);
        let error = poll_for_authorization(
            &mut transport,
            &mut FakeClock::default(),
            &MetaEndpoints::default(),
            &device(),
            &PollConfig::default(),
        )
        .expect_err("denied");
        assert!(error.to_string().contains("denied"));
    }

    #[test]
    fn a_dead_code_fails_fast_without_polling_forever() {
        let mut transport = ScriptedTransport::new(vec![]);
        let error = poll_for_authorization(
            &mut transport,
            &mut FakeClock::default(),
            &MetaEndpoints::default(),
            &device(),
            &PollConfig {
                min_interval_ms: 1_000,
                max_poll_ms: 0,
            },
        )
        .expect_err("bounded wait");
        assert!(error.to_string().contains("timed out"));
        assert!(transport.requests.borrow().is_empty());
    }

    #[test]
    fn expired_token_and_unknown_errors_are_typed() {
        let mut expired = ScriptedTransport::new(vec![reply(400, r#"{"error":"expired_token"}"#)]);
        let error = poll_for_authorization(
            &mut expired,
            &mut FakeClock::default(),
            &MetaEndpoints::default(),
            &device(),
            &PollConfig::default(),
        )
        .expect_err("expired");
        assert!(error.to_string().contains("expired"));

        let mut unknown = ScriptedTransport::new(vec![reply(500, r#"{"error":"server_error"}"#)]);
        let error = poll_for_authorization(
            &mut unknown,
            &mut FakeClock::default(),
            &MetaEndpoints::default(),
            &device(),
            &PollConfig::default(),
        )
        .expect_err("unknown");
        assert!(error.to_string().contains("server_error"));
    }

    #[test]
    fn mint_sends_the_bearer_and_reads_the_account() {
        let mut transport = ScriptedTransport::new(vec![reply(
            200,
            r#"{"api_key":"mk-1","base_url":"https://muse.example/v1","user_email":"z@example.com","user_full_name":"Z","subs_tier_name":"Muse Code","is_subs_active":true}"#,
        )]);
        let minted =
            mint_api_key(&mut transport, &MetaEndpoints::default(), "dca:1").expect("mint");
        assert_eq!(minted.api_key, "mk-1");
        assert_eq!(minted.resolved_base_url(), "https://muse.example/v1");
        assert_eq!(minted.user_email.as_deref(), Some("z@example.com"));
        assert!(minted.is_subs_active);
        let requests = transport.requests.borrow();
        assert_eq!(requests[0].2[0].0, "Authorization");
        assert_eq!(requests[0].2[0].1, "Bearer dca:1");
        assert!(requests[0].1.contains("dca:1"));
    }

    #[test]
    fn a_mint_without_a_key_is_an_error() {
        let mut transport = ScriptedTransport::new(vec![reply(200, r#"{"base_url":"x"}"#)]);
        let error =
            mint_api_key(&mut transport, &MetaEndpoints::default(), "dca:1").expect_err("no key");
        assert!(error.to_string().contains("api_key"));
    }

    #[test]
    fn the_api_key_is_the_credential_and_clears_the_dca_deadline() {
        let credential = credential_from(&token_login(), 42);
        assert_eq!(credential.access_token, "mk-secret");
        assert_eq!(credential.api_key.as_deref(), Some("mk-secret"));
        assert_eq!(credential.base_url, "https://api.meta.ai/v1");
        assert_eq!(credential.dca_expires_at_unix, None);
        assert!(!credential.is_expired(999_999, 0));
    }

    #[test]
    fn a_dca_only_login_keeps_its_deadline_and_expires() {
        let mut login = token_login();
        login.minted = None;
        login.mint_problem = Some("mint failed".to_string());
        let credential = credential_from(&login, 42);
        assert_eq!(credential.access_token, "dca:secret");
        assert_eq!(credential.dca_expires_at_unix, Some(1_000));
        assert!(!credential.is_expired(900, 0));
        assert!(credential.is_expired(1_000, 0));
    }

    #[test]
    fn summaries_and_debug_never_render_a_secret() {
        let login = token_login();
        let rendered = login.summary().to_string();
        assert!(!rendered.contains("mk-secret"));
        assert!(rendered.contains("zach@example.com"));
        let credential = credential_from(&login, 42);
        let summary = credential.summary().to_string();
        assert!(!summary.contains("mk-secret"));
        assert!(!summary.contains("dca:secret"));
        assert!(!format!("{:?}", login.token).contains("dca:secret"));
        assert!(!format!("{:?}", login.minted).contains("mk-secret"));
    }

    #[test]
    fn endpoint_validation_accepts_https_and_loopback_http_only() {
        assert!(validate_endpoint("https://auth.meta.com/x", "token").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:8080/token", "token").is_ok());
        assert!(validate_endpoint("http://localhost:9999/token", "token").is_ok());
        assert!(validate_endpoint("http://evil.example/token", "token").is_err());
        assert!(validate_endpoint("  ", "token").is_err());
    }

    #[test]
    fn the_credential_round_trips_through_a_store() {
        let store = MemoryStore::default();
        assert_eq!(load_credential(&store).expect("empty"), None);
        let credential = credential_from(&token_login(), 42);
        store_credential(&store, &credential).expect("store");
        let loaded = load_credential(&store).expect("load").expect("some");
        assert_eq!(loaded, credential);
    }
}

//! xAI (Grok) OAuth device-authorization login (RFC 8628).
//!
//! Ported from CLIProxyAPI `internal/auth/xai/{xai,types,token}.go`: OIDC
//! discovery of the device/token endpoints, the device-code request, the
//! polling state machine (`authorization_pending` / `slow_down` /
//! `expired_token` / `access_denied`) and refresh-token rotation.
//!
//! This is the credential-acquisition slice of the xAI provider port. It owns
//! no route and does not add a `ModelProvider` variant: the xAI provider row
//! (descriptor + catalog + responses executor) lands on top of it, so the
//! login path can be reviewed and tested on its own. Everything here is
//! additive — no existing provider's credential flow changes.
//!
//! Security posture matches the rest of clud: the client id and scope are the
//! public Grok CLI OAuth values upstream uses (never secrets), tokens are
//! persisted through the OS vault (`SecretStore`), and an endpoint returned by
//! discovery is only accepted when it is HTTPS on `x.ai`. Loopback HTTP is
//! accepted for tests only, so a fake OAuth server can drive the real state
//! machine without weakening the production path.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::provider_auth::{NativeSecretStore, SecretStore};

/// xAI's OAuth issuer.
pub const XAI_ISSUER: &str = "https://auth.x.ai";
/// OIDC discovery document used to resolve the device and token endpoints.
pub const XAI_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
/// Public Grok CLI OAuth client id (upstream `xai/types.go`).
pub const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// OAuth scopes the Grok CLI requests.
pub const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// API base URL a token is used against.
pub const XAI_API_BASE_URL: &str = "https://api.x.ai/v1";
/// RFC 8628 grant type.
pub const XAI_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Vault identifiers for the xAI OAuth token bundle. Frozen: renaming these
/// orphans every stored login.
pub const XAI_VAULT_SERVICE: &str = "clud.xai";
pub const XAI_VAULT_ACCOUNT: &str = "oauth";

/// Default interval when the device endpoint omits `interval` (RFC 8628 §3.2).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Bound on a single credential HTTP call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on the whole polling wait.
const MAX_POLL_DURATION: Duration = Duration::from_secs(30 * 60);

/// Endpoints resolved from xAI's OIDC discovery document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XaiEndpoints {
    pub device_authorization: String,
    pub token: String,
}

/// A device authorization response (RFC 8628 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: Option<u64>,
    pub interval: Option<u64>,
    pub token_endpoint: String,
}

/// A persisted xAI credential bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XaiTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    /// Absolute expiry, unix seconds. `None` when the server omitted a lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub token_endpoint: String,
    pub api_base_url: String,
}

impl XaiTokens {
    /// True when the access token is expired or inside `skew` seconds of it.
    pub fn is_expired(&self, now_unix: u64, skew: u64) -> bool {
        match self.expires_at_unix {
            Some(expires_at) => now_unix.saturating_add(skew) >= expires_at,
            None => false,
        }
    }

    /// True when this bundle can be renewed without a fresh device login.
    pub fn can_refresh(&self) -> bool {
        self.refresh_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
    }
}

/// One poll attempt against the token endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PollAttempt {
    Authorized(Box<XaiTokens>),
    Pending,
    SlowDown,
}

/// The device-flow client. `new()` talks to the real xAI endpoints; tests use
/// `for_test` to shorten polling.
#[derive(Clone)]
pub struct XaiOAuth {
    agent: ureq::Agent,
    discovery_url: String,
    min_poll_interval: Duration,
    max_poll_duration: Duration,
}

impl Default for XaiOAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl XaiOAuth {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_POLL_INTERVAL, MAX_POLL_DURATION)
    }

    /// Build a client with explicit polling bounds (tests / seams).
    pub fn with_limits(min_poll_interval: Duration, max_poll_duration: Duration) -> Self {
        Self {
            agent: ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build(),
            discovery_url: XAI_DISCOVERY_URL.to_string(),
            min_poll_interval,
            max_poll_duration,
        }
    }

    /// Point discovery at another URL. Only the test seam uses this.
    pub fn with_discovery_url(mut self, url: impl Into<String>) -> Self {
        self.discovery_url = url.into();
        self
    }

    /// Resolve the device and token endpoints from OIDC discovery.
    pub fn discover(&self) -> Result<XaiEndpoints, String> {
        let body = self.get_json(&self.discovery_url, "xai discovery")?;
        let parsed: DiscoveryDocument =
            serde_json::from_str(&body).map_err(|error| format!("xai discovery: {error}"))?;
        Ok(XaiEndpoints {
            device_authorization: validate_endpoint(
                &parsed.device_authorization_endpoint,
                "device_authorization_endpoint",
            )?,
            token: validate_endpoint(&parsed.token_endpoint, "token_endpoint")?,
        })
    }

    /// Request a device code, echoing the token endpoint that will poll it.
    pub fn request_device_code(&self, endpoints: &XaiEndpoints) -> Result<DeviceCode, String> {
        let form = [("client_id", XAI_CLIENT_ID), ("scope", XAI_SCOPE)];
        let (status, body) = self.post_form(&endpoints.device_authorization, &form)?;
        if status != 200 {
            return Err(format!(
                "xai device code request failed with status {status}: {}",
                body.trim()
            ));
        }
        let raw: RawDeviceCode =
            serde_json::from_str(&body).map_err(|e| format!("xai device code: {e}"))?;
        let device_code = raw.device_code.trim().to_string();
        if device_code.is_empty() {
            return Err("xai device code: response missing device_code".to_string());
        }
        let user_code = raw.user_code.trim().to_string();
        if user_code.is_empty() {
            return Err("xai device code: response missing user_code".to_string());
        }
        let verification_uri = raw.verification_uri.trim().to_string();
        let complete = raw
            .verification_uri_complete
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if verification_uri.is_empty() && complete.is_none() {
            return Err("xai device code: response missing verification URI".to_string());
        }
        Ok(DeviceCode {
            device_code,
            user_code,
            verification_uri,
            verification_uri_complete: complete,
            expires_in: raw.expires_in,
            interval: raw.interval,
            token_endpoint: endpoints.token.clone(),
        })
    }

    /// Poll until the user authorizes the device, denying it, or the code
    /// expires. Returns the credential bundle on success.
    pub fn wait_for_authorization(&self, device: &DeviceCode) -> Result<XaiTokens, String> {
        let mut interval = poll_interval(device.interval, self.min_poll_interval);
        let start = Instant::now();
        let deadline = code_deadline(device.expires_in, self.max_poll_duration);
        loop {
            match self.exchange_device_code(&device.token_endpoint, &device.device_code)? {
                PollAttempt::Authorized(tokens) => return Ok(*tokens),
                PollAttempt::Pending => {}
                PollAttempt::SlowDown => {
                    interval = slow_down(interval, self.min_poll_interval);
                }
            }
            // RFC 8628 §3.5: sleep between attempts, and never past the code's
            // own lifetime (the next poll could only fail with expired_token).
            if start.elapsed().saturating_add(interval) >= deadline {
                return Err("xai device code expired".to_string());
            }
            std::thread::sleep(interval);
        }
    }

    /// Exchange one poll attempt. `slow_down` grows the caller's interval via
    /// `slow_down`, so the attempt reports only the protocol outcome.
    fn exchange_device_code(
        &self,
        token_endpoint: &str,
        device_code: &str,
    ) -> Result<PollAttempt, String> {
        let form = [
            ("grant_type", XAI_DEVICE_CODE_GRANT_TYPE),
            ("device_code", device_code),
            ("client_id", XAI_CLIENT_ID),
        ];
        let (status, body) = self.post_form(token_endpoint, &form)?;
        let parsed: RawTokenResponse =
            serde_json::from_str(&body).map_err(|e| format!("xai device token: {e}"))?;
        if let Some(error) = parsed.error.as_deref().filter(|e| !e.is_empty()) {
            return match error {
                "authorization_pending" => Ok(PollAttempt::Pending),
                // RFC 8628 §3.5: the client must widen its interval.
                "slow_down" => Ok(PollAttempt::SlowDown),
                "expired_token" => Err("xai device code expired".to_string()),
                "access_denied" => Err("xai device authorization denied".to_string()),
                other => Err(match parsed.error_description.as_deref() {
                    Some(description) if !description.is_empty() => {
                        format!("xai device token error: {other}: {description}")
                    }
                    _ => format!("xai device token error: {other}"),
                }),
            };
        }
        if status != 200 {
            return Err(format!(
                "xai device token request failed with status {status}: {}",
                body.trim()
            ));
        }
        Ok(PollAttempt::Authorized(Box::new(token_bundle(
            parsed,
            token_endpoint,
        )?)))
    }

    /// Redeem a refresh token for a fresh access token.
    pub fn refresh(&self, refresh_token: &str, token_endpoint: &str) -> Result<XaiTokens, String> {
        let refresh_token = refresh_token.trim();
        if refresh_token.is_empty() {
            return Err("xai token refresh: refresh token is required".to_string());
        }
        let endpoint = if token_endpoint.trim().is_empty() {
            self.discover()?.token
        } else {
            validate_endpoint(token_endpoint, "token_endpoint")?
        };
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", XAI_CLIENT_ID),
            ("refresh_token", refresh_token),
        ];
        let (status, body) = self.post_form(&endpoint, &form)?;
        if status != 200 {
            return Err(format!(
                "xai token refresh failed with status {status}: {}",
                body.trim()
            ));
        }
        let parsed: RawTokenResponse =
            serde_json::from_str(&body).map_err(|e| format!("xai token response: {e}"))?;
        token_bundle(parsed, &endpoint)
    }

    fn get_json(&self, url: &str, what: &str) -> Result<String, String> {
        match self.agent.get(url).set("Accept", "application/json").call() {
            Ok(response) => response
                .into_string()
                .map_err(|error| format!("{what}: read response: {error}")),
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(format!(
                    "{what} failed with status {status}: {}",
                    body.trim()
                ))
            }
            Err(error) => Err(format!("{what}: request failed: {error}")),
        }
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), String> {
        let request = self
            .agent
            .post(url)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .set("Accept", "application/json");
        match request.send_form(form) {
            Ok(response) => {
                let status = response.status();
                let body = response
                    .into_string()
                    .map_err(|error| format!("xai token: read response: {error}"))?;
                Ok((status, body))
            }
            // ureq reports non-2xx as Err(Status); the body still carries the
            // OAuth error code on 400 responses.
            Err(ureq::Error::Status(status, response)) => {
                Ok((status, response.into_string().unwrap_or_default()))
            }
            Err(error) => Err(format!("xai token: request failed: {error}")),
        }
    }
}

/// Persist a bundle in the OS vault (JSON), replacing any earlier login.
pub fn store_tokens(store: &dyn SecretStore, tokens: &XaiTokens) -> Result<(), String> {
    let encoded = serde_json::to_string(tokens).map_err(|error| error.to_string())?;
    store
        .set(&encoded)
        .map_err(|error| format!("xai credential store: {error}"))
}

/// Load a stored bundle, if the vault holds one.
pub fn load_tokens(store: &dyn SecretStore) -> Result<Option<XaiTokens>, String> {
    let Some(raw) = store
        .get()
        .map_err(|error| format!("xai credential store: {error}"))?
    else {
        return Ok(None);
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| format!("xai credential store: unreadable entry: {error}"))
}

/// The OS-vault-backed store for xAI OAuth tokens.
pub fn vault_store() -> Result<NativeSecretStore, String> {
    NativeSecretStore::new_for(XAI_VAULT_SERVICE, XAI_VAULT_ACCOUNT)
        .map_err(|error| format!("xai credential store: {error}"))
}

/// Validate an endpoint from a discovery document.
///
/// Production rule: HTTPS on `x.ai` (upstream `ValidateOAuthEndpoint`). A
/// loopback HTTP URL is accepted so the device-flow tests and the hidden seam
/// can drive the real state machine against a local server; nothing else
/// downgrades.
pub fn validate_endpoint(raw_url: &str, field: &str) -> Result<String, String> {
    let raw_url = raw_url.trim();
    if raw_url.is_empty() {
        return Err(format!("xai discovery {field} is empty"));
    }
    let rest = if let Some(rest) = raw_url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = raw_url.strip_prefix("http://") {
        let host_port = rest.split('/').next().unwrap_or_default();
        let host = host_port.split(':').next().unwrap_or_default();
        if !is_loopback_host(host) {
            return Err(format!("xai discovery {field} must use https: {raw_url:?}"));
        }
        rest
    } else {
        return Err(format!("xai discovery {field} is invalid: {raw_url:?}"));
    };
    let host_port = rest.split('/').next().unwrap_or_default();
    let host = host_port
        .split(':')
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if host != "x.ai" && !host.ends_with(".x.ai") && !is_loopback_host(&host) {
        return Err(format!(
            "xai discovery {field} host {host:?} is not on x.ai"
        ));
    }
    Ok(raw_url.to_string())
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// Poll interval from the device response, floored at the RFC default.
fn poll_interval(interval_seconds: Option<u64>, minimum: Duration) -> Duration {
    match interval_seconds {
        Some(seconds) if seconds > 0 => Duration::from_secs(seconds).max(minimum),
        _ => minimum,
    }
}

/// RFC 8628 §3.5 `slow_down`: grow the interval by one step.
fn slow_down(current: Duration, step: Duration) -> Duration {
    current.saturating_add(step)
}

/// Absolute deadline for the polling loop, measured from the flow's start:
/// the code's own lifetime, capped by the configured maximum.
fn code_deadline(expires_in: Option<u64>, cap: Duration) -> Duration {
    match expires_in {
        Some(seconds) if seconds > 0 => Duration::from_secs(seconds).min(cap),
        _ => cap,
    }
}

/// Current unix time in seconds.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn token_bundle(raw: RawTokenResponse, token_endpoint: &str) -> Result<XaiTokens, String> {
    let access_token = raw.access_token.trim().to_string();
    if access_token.is_empty() {
        return Err("xai device token response missing access_token".to_string());
    }
    let (email, subject) = raw
        .id_token
        .as_deref()
        .map(parse_identity)
        .unwrap_or((None, None));
    Ok(XaiTokens {
        access_token,
        refresh_token: non_empty(raw.refresh_token),
        id_token: non_empty(raw.id_token),
        token_type: non_empty(raw.token_type),
        expires_in: raw.expires_in.filter(|seconds| *seconds > 0),
        expires_at_unix: raw
            .expires_in
            .filter(|seconds| *seconds > 0)
            .map(|seconds| now_unix().saturating_add(seconds)),
        email,
        subject,
        token_endpoint: token_endpoint.to_string(),
        api_base_url: XAI_API_BASE_URL.to_string(),
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Read `email` / `sub` out of an id_token without verifying it (the token is
/// received over TLS from the issuer; clud only uses it for display).
pub fn parse_identity(id_token: &str) -> (Option<String>, Option<String>) {
    let Some(payload) = id_token.split('.').nth(1) else {
        return (None, None);
    };
    let Ok(raw) = base64url_decode(payload) else {
        return (None, None);
    };
    let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return (None, None);
    };
    let field = |name: &str| {
        claims
            .get(name)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    (field("email"), field("sub"))
}

fn base64url_decode(input: &str) -> Result<Vec<u8>, String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits: u32 = 0;
    let mut bit_count = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for byte in input.bytes().filter(|byte| *byte != b'=') {
        let value = ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)
            .ok_or_else(|| "invalid base64url character".to_string())? as u32;
        bits = (bits << 6) | value;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push((bits >> bit_count) as u8);
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct DiscoveryDocument {
    #[serde(default)]
    device_authorization_endpoint: String,
    #[serde(default)]
    token_endpoint: String,
}

#[derive(Deserialize)]
struct RawDeviceCode {
    #[serde(default)]
    device_code: String,
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct RawTokenResponse {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Arguments for the hidden test/automation seam. No production path calls it.
#[derive(Debug, Clone, Default)]
pub struct SeamArgs<'a> {
    pub discovery_url: Option<&'a str>,
    pub device_endpoint: Option<&'a str>,
    pub token_endpoint: Option<&'a str>,
    pub interval_ms: Option<u64>,
    pub max_poll_ms: Option<u64>,
}

/// Run a device login and print progress as JSON lines. Secrets are never
/// printed: the device code is echoed because the user needs it to authorize,
/// and success reports only identity metadata.
pub fn run_seam(args: &SeamArgs<'_>) -> i32 {
    let minimum = args
        .interval_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_POLL_INTERVAL);
    let cap = args
        .max_poll_ms
        .map(Duration::from_millis)
        .unwrap_or(MAX_POLL_DURATION);
    let mut client = XaiOAuth::with_limits(minimum, cap);
    if let Some(url) = args.discovery_url {
        client = client.with_discovery_url(url);
    }
    let endpoints = match (args.device_endpoint, args.token_endpoint) {
        (Some(device), Some(token)) => {
            match (
                validate_endpoint(device, "device_authorization_endpoint"),
                validate_endpoint(token, "token_endpoint"),
            ) {
                (Ok(device_authorization), Ok(token)) => XaiEndpoints {
                    device_authorization,
                    token,
                },
                (Err(error), _) | (_, Err(error)) => return seam_failure(&error),
            }
        }
        (None, None) => match client.discover() {
            Ok(endpoints) => endpoints,
            Err(error) => return seam_failure(&error),
        },
        _ => return seam_failure("both --device-endpoint and --token-endpoint are required"),
    };
    let device = match client.request_device_code(&endpoints) {
        Ok(device) => device,
        Err(error) => return seam_failure(&error),
    };
    println!(
        "{}",
        serde_json::json!({
            "event": "device_code",
            "user_code": device.user_code,
            "verification_uri": device.verification_uri,
            "verification_uri_complete": device.verification_uri_complete,
            "expires_in": device.expires_in,
            "interval": device.interval,
        })
    );
    match client.wait_for_authorization(&device) {
        Ok(tokens) => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "authorized",
                    "email": tokens.email,
                    "subject": tokens.subject,
                    "expires_in": tokens.expires_in,
                    "has_refresh_token": tokens.can_refresh(),
                    "api_base_url": tokens.api_base_url,
                })
            );
            0
        }
        Err(error) => seam_failure(&error),
    }
}

fn seam_failure(error: &str) -> i32 {
    eprintln!("xai device login: {error}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_auth::SecretStoreError;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A scripted HTTP server: each request consumes the next response.
    struct FakeServer {
        base: String,
        requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl FakeServer {
        fn requests(&self) -> Vec<(String, String)> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn fake_server(script: Vec<(u16, String)>) -> FakeServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        std::thread::spawn(move || {
            for (status, body) in script {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut stream);
                recorded.lock().unwrap().push(request);
                let reason = if status == 200 { "OK" } else { "Bad Request" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        FakeServer {
            base: format!("http://{address}"),
            requests,
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> (String, String) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 2048];
        let mut header_end = None;
        let mut content_length = 0usize;
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if header_end.is_none() {
                if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = Some(position + 4);
                    let head = String::from_utf8_lossy(&buffer[..position]).to_string();
                    content_length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                }
            }
            if let Some(end) = header_end {
                if buffer.len() >= end + content_length {
                    let head = String::from_utf8_lossy(&buffer[..end - 4]).to_string();
                    let path = head
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let body =
                        String::from_utf8_lossy(&buffer[end..end + content_length]).to_string();
                    return (path, body);
                }
            }
        }
        (String::new(), String::new())
    }

    fn base64url(input: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let bytes = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let value =
                (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
            let take = chunk.len() + 1;
            for index in 0..take {
                let shift = 18 - (index as u32 * 6);
                out.push(ALPHABET[((value >> shift) & 0x3f) as usize] as char);
            }
        }
        out
    }

    fn id_token(email: &str, subject: &str) -> String {
        let header = base64url(br#"{"alg":"none"}"#);
        let claims = base64url(format!(r#"{{"email":"{email}","sub":"{subject}"}}"#).as_bytes());
        format!("{header}.{claims}.signature")
    }

    fn device_code(endpoints: &XaiEndpoints) -> DeviceCode {
        DeviceCode {
            device_code: "device-abc".to_string(),
            user_code: "USER-123".to_string(),
            verification_uri: "https://x.ai/device".to_string(),
            verification_uri_complete: None,
            expires_in: Some(60),
            interval: Some(1),
            token_endpoint: endpoints.token.clone(),
        }
    }

    fn test_client(base: &str) -> XaiOAuth {
        XaiOAuth::with_limits(Duration::from_millis(1), Duration::from_secs(5))
            .with_discovery_url(format!("{base}/discovery"))
    }

    #[test]
    fn endpoints_must_be_https_on_xai_or_loopback() {
        assert_eq!(
            validate_endpoint("https://auth.x.ai/oauth/token", "token_endpoint").unwrap(),
            "https://auth.x.ai/oauth/token"
        );
        assert_eq!(
            validate_endpoint("https://cli-chat-proxy.grok.com/v1/oauth", "token_endpoint")
                .unwrap_err(),
            "xai discovery token_endpoint host \"cli-chat-proxy.grok.com\" is not on x.ai"
        );
        assert_eq!(
            validate_endpoint("http://auth.x.ai/oauth/token", "token_endpoint").unwrap_err(),
            "xai discovery token_endpoint must use https: \"http://auth.x.ai/oauth/token\""
        );
        assert!(validate_endpoint("http://127.0.0.1:9/token", "token_endpoint").is_ok());
        assert_eq!(
            validate_endpoint("  ", "token_endpoint").unwrap_err(),
            "xai discovery token_endpoint is empty"
        );
    }

    #[test]
    fn discovery_resolves_both_endpoints_from_the_document() {
        let server = fake_server(vec![(
            200,
            r#"{"device_authorization_endpoint":"https://auth.x.ai/oauth2/device","token_endpoint":"https://auth.x.ai/oauth2/token"}"#.to_string(),
        )]);
        let endpoints = test_client(&server.base).discover().unwrap();
        assert_eq!(
            endpoints.device_authorization,
            "https://auth.x.ai/oauth2/device"
        );
        assert_eq!(endpoints.token, "https://auth.x.ai/oauth2/token");
        assert_eq!(server.requests()[0].0, "/discovery");
    }

    #[test]
    fn device_code_request_rejects_a_missing_user_code() {
        let server = fake_server(vec![(200, r#"{"device_code":"device-abc"}"#.to_string())]);
        let endpoints = XaiEndpoints {
            device_authorization: format!("{}/device", server.base),
            token: format!("{}/token", server.base),
        };
        assert_eq!(
            test_client(&server.base)
                .request_device_code(&endpoints)
                .unwrap_err(),
            "xai device code: response missing user_code"
        );
        let body = &server.requests()[0].1;
        assert!(body.contains("client_id=b1a00492"));
        assert!(body.contains("scope=openid"));
    }

    #[test]
    fn polling_survives_pending_and_slow_down_then_stores_the_bundle() {
        let identity = id_token("zach@example.com", "sub-42");
        let server = fake_server(vec![
            (
                200,
                r#"{"device_code":"device-abc","user_code":"USER-123","verification_uri":"https://x.ai/device","expires_in":60,"interval":1}"#
                    .to_string(),
            ),
            (400, r#"{"error":"authorization_pending"}"#.to_string()),
            (400, r#"{"error":"slow_down"}"#.to_string()),
            (
                200,
                format!(
                    r#"{{"access_token":"access-1","refresh_token":"refresh-1","id_token":"{identity}","token_type":"Bearer","expires_in":3600}}"#
                ),
            ),
        ]);
        let client = test_client(&server.base);
        let endpoints = XaiEndpoints {
            device_authorization: format!("{}/device", server.base),
            token: format!("{}/token", server.base),
        };
        let device = client.request_device_code(&endpoints).unwrap();
        assert_eq!(device.user_code, "USER-123");
        let tokens = client.wait_for_authorization(&device).unwrap();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(tokens.email.as_deref(), Some("zach@example.com"));
        assert_eq!(tokens.subject.as_deref(), Some("sub-42"));
        assert_eq!(tokens.token_endpoint, endpoints.token);
        assert_eq!(tokens.expires_in, Some(3600));
        assert!(tokens.expires_at_unix.unwrap() > now_unix());
        assert!(tokens.can_refresh());
        // Three polls, each carrying the device-code grant.
        let polls: Vec<_> = server.requests().into_iter().skip(1).collect();
        assert_eq!(polls.len(), 3);
        assert!(polls.iter().all(|(path, body)| path == "/token"
            && body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
            && body.contains("device_code=device-abc")));
    }

    #[test]
    fn polling_refuses_denied_and_expired_codes() {
        let denied = fake_server(vec![(400, r#"{"error":"access_denied"}"#.to_string())]);
        let client = test_client(&denied.base);
        let endpoints = XaiEndpoints {
            device_authorization: format!("{}/device", denied.base),
            token: format!("{}/token", denied.base),
        };
        assert_eq!(
            client
                .wait_for_authorization(&device_code(&endpoints))
                .unwrap_err(),
            "xai device authorization denied"
        );

        let expired = fake_server(vec![(400, r#"{"error":"expired_token"}"#.to_string())]);
        let endpoints = XaiEndpoints {
            device_authorization: format!("{}/device", expired.base),
            token: format!("{}/token", expired.base),
        };
        assert_eq!(
            test_client(&expired.base)
                .wait_for_authorization(&device_code(&endpoints))
                .unwrap_err(),
            "xai device code expired"
        );
    }

    #[test]
    fn polling_stops_at_the_code_lifetime_without_a_hang() {
        let server = fake_server(vec![(
            400,
            r#"{"error":"authorization_pending"}"#.to_string(),
        )]);
        let endpoints = XaiEndpoints {
            device_authorization: format!("{}/device", server.base),
            token: format!("{}/token", server.base),
        };
        let mut device = device_code(&endpoints);
        device.expires_in = Some(1);
        let started = Instant::now();
        assert_eq!(
            test_client(&server.base)
                .wait_for_authorization(&device)
                .unwrap_err(),
            "xai device code expired"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn refresh_exchanges_a_refresh_token_for_a_new_bundle() {
        let server = fake_server(vec![(
            200,
            r#"{"access_token":"access-2","refresh_token":"refresh-2","token_type":"Bearer","expires_in":900}"#
                .to_string(),
        )]);
        let tokens = test_client(&server.base)
            .refresh("refresh-1", &format!("{}/token", server.base))
            .unwrap();
        assert_eq!(tokens.access_token, "access-2");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-2"));
        let body = &server.requests()[0].1;
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=refresh-1"));
        assert_eq!(
            test_client(&server.base).refresh("  ", "").unwrap_err(),
            "xai token refresh: refresh token is required"
        );
    }

    #[test]
    fn refresh_surfaces_the_server_error_description() {
        let server = fake_server(vec![(
            400,
            r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#.to_string(),
        )]);
        assert_eq!(
            test_client(&server.base)
                .refresh("refresh-1", &format!("{}/token", server.base))
                .unwrap_err(),
            "xai token refresh failed with status 400: {\"error\":\"invalid_grant\",\"error_description\":\"refresh token revoked\"}"
        );
    }

    #[test]
    fn identity_parsing_ignores_an_unreadable_token() {
        let (email, subject) = parse_identity(&id_token("a@b.c", "s-1"));
        assert_eq!(email.as_deref(), Some("a@b.c"));
        assert_eq!(subject.as_deref(), Some("s-1"));
        assert_eq!(parse_identity(""), (None, None));
        assert_eq!(parse_identity("not-a-jwt"), (None, None));
        assert_eq!(parse_identity("a.!!!.c"), (None, None));
    }

    #[test]
    fn expiry_math_uses_the_recorded_lifetime() {
        let tokens = XaiTokens {
            access_token: "a".to_string(),
            refresh_token: None,
            id_token: None,
            token_type: None,
            expires_in: Some(100),
            expires_at_unix: Some(1_000),
            email: None,
            subject: None,
            token_endpoint: "https://auth.x.ai/oauth2/token".to_string(),
            api_base_url: XAI_API_BASE_URL.to_string(),
        };
        assert!(tokens.is_expired(950, 60));
        assert!(!tokens.is_expired(900, 60));
        assert!(!tokens.can_refresh());
        let no_lifetime = XaiTokens {
            expires_at_unix: None,
            ..tokens
        };
        assert!(!no_lifetime.is_expired(u64::MAX, 0));
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
        let tokens = XaiTokens {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            id_token: None,
            token_type: Some("Bearer".to_string()),
            expires_in: Some(60),
            expires_at_unix: Some(120),
            email: Some("a@b.c".to_string()),
            subject: None,
            token_endpoint: "https://auth.x.ai/oauth2/token".to_string(),
            api_base_url: XAI_API_BASE_URL.to_string(),
        };
        store_tokens(&store, &tokens).unwrap();
        assert_eq!(load_tokens(&store).unwrap(), Some(tokens));
        store.set("not json").unwrap();
        assert!(load_tokens(&store)
            .unwrap_err()
            .starts_with("xai credential store: unreadable entry"));
    }
}

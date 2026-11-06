//! Clud-managed ChatGPT subscription credentials (issue #629).
//!
//! This is deliberately separate from the Codex CLI's `~/.codex/auth.json`:
//! clud must be able to remove only its own credentials and must never let a
//! platform API key and subscription login implicitly select one another.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::args::CodexAuthSubcommand;
pub(crate) use crate::platform::codex_auth::protect_windows_credential_file;

const AUTH_FILE: &str = "codex-auth.json";
const LOCK_FILE: &str = "codex-auth.lock";
const EXPERIMENTAL_ACK_MESSAGE: &str =
    "subscription authentication is experimental; re-run with --acknowledge-experimental";
const OAUTH_ISSUER: &str = "https://auth.openai.com";
// Compatibility contract researched from openai/codex `codex-rs/login`.
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CALLBACK_PATH: &str = "/auth/callback";
pub const REFRESH_TEMPORARY: &str = "Codex credential refresh is temporarily unavailable";
pub const REFRESH_REJECTED: &str =
    "the Codex refresh token was rejected -- run `clud auth login codex`";
pub const REFRESH_MALFORMED: &str = "Codex credential refresh returned an invalid response";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshFailure {
    Temporary,
    Ambiguous,
    Rejected,
    Malformed,
}

impl RefreshFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Temporary | Self::Ambiguous => REFRESH_TEMPORARY,
            Self::Rejected => REFRESH_REJECTED,
            Self::Malformed => REFRESH_MALFORMED,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscriptionCredentials {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<u64>,
}

impl fmt::Debug for SubscriptionCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionCredentials")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[redacted]"),
            )
            .field("email", &self.email)
            .field("expires_at_unix", &self.expires_at_unix)
            .finish()
    }
}

/// Location of clud-owned credentials. It intentionally does not share the
/// Codex CLI location, so `clud auth logout codex` cannot remove another
/// application's credentials.
pub fn credentials_path_at(home: &Path) -> PathBuf {
    home.join(".clud").join(AUTH_FILE)
}

fn lock_path_at(home: &Path) -> PathBuf {
    home.join(".clud").join(LOCK_FILE)
}

fn acquire_lock(home: &Path) -> Result<File, String> {
    let path = lock_path_at(home);
    fs::create_dir_all(
        path.parent()
            .expect("credential lock path always has a parent"),
    )
    .map_err(|error| format!("could not create clud auth directory: {error}"))?;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("could not open clud credential lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("could not lock clud credentials: {error}"))?;
    Ok(lock)
}

pub fn load_at(home: &Path) -> Result<Option<SubscriptionCredentials>, String> {
    let path = credentials_path_at(home);
    match fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw).map(Some).map_err(|_| {
            "clud subscription credentials are corrupted; run `clud auth logout codex`".to_string()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "could not read clud subscription credentials: {error}"
        )),
    }
}

pub fn remove_at(home: &Path) -> Result<bool, String> {
    let _lock = acquire_lock(home)?;
    match fs::remove_file(credentials_path_at(home)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "could not remove clud subscription credentials: {error}"
        )),
    }
}

/// Atomically write credentials after serializing against other clud
/// processes. On Unix the file is created with mode 0600; on Windows the file
/// gets a protected owner-only DACL which is read back and verified.
pub fn save_at(home: &Path, credentials: &SubscriptionCredentials) -> Result<(), String> {
    let _lock = acquire_lock(home)?;
    save_locked(home, credentials)
}

/// Import only if the selected record is still the one shown to the user.
/// A concurrent login or refresh must never be overwritten by a stale prompt.
pub fn replace_if_unchanged_at(
    home: &Path,
    expected: Option<&SubscriptionCredentials>,
    replacement: &SubscriptionCredentials,
) -> Result<bool, String> {
    let _lock = acquire_lock(home)?;
    if load_at(home)?.as_ref() != expected {
        return Ok(false);
    }
    save_locked(home, replacement)?;
    Ok(true)
}

fn save_locked(home: &Path, credentials: &SubscriptionCredentials) -> Result<(), String> {
    let path = credentials_path_at(home);
    let temp = path.with_extension("tmp");
    let encoded = serde_json::to_vec(credentials)
        .map_err(|error| format!("could not encode clud credentials: {error}"))?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    crate::platform::codex_auth::restrict_credential_open_options(&mut options);
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("could not create clud credential file: {error}"))?;
    protect_windows_credential_file(&temp)?;
    file.write_all(&encoded)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write clud credentials: {error}"))?;
    drop(file);
    fs::rename(&temp, &path)
        .map_err(|error| format!("could not replace clud credentials atomically: {error}"))?;
    protect_windows_credential_file(&path)?;
    Ok(())
}

/// Re-read after acquiring the shared lock, refresh at most once, and replace
/// the record atomically. Its transport is injected so this invariant can be
/// proven without an account or token-shaped fixture in a test log.
pub fn refresh_if_needed_at(
    home: &Path,
    now_unix: u64,
    refresh: impl FnOnce(&str) -> Result<(String, String, Option<u64>), String>,
) -> Result<SubscriptionCredentials, String> {
    let _lock = acquire_lock(home)?;
    let current = load_at(home)?.ok_or_else(|| "no clud ChatGPT subscription login".to_string())?;
    if current
        .expires_at_unix
        .is_none_or(|expiry| expiry > now_unix.saturating_add(60))
    {
        return Ok(current);
    }
    refresh_locked(home, current, refresh)
}

/// Recover a bearer rejected by the model API. A competing clud process may
/// already have rotated it; the lock and re-read avoid replaying its old grant.
pub fn refresh_after_rejection_at(
    home: &Path,
    rejected_access_token: &str,
    refresh: impl FnOnce(&str) -> Result<(String, String, Option<u64>), String>,
) -> Result<SubscriptionCredentials, String> {
    let _lock = acquire_lock(home)?;
    let current = load_at(home)?.ok_or_else(|| "no clud ChatGPT subscription login".to_string())?;
    if current.access_token != rejected_access_token {
        return Ok(current);
    }
    refresh_locked(home, current, refresh)
}

pub fn refresh_after_rejection(
    home: &Path,
    rejected_access_token: &str,
) -> Result<SubscriptionCredentials, String> {
    refresh_after_rejection_at(home, rejected_access_token, refresh_remote)
}

fn refresh_locked(
    home: &Path,
    current: SubscriptionCredentials,
    refresh: impl FnOnce(&str) -> Result<(String, String, Option<u64>), String>,
) -> Result<SubscriptionCredentials, String> {
    let (access_token, refresh_token, expires_at_unix) = refresh(&current.refresh_token)?;
    if access_token.trim().is_empty() || refresh_token.trim().is_empty() {
        return Err(REFRESH_MALFORMED.to_string());
    }
    let replacement = SubscriptionCredentials {
        expires_at_unix: expires_at_unix.or_else(|| {
            safe_identity_claims(&access_token).and_then(|claims| claims.expires_at_unix)
        }),
        access_token,
        refresh_token,
        ..current
    };
    save_locked(home, &replacement)?;
    Ok(replacement)
}

pub fn load_fresh_at(home: &Path) -> Result<SubscriptionCredentials, String> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    refresh_if_needed_at(home, now_unix, refresh_remote)
}

fn refresh_remote(refresh_token: &str) -> Result<(String, String, Option<u64>), String> {
    retry_refresh(refresh_token, refresh_remote_once).map_err(|error| error.message().to_string())
}

fn retry_refresh(
    refresh_token: &str,
    mut refresh: impl FnMut(&str) -> Result<(String, String, Option<u64>), RefreshFailure>,
) -> Result<(String, String, Option<u64>), RefreshFailure> {
    for attempt in 0..3 {
        match refresh(refresh_token) {
            Ok(tokens) => return Ok(tokens),
            Err(RefreshFailure::Temporary) if attempt < 2 => {
                thread::sleep(Duration::from_millis(200 * (attempt + 1)));
            }
            Err(error) => return Err(error),
        }
    }
    Err(RefreshFailure::Temporary)
}

fn refresh_remote_once(
    refresh_token: &str,
) -> Result<(String, String, Option<u64>), RefreshFailure> {
    #[derive(Deserialize)]
    struct Tokens {
        access_token: String,
        refresh_token: String,
        #[serde(default)]
        expires_in: Option<u64>,
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(5))
        .build();
    let response = agent
        .post(&format!("{OAUTH_ISSUER}/oauth/token"))
        .send_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CODEX_CLIENT_ID),
        ])
        .map_err(|error| match error {
            ureq::Error::Transport(transport) => match transport.kind() {
                ureq::ErrorKind::Dns
                | ureq::ErrorKind::ConnectionFailed
                | ureq::ErrorKind::ProxyConnect => RefreshFailure::Temporary,
                _ => RefreshFailure::Ambiguous,
            },
            ureq::Error::Status(status, response) => classify_refresh_status(status, response),
        })?;
    let tokens: Tokens =
        serde_json::from_reader(response.into_reader()).map_err(|_| RefreshFailure::Malformed)?;
    if tokens.access_token.trim().is_empty() || tokens.refresh_token.trim().is_empty() {
        return Err(RefreshFailure::Malformed);
    }
    let expiry = tokens.expires_in.map(|seconds| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_add(seconds)
    });
    Ok((tokens.access_token, tokens.refresh_token, expiry))
}

fn classify_refresh_status(status: u16, response: ureq::Response) -> RefreshFailure {
    if status == 408 || status >= 500 {
        // The token endpoint may have rotated the one-time grant before its
        // response was lost. Retrying it could trigger reuse invalidation.
        return RefreshFailure::Ambiguous;
    }
    if status == 429 {
        return RefreshFailure::Temporary;
    }
    // The OAuth endpoint may answer 400 invalid_grant or a provider-specific
    // token-reuse code. Only bounded enum-like fields are inspected; response
    // text, token fragments and account identifiers are never retained.
    let mut raw = Vec::new();
    let _ = response.into_reader().take(4096).read_to_end(&mut raw);
    classify_refresh_error_body(status, &raw)
}

fn classify_refresh_error_body(status: u16, raw: &[u8]) -> RefreshFailure {
    if status == 408 || status >= 500 {
        return RefreshFailure::Ambiguous;
    }
    if status == 429 {
        return RefreshFailure::Temporary;
    }
    let document: serde_json::Value = match serde_json::from_slice(raw) {
        Ok(document) => document,
        Err(_) => return RefreshFailure::Malformed,
    };
    let code = document
        .pointer("/error/code")
        .or_else(|| document.pointer("/error"))
        .and_then(serde_json::Value::as_str);
    match code {
        Some("temporarily_unavailable" | "server_error") => RefreshFailure::Temporary,
        Some(
            "invalid_grant"
            | "refresh_token_reused"
            | "refresh_token_invalidated"
            | "refresh_token_expired"
            | "invalid_token",
        ) => RefreshFailure::Rejected,
        _ => RefreshFailure::Malformed,
    }
}

pub fn run(subcommand: &CodexAuthSubcommand, interrupted: &AtomicBool) -> i32 {
    let Some(home) = crate::home::user_home() else {
        crate::loop_console::diagnostic!("codex-auth: home directory is unavailable");
        return 2;
    };
    match subcommand {
        CodexAuthSubcommand::Login {
            acknowledge_experimental,
            ..
        } if !acknowledge_experimental => {
            crate::loop_console::diagnostic!("{EXPERIMENTAL_ACK_MESSAGE}");
            2
        }
        CodexAuthSubcommand::Login { no_browser, .. } => login(&home, !no_browser, interrupted),
        CodexAuthSubcommand::Status { json } => match load_at(&home) {
            Ok(Some(credentials)) if *json => {
                let expired = credentials_expired(&credentials, now_unix());
                println!(
                    "{}",
                    serde_json::json!({
                        "source": "clud_chatgpt_subscription",
                        "logged_in": !expired,
                        "account_id": credentials.account_id,
                        "email": credentials.email,
                        "expires_at_unix": credentials.expires_at_unix,
                        "refresh_required": expired,
                    })
                );
                if expired {
                    1
                } else {
                    0
                }
            }
            Ok(Some(credentials)) => {
                let expired = credentials_expired(&credentials, now_unix());
                println!("source: clud ChatGPT subscription");
                if let Some(email) = credentials.email {
                    println!("account: {email}");
                }
                if expired {
                    println!("status: refresh or login required");
                    1
                } else {
                    println!("status: logged in");
                    0
                }
            }
            Ok(None) if *json => {
                println!(
                    "{}",
                    serde_json::json!({"source": "none", "logged_in": false})
                );
                1
            }
            Ok(None) => {
                println!("source: none\nstatus: login required");
                1
            }
            Err(error) => {
                crate::loop_console::diagnostic!("codex-auth: {error}");
                2
            }
        },
        CodexAuthSubcommand::Logout { json } => match remove_at(&home) {
            Ok(removed) if *json => {
                println!("{}", serde_json::json!({"removed": removed}));
                0
            }
            Ok(true) => {
                println!("clud subscription credentials removed");
                0
            }
            Ok(false) => {
                println!("no clud subscription credentials found");
                0
            }
            Err(error) => {
                crate::loop_console::diagnostic!("codex-auth: {error}");
                2
            }
        },
    }
}

fn login(home: &Path, open_browser: bool, interrupted: &AtomicBool) -> i32 {
    let listener = match bind_callback_listener() {
        Ok(listener) => listener,
        Err(error) => {
            crate::loop_console::diagnostic!("codex-auth: could not bind OAuth callback: {error}");
            return 2;
        }
    };
    let port = listener
        .local_addr()
        .expect("bound listener has address")
        .port();
    let (verifier, challenge) = pkce_pair().expect("OS random source available");
    let state = random_urlsafe(32).expect("OS random source available");
    let nonce = random_urlsafe(32).expect("OS random source available");
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    let authorize_url = authorization_url(&redirect_uri, &challenge, &state, &nonce);
    println!("Open this URL to sign in with ChatGPT:\n{authorize_url}");
    if open_browser {
        let _ = open::that(&authorize_url);
    }
    let code = match wait_for_callback_with(
        &listener,
        &state,
        Instant::now() + Duration::from_secs(10 * 60),
        || interrupted.load(Ordering::SeqCst),
    ) {
        Ok(code) => code,
        Err(error) => {
            crate::loop_console::diagnostic!("codex-auth: {error}");
            return 1;
        }
    };
    match exchange_code(&code, &redirect_uri, &verifier, &nonce)
        .and_then(|credentials| save_at(home, &credentials))
    {
        Ok(()) => {
            println!("clud ChatGPT subscription login complete");
            0
        }
        Err(error) => {
            crate::loop_console::diagnostic!("codex-auth: {error}");
            1
        }
    }
}

fn authorization_url(redirect_uri: &str, challenge: &str, state: &str, nonce: &str) -> String {
    format!(
        "{OAUTH_ISSUER}/oauth/authorize?response_type=code&client_id={CODEX_CLIENT_ID}&redirect_uri={}&scope={}&code_challenge={challenge}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={state}&nonce={nonce}",
        form_encode(redirect_uri),
        form_encode("openid profile email offline_access api.connectors.read api.connectors.invoke"),
    )
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn credentials_expired(credentials: &SubscriptionCredentials, now: u64) -> bool {
    credentials
        .expires_at_unix
        .is_some_and(|expiry| expiry <= now.saturating_add(60))
}

fn bind_callback_listener() -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", 1455))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 1457)))
        .map_err(|error| format!("ports 1455 and 1457 are unavailable: {error}"))
}

fn wait_for_callback_with(
    listener: &TcpListener,
    expected_state: &str,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("could not configure OAuth callback: {error}"))?;
    while Instant::now() < deadline {
        if cancelled() {
            return Err("OAuth sign-in cancelled".to_string());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut request = [0_u8; 8192];
                let read = stream
                    .read(&mut request)
                    .map_err(|error| format!("could not read OAuth callback: {error}"))?;
                let target = std::str::from_utf8(&request[..read])
                    .ok()
                    .and_then(|raw| raw.lines().next())
                    .and_then(|line| line.split_whitespace().nth(1))
                    .ok_or_else(|| "received malformed OAuth callback".to_string())?;
                let result = callback_code(target, expected_state);
                let response = if result.is_ok() {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nSign-in complete. You may close this tab.".as_slice()
                } else {
                    b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nSign-in could not be completed.".as_slice()
                };
                let _ = stream.write_all(response);
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25))
            }
            Err(error) => return Err(format!("could not accept OAuth callback: {error}")),
        }
    }
    Err("OAuth callback timed out; re-run `clud auth login codex`".to_string())
}

fn callback_code(target: &str, expected_state: &str) -> Result<String, String> {
    if target.split('?').next() != Some(CALLBACK_PATH) {
        return Err("OAuth callback used an unexpected path".to_string());
    }
    let query = target
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default();
    let value = |key: &str| {
        query.split('&').find_map(|part| {
            part.split_once('=')
                .filter(|(name, _)| *name == key)
                .map(|(_, value)| form_decode(value))
        })
    };
    if value("state").as_deref() != Some(expected_state) {
        return Err("OAuth callback state did not match; sign-in was rejected".to_string());
    }
    if value("error").is_some() {
        return Err("ChatGPT denied the sign-in request".to_string());
    }
    value("code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| "OAuth callback did not include an authorization code".to_string())
}

fn exchange_code(
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    expected_nonce: &str,
) -> Result<SubscriptionCredentials, String> {
    #[derive(Deserialize)]
    struct Tokens {
        access_token: String,
        refresh_token: String,
        #[serde(default)]
        id_token: Option<String>,
        #[serde(default)]
        expires_in: Option<u64>,
    }
    let response = ureq::post(&format!("{OAUTH_ISSUER}/oauth/token"))
        .send_form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CODEX_CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .map_err(|_| "OAuth token exchange failed; re-run `clud auth login codex`".to_string())?;
    let tokens: Tokens = serde_json::from_reader(response.into_reader())
        .map_err(|_| "OAuth token exchange returned an invalid response".to_string())?;
    let claims = tokens
        .id_token
        .as_deref()
        .and_then(safe_identity_claims)
        .ok_or_else(|| "OAuth token exchange did not return a valid identity token".to_string())?;
    if claims.nonce.as_deref() != Some(expected_nonce) {
        return Err("OAuth identity token nonce did not match; sign-in was rejected".to_string());
    }
    let expires_at_unix = claims.expires_at_unix.or_else(|| {
        tokens
            .expires_in
            .map(|seconds| now_unix().saturating_add(seconds))
    });
    Ok(SubscriptionCredentials {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        account_id: claims.account_id,
        email: claims.email,
        expires_at_unix,
    })
}

#[derive(Deserialize)]
struct IdentityClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "exp")]
    expires_at_unix: Option<u64>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
}

#[derive(Deserialize)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

struct SafeIdentityClaims {
    email: Option<String>,
    account_id: Option<String>,
    expires_at_unix: Option<u64>,
    nonce: Option<String>,
}

/// Decode only display-safe JWT claims. The token itself is never retained or
/// printed, and signature verification remains the issuer's responsibility.
fn safe_identity_claims(token: &str) -> Option<SafeIdentityClaims> {
    let payload = token.split('.').nth(1)?;
    let bytes =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload).ok()?;
    let claims: IdentityClaims = serde_json::from_slice(&bytes).ok()?;
    Some(SafeIdentityClaims {
        email: claims.email,
        account_id: claims.auth.and_then(|auth| auth.chatgpt_account_id),
        expires_at_unix: claims.expires_at_unix,
        nonce: claims.nonce,
    })
}

fn pkce_pair() -> Result<(String, String), getrandom::Error> {
    let verifier = random_urlsafe(64)?;
    let challenge = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        Sha256::digest(verifier.as_bytes()),
    );
    Ok((verifier, challenge))
}

fn random_urlsafe(bytes: usize) -> Result<String, getrandom::Error> {
    let mut raw = vec![0_u8; bytes];
    getrandom::fill(&mut raw)?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        raw,
    ))
}

fn form_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}
fn form_decode(value: &str) -> String {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(hex);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    #[test]
    fn clud_auth_path_is_distinct_from_codex_cli_auth() {
        let home = Path::new("home");
        assert_eq!(
            credentials_path_at(home),
            Path::new("home/.clud/codex-auth.json")
        );
    }

    #[test]
    fn credential_store_round_trip_is_clud_owned() {
        let home = tempfile::TempDir::new().unwrap();
        let credentials = SubscriptionCredentials {
            access_token: "access-secret".to_string(),
            refresh_token: "refresh-secret".to_string(),
            account_id: Some("acct-1".to_string()),
            email: Some("person@example.test".to_string()),
            expires_at_unix: Some(123),
        };
        save_at(home.path(), &credentials).unwrap();
        assert_eq!(load_at(home.path()).unwrap(), Some(credentials));
        assert!(!home.path().join(".codex/auth.json").exists());
        assert!(remove_at(home.path()).unwrap());
        assert_eq!(load_at(home.path()).unwrap(), None);
    }

    #[test]
    fn credential_debug_never_exposes_tokens_or_account_id() {
        let credentials = SubscriptionCredentials {
            access_token: "access-secret".to_string(),
            refresh_token: "refresh-secret".to_string(),
            account_id: Some("acct-secret".to_string()),
            email: Some("person@example.test".to_string()),
            expires_at_unix: Some(123),
        };
        let debug = format!("{credentials:?}");
        for secret in ["access-secret", "refresh-secret", "acct-secret"] {
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn pkce_and_callback_validation_follow_codex_compatibility_contract() {
        let (verifier, challenge) = pkce_pair().unwrap();
        assert!((43..=128).contains(&verifier.len()));
        assert_eq!(challenge.len(), 43);
        assert_eq!(
            callback_code("/auth/callback?code=abc&state=right", "right").unwrap(),
            "abc"
        );
        assert!(callback_code("/auth/callback?code=abc&state=wrong", "right").is_err());
        assert!(callback_code("/wrong?code=abc&state=right", "right").is_err());
        let authorize = authorization_url(
            "http://localhost:1455/auth/callback",
            &challenge,
            "state",
            "nonce",
        );
        assert!(authorize.contains("code_challenge_method=S256"));
        assert!(authorize.contains("state=state&nonce=nonce"));
    }

    #[test]
    fn status_requires_refresh_within_expiry_skew() {
        let credentials = SubscriptionCredentials {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            account_id: None,
            email: None,
            expires_at_unix: Some(160),
        };
        assert!(credentials_expired(&credentials, 100));
        assert!(!credentials_expired(&credentials, 99));
    }

    #[test]
    fn callback_wait_has_stable_timeout_and_cancellation_outcomes() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let timeout = wait_for_callback_with(&listener, "state", Instant::now(), || false);
        assert_eq!(
            timeout.unwrap_err(),
            "OAuth callback timed out; re-run `clud auth login codex`"
        );
        let cancelled = wait_for_callback_with(
            &listener,
            "state",
            Instant::now() + Duration::from_secs(1),
            || true,
        );
        assert_eq!(cancelled.unwrap_err(), "OAuth sign-in cancelled");
    }

    #[test]
    fn refresh_replaces_only_an_expired_record() {
        let home = tempfile::TempDir::new().unwrap();
        save_at(
            home.path(),
            &SubscriptionCredentials {
                access_token: "old-access".to_string(),
                refresh_token: "old-refresh".to_string(),
                account_id: Some("acct".to_string()),
                email: None,
                expires_at_unix: Some(100),
            },
        )
        .unwrap();
        let refreshed = refresh_if_needed_at(home.path(), 100, |token| {
            assert_eq!(token, "old-refresh");
            Ok((
                "new-access".to_string(),
                "new-refresh".to_string(),
                Some(200),
            ))
        })
        .unwrap();
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(
            load_at(home.path()).unwrap().unwrap().refresh_token,
            "new-refresh"
        );
    }

    #[test]
    fn rejected_access_token_forces_refresh_before_recorded_expiry() {
        let home = tempfile::TempDir::new().unwrap();
        save_at(
            home.path(),
            &SubscriptionCredentials {
                access_token: "rejected-access".to_string(),
                refresh_token: "old-refresh".to_string(),
                account_id: Some("acct".to_string()),
                email: None,
                expires_at_unix: Some(u64::MAX),
            },
        )
        .unwrap();
        let recovered = refresh_after_rejection_at(home.path(), "rejected-access", |token| {
            assert_eq!(token, "old-refresh");
            Ok((
                "new-access".to_string(),
                "new-refresh".to_string(),
                Some(900),
            ))
        })
        .unwrap();
        assert_eq!(recovered.access_token, "new-access");
        assert_eq!(
            load_at(home.path()).unwrap().unwrap().refresh_token,
            "new-refresh"
        );
    }

    #[test]
    fn rejected_access_token_reuses_a_competing_refresh() {
        let home = tempfile::TempDir::new().unwrap();
        save_at(
            home.path(),
            &SubscriptionCredentials {
                access_token: "already-refreshed".to_string(),
                refresh_token: "new-refresh".to_string(),
                account_id: Some("acct".to_string()),
                email: None,
                expires_at_unix: Some(u64::MAX),
            },
        )
        .unwrap();
        let recovered = refresh_after_rejection_at(home.path(), "rejected-access", |_| {
            panic!("a newer stored token must not be refreshed again")
        })
        .unwrap();
        assert_eq!(recovered.access_token, "already-refreshed");
    }

    #[test]
    fn native_import_does_not_replace_a_record_changed_during_the_prompt() {
        let home = tempfile::TempDir::new().unwrap();
        let initial = SubscriptionCredentials {
            access_token: "old-access".to_string(),
            refresh_token: "old-refresh".to_string(),
            account_id: Some("account-a".to_string()),
            email: None,
            expires_at_unix: Some(100),
        };
        let mut concurrent = initial.clone();
        concurrent.access_token = "new-access".to_string();
        concurrent.refresh_token = "new-refresh".to_string();
        let mut imported = initial.clone();
        imported.account_id = Some("account-b".to_string());

        save_at(home.path(), &initial).unwrap();
        save_at(home.path(), &concurrent).unwrap();
        assert!(!replace_if_unchanged_at(home.path(), Some(&initial), &imported).unwrap());
        assert_eq!(load_at(home.path()).unwrap(), Some(concurrent.clone()));
        assert!(replace_if_unchanged_at(home.path(), Some(&concurrent), &imported).unwrap());
        assert_eq!(load_at(home.path()).unwrap(), Some(imported));
    }

    #[test]
    fn oauth_refresh_status_classifies_grants_without_leaking_response_text() {
        for (status, body, expected) in [
            (
                400,
                r#"{"error":"invalid_grant"}"#,
                RefreshFailure::Rejected,
            ),
            (
                401,
                r#"{"error":{"code":"refresh_token_reused","message":"Bearer secret"}}"#,
                RefreshFailure::Rejected,
            ),
            (429, r#"{}"#, RefreshFailure::Temporary),
            (503, r#"{}"#, RefreshFailure::Ambiguous),
            (
                400,
                r#"{"error":"invalid_request"}"#,
                RefreshFailure::Malformed,
            ),
            (401, "<html>login</html>", RefreshFailure::Malformed),
            (
                400,
                r#"{"error":"temporarily_unavailable"}"#,
                RefreshFailure::Temporary,
            ),
        ] {
            assert_eq!(
                classify_refresh_error_body(status, body.as_bytes()),
                expected
            );
        }
        assert!(!format!("{:?}", RefreshFailure::Rejected).contains("secret"));
    }

    #[test]
    fn temporary_refresh_is_bounded_and_rejected_grants_do_not_retry() {
        let mut attempts = 0;
        let result = retry_refresh("private-refresh", |token| {
            assert_eq!(token, "private-refresh");
            attempts += 1;
            if attempts < 3 {
                Err(RefreshFailure::Temporary)
            } else {
                Ok(("access".into(), "rotated".into(), Some(10)))
            }
        });
        assert!(result.is_ok());
        assert_eq!(attempts, 3);

        let mut rejected_attempts = 0;
        let result = retry_refresh("private-refresh", |_| {
            rejected_attempts += 1;
            Err(RefreshFailure::Rejected)
        });
        assert_eq!(result.unwrap_err(), RefreshFailure::Rejected);
        assert_eq!(rejected_attempts, 1);

        let mut exhausted_attempts = 0;
        let result = retry_refresh("private-refresh", |_| {
            exhausted_attempts += 1;
            Err(RefreshFailure::Temporary)
        });
        assert_eq!(result.unwrap_err(), RefreshFailure::Temporary);
        assert_eq!(exhausted_attempts, 3);
    }

    #[test]
    fn concurrent_refresh_contenders_share_one_atomic_replacement() {
        let home = tempfile::TempDir::new().unwrap();
        save_at(
            home.path(),
            &SubscriptionCredentials {
                access_token: "old-access".to_string(),
                refresh_token: "old-refresh".to_string(),
                account_id: None,
                email: None,
                expires_at_unix: Some(100),
            },
        )
        .unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(6));
        let mut contenders = Vec::new();
        for _ in 0..6 {
            let home = home.path().to_path_buf();
            let refreshes = Arc::clone(&refreshes);
            let barrier = Arc::clone(&barrier);
            contenders.push(thread::spawn(move || {
                barrier.wait();
                refresh_if_needed_at(&home, 100, |_| {
                    refreshes.fetch_add(1, Ordering::SeqCst);
                    Ok((
                        "new-access".to_string(),
                        "new-refresh".to_string(),
                        Some(200),
                    ))
                })
                .unwrap()
            }));
        }
        for contender in contenders {
            assert_eq!(contender.join().unwrap().access_token, "new-access");
        }
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn identity_claims_extract_only_status_fields() {
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            br#"{"email":"person@example.test","exp":123,"nonce":"expected-nonce","https://api.openai.com/auth":{"chatgpt_account_id":"acct-1"}}"#,
        );
        let claims = safe_identity_claims(&format!("x.{payload}.y")).unwrap();
        assert_eq!(claims.email.as_deref(), Some("person@example.test"));
        assert_eq!(claims.account_id.as_deref(), Some("acct-1"));
        assert_eq!(claims.expires_at_unix, Some(123));
        assert_eq!(claims.nonce.as_deref(), Some("expected-nonce"));
    }
}

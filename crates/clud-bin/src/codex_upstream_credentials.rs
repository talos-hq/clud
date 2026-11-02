use super::*;

/// A secret-free explanation of why the bridge cannot be admitted.  This is
/// deliberately narrower than [`UpstreamError`]: it owns the stable launch
/// UX, while request-time resolution retains the detailed transport policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexBridgeCredentialError {
    Missing,
    ExpiredOrRevoked,
    RefreshRejected,
    RefreshTemporary,
    RefreshMalformed,
    Corrupt,
    Unreadable,
}

impl std::fmt::Display for CodexBridgeCredentialError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str(
                "cannot launch Claude with the Codex provider: no Codex bridge credential is configured.\n\
\n\
Choose one:\n\
  ChatGPT subscription: clud auth login codex --acknowledge-experimental\n\
  OpenAI API (metered, pay-per-token): set OPENAI_API_KEY with CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER=codex\n\
\n\
A native Codex CLI login can be imported after confirmation in an interactive bridge launch.\n\
To use that login now: clud --codex --harness default",
            ),
            Self::ExpiredOrRevoked => formatter.write_str(
                "cannot launch Claude with the Codex provider: the clud-owned Codex bridge credential has expired or was revoked.\n\
\n\
Recover with:\n\
  clud auth login codex --acknowledge-experimental",
            ),
            Self::RefreshRejected => formatter.write_str(
                "cannot launch Claude with the Codex provider: the clud-owned Codex refresh token was rejected.\n\
\n\
Recover with:\n\
  clud auth login codex --acknowledge-experimental",
            ),
            Self::RefreshTemporary => formatter.write_str(
                "cannot launch Claude with the Codex provider: credential refresh is temporarily unavailable. Retry after the connection or provider recovers",
            ),
            Self::RefreshMalformed => formatter.write_str(
                "cannot launch Claude with the Codex provider: credential refresh returned an invalid response. Retry or update clud",
            ),
            Self::Corrupt => formatter.write_str(
                "cannot launch Claude with the Codex provider: the clud-owned Codex bridge credential is corrupt.\n\
\n\
Recover with:\n\
  clud auth logout codex\n\
  clud auth login codex --acknowledge-experimental",
            ),
            Self::Unreadable => formatter.write_str(
                "cannot launch Claude with the Codex provider: the clud-owned Codex bridge credential is unreadable.\n\
\n\
Restore access to the clud credential store, then run:\n\
  clud auth login codex --acknowledge-experimental",
            ),
        }
    }
}

/// Platform API key credentials.
#[derive(Clone)]
pub struct ApiKeyCredentials {
    target: UpstreamTarget,
}

impl ApiKeyCredentials {
    pub fn new(api_key: impl AsRef<str>, base_url: Option<String>) -> Result<Self, UpstreamError> {
        let target = resolve_api_key_target(Some(api_key.as_ref().to_string()), base_url)?;
        Ok(Self { target })
    }

    /// Read the platform key from the environment.
    ///
    /// There is deliberately no fallback chain here: #629 requires that
    /// subscription auth and `OPENAI_API_KEY` never silently substitute for
    /// one another, so a missing key is an error rather than a downgrade.
    pub fn from_env() -> Result<Self, UpstreamError> {
        let target = resolve_api_key_target(
            std::env::var("OPENAI_API_KEY").ok(),
            std::env::var("OPENAI_BASE_URL").ok(),
        )?;
        Ok(Self { target })
    }
}

impl std::fmt::Debug for ApiKeyCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ApiKeyCredentials").finish()
    }
}

impl CredentialSource for ApiKeyCredentials {
    fn resolve(&self) -> Result<UpstreamTarget, UpstreamError> {
        Ok(self.target.clone())
    }
}

/// The metered `OPENAI_API_KEY` fallback, refused unless the operator set
/// both metered overrides for Codex. A missing subscription never silently
/// becomes pay-per-token billing.
pub(super) const METERED_API_KEY_REFUSED: &str =
    "no clud ChatGPT subscription login; OPENAI_API_KEY is metered and needs \
     CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER=codex";

fn metered_api_key_from_env() -> Result<ApiKeyCredentials, UpstreamError> {
    metered_api_key_gate(
        crate::metered_policy::process_allows_codex_api_key(),
        ApiKeyCredentials::from_env,
    )
}

pub(super) fn metered_api_key_gate(
    allowed: bool,
    api_key: impl FnOnce() -> Result<ApiKeyCredentials, UpstreamError>,
) -> Result<ApiKeyCredentials, UpstreamError> {
    if allowed {
        api_key()
    } else {
        Err(UpstreamError::Credentials(METERED_API_KEY_REFUSED))
    }
}

/// Pure resolution so the policy is testable without touching process env.
/// A per-client identifier. Opaque upstream; it only has to be stable and
/// unique enough to correlate one session.
pub(super) fn new_session_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return "00000000-0000-4000-8000-000000000000".to_string();
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-4{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

pub(super) fn resolve_api_key_target(
    api_key: Option<String>,
    base_url: Option<String>,
) -> Result<UpstreamTarget, UpstreamError> {
    let key = api_key
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .ok_or(UpstreamError::Credentials("OPENAI_API_KEY is not set"))?;
    let base = base_url
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    Ok(UpstreamTarget::new(base, format!("Bearer {key}")))
}

/// Credentials read from the Codex CLI's own `auth.json`.
///
/// This reads an existing login; it does **not** implement the OAuth/PKCE
/// flow, which stays #629's scope. It exists so the bridge can be validated
/// against a real subscription without inventing a second login path.
#[derive(Clone)]
pub struct CodexCliCredentials {
    target: UpstreamTarget,
    record: SubscriptionCredentials,
}

/// ChatGPT subscription credentials created exclusively by `clud codex-auth`.
/// The presence of this record is an explicit persisted source selection: it
/// never falls back to `OPENAI_API_KEY`, and logout removes only this source.
#[derive(Clone)]
pub struct CludSubscriptionCredentials {
    target: UpstreamTarget,
    home: Option<std::path::PathBuf>,
    email: Option<String>,
}

impl CludSubscriptionCredentials {
    pub fn from_home() -> Result<Self, UpstreamError> {
        let home =
            crate::home::user_home().ok_or(UpstreamError::Credentials("no home directory"))?;
        let credentials = match codex_auth::load_fresh_at(&home) {
            Ok(credentials) => credentials,
            Err(error) if error == "no clud ChatGPT subscription login" => {
                return Err(UpstreamError::Credentials(
                    "no clud ChatGPT subscription login",
                ));
            }
            Err(error) => return Err(subscription_upstream_error(&error)),
        };
        let mut source = Self::from_record(credentials)?;
        source.home = Some(home);
        Ok(source)
    }

    pub fn from_record(credentials: SubscriptionCredentials) -> Result<Self, UpstreamError> {
        if credentials.access_token.trim().is_empty() {
            return Err(UpstreamError::Credentials(
                "clud subscription credentials have no access token",
            ));
        }
        if credentials
            .expires_at_unix
            .is_some_and(|expiry| token_expiry_reached(expiry, SystemTime::now()))
        {
            return Err(UpstreamError::Credentials(CLUD_CREDENTIALS_EXPIRED));
        }
        let email = credentials.email.clone();
        Ok(Self {
            target: UpstreamTarget::new(
                CODEX_BACKEND_BASE_URL,
                format!("Bearer {}", credentials.access_token),
            )
            .with_account_id(credentials.account_id)
            .with_header("originator", CODEX_ORIGINATOR),
            home: None,
            email,
        })
    }
}

fn subscription_upstream_error(error: &str) -> UpstreamError {
    let message = match error {
        codex_auth::REFRESH_TEMPORARY => codex_auth::REFRESH_TEMPORARY,
        codex_auth::REFRESH_REJECTED => codex_auth::REFRESH_REJECTED,
        codex_auth::REFRESH_MALFORMED => codex_auth::REFRESH_MALFORMED,
        _ => "the Codex login has expired -- run `clud auth login codex`",
    };
    UpstreamError::Credentials(message)
}

fn classify_subscription_load_error(error: &str) -> CodexBridgeCredentialError {
    if error == "no clud ChatGPT subscription login" {
        CodexBridgeCredentialError::Missing
    } else if error == codex_auth::REFRESH_TEMPORARY {
        CodexBridgeCredentialError::RefreshTemporary
    } else if error == codex_auth::REFRESH_MALFORMED {
        CodexBridgeCredentialError::RefreshMalformed
    } else if error == codex_auth::REFRESH_REJECTED {
        CodexBridgeCredentialError::RefreshRejected
    } else if error.starts_with("clud subscription credentials are corrupted")
        || error == "clud subscription credentials have no access token"
    {
        CodexBridgeCredentialError::Corrupt
    } else if error.starts_with("could not read clud subscription credentials")
        || error.starts_with("could not create clud auth directory")
        || error.starts_with("could not open clud credential lock")
        || error.starts_with("could not lock clud credentials")
    {
        CodexBridgeCredentialError::Unreadable
    } else {
        CodexBridgeCredentialError::ExpiredOrRevoked
    }
}

fn token_expiry_reached(expiry: u64, now: SystemTime) -> bool {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .is_ok_and(|elapsed| elapsed + TOKEN_EXPIRY_SKEW >= Duration::from_secs(expiry))
}

impl std::fmt::Debug for CludSubscriptionCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CludSubscriptionCredentials")
            .finish()
    }
}

impl CredentialSource for CludSubscriptionCredentials {
    fn resolve(&self) -> Result<UpstreamTarget, UpstreamError> {
        Ok(self.target.clone())
    }

    fn recover_after_unauthorized(
        &self,
        rejected: &UpstreamTarget,
    ) -> Result<Option<UpstreamTarget>, UpstreamError> {
        let Some(home) = self.home.as_deref() else {
            return Ok(None);
        };
        let Some(access_token) = rejected.authorization.strip_prefix("Bearer ") else {
            return Ok(None);
        };
        let replacement = codex_auth::refresh_after_rejection(home, access_token)
            .map_err(|error| subscription_upstream_error(&error))?;
        if replacement.account_id != self.target.account_id || replacement.email != self.email {
            return Err(UpstreamError::Credentials(
                "Codex account changed during credential recovery",
            ));
        }
        Self::from_record(replacement).map(|source| Some(source.target))
    }
}

impl CodexCliCredentials {
    /// Load from `$CODEX_HOME/auth.json`, defaulting to `~/.codex`.
    pub fn from_codex_home() -> Result<Self, UpstreamError> {
        let home = std::env::var_os("CODEX_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| crate::home::user_home().map(|home| home.join(".codex")))
            .ok_or(UpstreamError::Credentials("no Codex home directory"))?;
        let raw = std::fs::read(home.join("auth.json"))
            .map_err(|_| UpstreamError::Credentials("Codex auth.json is not readable"))?;
        Self::from_auth_json(&raw)
    }

    /// Parse an `auth.json` document. Split out so the shape is testable
    /// without touching a real credential file.
    pub fn from_auth_json(raw: &[u8]) -> Result<Self, UpstreamError> {
        let document: serde_json::Value = serde_json::from_slice(raw)
            .map_err(|_| UpstreamError::Credentials("Codex auth.json is not valid JSON"))?;
        let access_token = document
            .pointer("/tokens/access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .ok_or(UpstreamError::Credentials(
                "Codex auth.json has no access token",
            ))?;
        let refresh_token = document
            .pointer("/tokens/refresh_token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .ok_or(UpstreamError::Credentials(
                "Codex auth.json has no refresh token",
            ))?;
        // Full refresh is #629's scope. This is only the guardrail: a login
        // that has already expired must fail with an instruction the user can
        // act on, rather than an opaque upstream error a retry cannot fix.
        if token_is_expired(access_token, SystemTime::now()) {
            return Err(UpstreamError::Credentials(CREDENTIALS_EXPIRED));
        }
        let account_id = document
            .pointer("/tokens/account_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string);
        let access_claims = token_claims(access_token);
        // Codex stores the account identity in `id_token`; access tokens also
        // commonly carry it, so retain that older shape as a fallback.
        let id_claims = document
            .pointer("/tokens/id_token")
            .and_then(serde_json::Value::as_str)
            .and_then(token_claims);
        let email = id_claims
            .as_ref()
            .or(access_claims.as_ref())
            .and_then(|claims| claims.get("email"))
            .and_then(serde_json::Value::as_str)
            .filter(|email| !email.trim().is_empty())
            .map(str::to_string);
        let expires_at_unix = access_claims
            .as_ref()
            .and_then(|claims| claims.get("exp"))
            .and_then(serde_json::Value::as_u64);
        let record = SubscriptionCredentials {
            access_token: access_token.to_string(),
            refresh_token: refresh_token.to_string(),
            account_id: account_id.clone(),
            email,
            expires_at_unix,
        };
        Ok(Self {
            target: UpstreamTarget::new(CODEX_BACKEND_BASE_URL, format!("Bearer {access_token}"))
                .with_account_id(account_id),
            record,
        })
    }

    /// Copyable representation for clud's own credential store. The caller
    /// must obtain an explicit user choice before persisting this record.
    pub fn subscription_record(&self) -> SubscriptionCredentials {
        self.record.clone()
    }
}

/// Whether a bearer is a JWT whose `exp` claim has passed.
///
/// The signature is deliberately not verified — that is the issuer's job, and
/// clud has no key. This only reads a claim to avoid *starting* a turn on a
/// token that is already dead. A bearer that is not a JWT, or carries no `exp`,
/// is treated as live: opaque tokens are legitimate and must keep working.
fn token_is_expired(token: &str, now: SystemTime) -> bool {
    let Some(expiry) = token_expiry(token) else {
        return false;
    };
    let Ok(elapsed) = now.duration_since(SystemTime::UNIX_EPOCH) else {
        return false;
    };
    elapsed + TOKEN_EXPIRY_SKEW >= Duration::from_secs(expiry)
}

/// The `exp` claim of a JWT, if the bearer is one.
fn token_expiry(token: &str) -> Option<u64> {
    token_claims(token)?
        .get("exp")
        .and_then(serde_json::Value::as_u64)
}

fn token_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

/// Minimal unpadded base64url decoder. A dependency for one claim read would be
/// a poor trade, and the alphabet is fixed.
pub(super) fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut buffer = 0_u32;
    let mut bits = 0_u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = ALPHABET.iter().position(|candidate| *candidate == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

impl std::fmt::Debug for CodexCliCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CodexCliCredentials").finish()
    }
}

impl CredentialSource for CodexCliCredentials {
    fn resolve(&self) -> Result<UpstreamTarget, UpstreamError> {
        Ok(self.target.clone())
    }
}

/// The credential source the bridge uses, resolved once per launch.
#[derive(Debug, Clone)]
pub enum ResolvedCredentials {
    ApiKey(ApiKeyCredentials),
    Subscription(CludSubscriptionCredentials),
}

impl ResolvedCredentials {
    /// Probe whether the direct Codex-via-Claude route can start. This never
    /// returns credentials, so it cannot pin a token for the harness lifetime;
    /// each request still calls `resolve_default` through `build_pipeline`.
    pub fn preflight_default() -> Result<(), CodexBridgeCredentialError> {
        let Some(home) = crate::home::user_home() else {
            return Err(CodexBridgeCredentialError::Unreadable);
        };
        Self::preflight_with(codex_auth::load_fresh_at(&home), metered_api_key_from_env)
    }

    fn preflight_with(
        subscription: Result<SubscriptionCredentials, String>,
        api_key: impl FnOnce() -> Result<ApiKeyCredentials, UpstreamError>,
    ) -> Result<(), CodexBridgeCredentialError> {
        match subscription {
            Ok(credentials) => CludSubscriptionCredentials::from_record(credentials)
                .map(|_| ())
                .map_err(|error| match error {
                    UpstreamError::Credentials(message) => {
                        classify_subscription_load_error(message)
                    }
                    _ => CodexBridgeCredentialError::Corrupt,
                }),
            Err(error)
                if classify_subscription_load_error(&error)
                    == CodexBridgeCredentialError::Missing =>
            {
                api_key()
                    .map(|_| ())
                    .map_err(|_| CodexBridgeCredentialError::Missing)
            }
            Err(error) => Err(classify_subscription_load_error(&error)),
        }
    }

    /// A clud-managed subscription record is an explicit, persisted selection
    /// made by `clud auth login codex`; while it exists no API-key fallback is
    /// attempted. Without it, only the platform API-key path is considered.
    pub fn resolve_default() -> Result<Self, UpstreamError> {
        Self::resolve_with(
            CludSubscriptionCredentials::from_home(),
            metered_api_key_from_env,
        )
    }

    pub(super) fn resolve_with(
        subscription: Result<CludSubscriptionCredentials, UpstreamError>,
        api_key: impl FnOnce() -> Result<ApiKeyCredentials, UpstreamError>,
    ) -> Result<Self, UpstreamError> {
        match subscription {
            Ok(credentials) => Ok(Self::Subscription(credentials)),
            Err(UpstreamError::Credentials("no clud ChatGPT subscription login")) => {
                api_key().map(Self::ApiKey)
            }
            Err(error) => Err(error),
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Self::ApiKey(_) => "OPENAI_API_KEY",
            Self::Subscription(_) => "clud ChatGPT subscription",
        }
    }
}

impl CredentialSource for ResolvedCredentials {
    fn resolve(&self) -> Result<UpstreamTarget, UpstreamError> {
        match self {
            Self::ApiKey(credentials) => credentials.resolve(),
            Self::Subscription(credentials) => credentials.resolve(),
        }
    }

    fn recover_after_unauthorized(
        &self,
        rejected: &UpstreamTarget,
    ) -> Result<Option<UpstreamTarget>, UpstreamError> {
        match self {
            Self::ApiKey(_) => Ok(None),
            Self::Subscription(credentials) => credentials.recover_after_unauthorized(rejected),
        }
    }
}

#[cfg(test)]
mod preflight_tests {
    use super::*;

    #[test]
    fn missing_subscription_never_falls_back_to_a_metered_api_key_without_consent() {
        let refused = metered_api_key_gate(false, || unreachable!("key must not be read"))
            .expect_err("no consent");
        assert!(matches!(
            refused,
            UpstreamError::Credentials(METERED_API_KEY_REFUSED)
        ));
        assert!(METERED_API_KEY_REFUSED.contains("CLUD_ALLOW_METERED=1"));
        assert!(METERED_API_KEY_REFUSED.contains("CLUD_METERED_PROVIDER=codex"));
        let missing = ResolvedCredentials::preflight_with(
            Err("no clud ChatGPT subscription login".to_string()),
            || metered_api_key_gate(false, || unreachable!("key must not be read")),
        );
        assert_eq!(missing, Err(CodexBridgeCredentialError::Missing));
        let allowed = metered_api_key_gate(true, || ApiKeyCredentials::new("sk-test", None));
        assert!(allowed.is_ok());
    }

    #[test]
    fn bridge_preflight_renderer_is_actionable_and_secret_free() {
        let missing = CodexBridgeCredentialError::Missing.to_string();
        assert!(missing.contains("clud auth login codex --acknowledge-experimental"));
        assert!(missing.contains("set OPENAI_API_KEY"));
        assert!(missing.contains("CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER=codex"));
        assert!(missing.contains("clud --codex --harness default"));
        assert!(missing.contains("can be imported after confirmation"));
        assert!(!missing.contains("Bearer "));

        let expired = CodexBridgeCredentialError::ExpiredOrRevoked.to_string();
        assert!(expired.contains("expired or was revoked"));
        assert!(expired.contains("clud auth login codex --acknowledge-experimental"));

        let corrupt = CodexBridgeCredentialError::Corrupt.to_string();
        assert!(corrupt.contains("clud auth logout codex"));
        assert!(corrupt.contains("clud auth login codex --acknowledge-experimental"));

        let unreadable = CodexBridgeCredentialError::Unreadable.to_string();
        assert!(unreadable.contains("unreadable"));
        assert!(!unreadable.contains(".clud/codex-auth.json"));
    }

    #[test]
    fn preflight_classifies_clud_owned_state_without_echoing_details() {
        assert_eq!(
            classify_subscription_load_error("no clud ChatGPT subscription login"),
            CodexBridgeCredentialError::Missing
        );
        assert_eq!(
            classify_subscription_load_error(
                "clud subscription credentials are corrupted; run `clud auth logout codex`"
            ),
            CodexBridgeCredentialError::Corrupt
        );
        assert_eq!(
            classify_subscription_load_error(
                "could not read clud subscription credentials: permission denied"
            ),
            CodexBridgeCredentialError::Unreadable
        );
        assert_eq!(
            classify_subscription_load_error(
                "the Codex login has expired -- run `clud auth login codex`"
            ),
            CodexBridgeCredentialError::ExpiredOrRevoked
        );
    }

    #[test]
    fn preflight_admits_subscription_or_api_key_without_retaining_either() {
        let subscription = SubscriptionCredentials {
            access_token: "test-subscription-access".to_string(),
            refresh_token: "test-subscription-refresh".to_string(),
            account_id: None,
            email: None,
            expires_at_unix: None,
        };
        assert!(ResolvedCredentials::preflight_with(Ok(subscription), || unreachable!()).is_ok());
        assert!(ResolvedCredentials::preflight_with(
            Err("no clud ChatGPT subscription login".to_string()),
            || ApiKeyCredentials::new("test-api-key", None),
        )
        .is_ok());
    }

    #[test]
    fn preflight_reports_expired_corrupt_and_unreadable_owned_records() {
        for (raw, expected) in [
            (
                "the Codex login has expired -- run `clud auth login codex`",
                CodexBridgeCredentialError::ExpiredOrRevoked,
            ),
            (
                "clud subscription credentials are corrupted; run `clud auth logout codex`",
                CodexBridgeCredentialError::Corrupt,
            ),
            (
                "could not read clud subscription credentials: permission denied",
                CodexBridgeCredentialError::Unreadable,
            ),
        ] {
            assert_eq!(
                ResolvedCredentials::preflight_with(Err(raw.to_string()), || unreachable!()),
                Err(expected)
            );
        }
    }

    #[test]
    fn codex_cli_record_has_every_field_needed_for_a_clud_owned_copy() {
        let credentials = CodexCliCredentials::from_auth_json(
            br#"{"tokens":{"access_token":"e30.eyJlbWFpbCI6InBlcnNvbkBleGFtcGxlLnRlc3QiLCJleHAiOjQxMDI0NDQ4MDB9.sig","refresh_token":"refresh-token","account_id":"acct-1"}}"#,
        )
        .unwrap()
        .subscription_record();
        assert_eq!(credentials.refresh_token, "refresh-token");
        assert_eq!(credentials.account_id.as_deref(), Some("acct-1"));
        assert_eq!(credentials.email.as_deref(), Some("person@example.test"));
        assert_eq!(credentials.expires_at_unix, Some(4_102_444_800));
    }

    #[test]
    fn codex_cli_record_reads_email_from_the_canonical_id_token() {
        let credentials = CodexCliCredentials::from_auth_json(
            br#"{"tokens":{"access_token":"opaque-access","refresh_token":"refresh-token","id_token":"e30.eyJlbWFpbCI6ImlkQGV4YW1wbGUudGVzdCJ9.sig"}}"#,
        )
        .unwrap()
        .subscription_record();
        assert_eq!(credentials.email.as_deref(), Some("id@example.test"));
    }
}

#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// Maximum time to the first upstream byte, while downstream can still
    /// choose a non-200 status and retry safely. `None` preserves the idle
    /// read timeout for callers that deliberately do not impose this budget.
    pub first_frame_timeout: Option<Duration>,
    pub overall_timeout: Duration,
    pub max_response_bytes: usize,
    /// Attempt budget for a recognised transient failure.
    pub max_attempts: u32,
    /// Attempt budget for an unrecognised 5xx.
    pub unknown_max_attempts: u32,
    /// Base of the exponential backoff.
    pub retry_delay: Duration,
    /// Ceiling on one sleep, including a `Retry-After`-derived one.
    pub max_retry_delay: Duration,
    /// Ceiling on total time spent sleeping between attempts.
    pub max_retry_elapsed: Duration,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            first_frame_timeout: None,
            overall_timeout: DEFAULT_OVERALL_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            unknown_max_attempts: DEFAULT_UNKNOWN_MAX_ATTEMPTS,
            retry_delay: DEFAULT_RETRY_DELAY,
            max_retry_delay: DEFAULT_MAX_RETRY_DELAY,
            max_retry_elapsed: DEFAULT_MAX_RETRY_ELAPSED,
        }
    }
}

/// Outcome of one completed stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamOutcome {
    pub attempts: u32,
    pub bytes: usize,
}

pub(super) type RetryObserver =
    std::sync::Arc<dyn Fn(&UpstreamError, u32, u32, Option<Duration>) + Send + Sync>;

//! Foreground API server lifecycle and credential setup.

use std::io::{self, Write};
use std::sync::Arc;

use crate::backend::ModelProvider;
use crate::codex_bridge::{
    BridgeConfig, BridgeHandle, StandaloneClaudeCredential, StandaloneClaudeCredentialSource,
    UnifiedGatewayConfig,
};
use crate::provider_auth::{
    NativeSecretStore, SecretStore, OPENROUTER_VAULT_ACCOUNT, OPENROUTER_VAULT_SERVICE,
};
use crate::provider_registry;

struct ClaudeApiKey(String);

impl StandaloneClaudeCredentialSource for ClaudeApiKey {
    fn credential(&self) -> Option<StandaloneClaudeCredential> {
        Some(StandaloneClaudeCredential::ApiKey(self.0.clone()))
    }
}

/// Start the API server and stop its listener on the first Ctrl+C.
#[allow(clippy::too_many_arguments)]
pub fn run(
    port: u16,
    claude: bool,
    codex: bool,
    openrouter: bool,
    deepseek: bool,
    kimi: bool,
) -> i32 {
    match start(port, claude, codex, openrouter, deepseek, kimi) {
        Ok(mut bridge) => {
            let (sender, receiver) = std::sync::mpsc::channel();
            if ctrlc::set_handler(move || {
                let _ = sender.send(());
            })
            .is_err()
            {
                eprintln!("clud serve: could not install interrupt handler");
                return 1;
            }
            println!("clud serve listening at {}", bridge.base_url());
            let _ = io::stdout().flush();
            let _ = receiver.recv();
            match bridge.shutdown() {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("clud serve: {error}");
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("clud serve: {error}");
            2
        }
    }
}

fn claude_api_key() -> Result<String, String> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Claude credentials are missing; set ANTHROPIC_API_KEY".to_string())?;
    if !key.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err("Claude credential is malformed".to_string());
    }
    Ok(key)
}

/// Read one Anthropic-compatible provider's stored key through the same
/// vault → fallback-file → env chain `clud auth login` writes to. The error
/// names the provider's own login command so a second/third served provider
/// stays actionable without a bespoke message per provider.
fn anthropic_compat_key(provider: ModelProvider) -> Result<String, String> {
    let descriptor = provider_registry::descriptor_for(provider)
        .ok_or_else(|| format!("no provider descriptor for {provider:?}"))?;
    let vault = NativeSecretStore::new_for(descriptor.vault_service, descriptor.vault_account)
        .map_err(|error| error.to_string())?;
    vault
        .get()
        .map_err(|error| error.to_string())?
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "{} credentials are missing; run `{}`",
                descriptor.display_name, descriptor.login_command
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn start(
    port: u16,
    claude: bool,
    codex: bool,
    openrouter: bool,
    deepseek: bool,
    kimi: bool,
) -> Result<BridgeHandle, String> {
    if !claude && !codex && !openrouter && !deepseek && !kimi {
        return Err(
            "select at least one of --claude, --codex, --openrouter, --deepseek, --kimi"
                .to_string(),
        );
    }
    let claude_key = claude.then(claude_api_key).transpose()?;
    if codex {
        crate::codex_upstream::ResolvedCredentials::preflight_default()
            .map_err(|error| error.to_string())?;
    }
    let client_token = std::env::var("CLUD_SERVE_CLIENT_TOKEN")
        .map_err(|_| "set CLUD_SERVE_CLIENT_TOKEN to a random client token".to_string())?;
    let openrouter_key = openrouter
        .then(|| {
            let vault =
                NativeSecretStore::new_for(OPENROUTER_VAULT_SERVICE, OPENROUTER_VAULT_ACCOUNT)
                    .map_err(|error| error.to_string())?;
            vault
                .get()
                .map_err(|error| error.to_string())?
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| {
                    "OpenRouter credentials are missing; run `clud auth login openrouter`"
                        .to_string()
                })
        })
        .transpose()?;
    let deepseek_key = deepseek
        .then(|| anthropic_compat_key(ModelProvider::DeepSeek))
        .transpose()?;
    let kimi_key = kimi
        .then(|| anthropic_compat_key(ModelProvider::Kimi))
        .transpose()?;
    for key in [
        claude_key.as_deref(),
        openrouter_key.as_deref(),
        deepseek_key.as_deref(),
        kimi_key.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if key == client_token {
            return Err("client token must differ from provider credentials".to_string());
        }
    }
    if codex && std::env::var("OPENAI_API_KEY").ok().as_deref() == Some(client_token.as_str()) {
        return Err("client token must differ from provider credentials".to_string());
    }
    let mut gateway = UnifiedGatewayConfig::new(None, codex).with_openrouter(openrouter_key);
    if let Some(upstreams) = crate::codex_bridge::unified_integration_upstreams_from_process() {
        gateway = gateway.with_standalone_test_upstreams(&upstreams);
    }
    // Added after the test overrides so a route's base URL is already the
    // overridden one when `with_route` resolves it.
    if deepseek_key.is_some() {
        gateway = gateway.with_route(ModelProvider::DeepSeek, deepseek_key);
    }
    if kimi_key.is_some() {
        gateway = gateway.with_route(ModelProvider::Kimi, kimi_key);
    }
    let mut config = BridgeConfig::default().with_unified_gateway(gateway);
    if let Some(key) = claude_key {
        config = config.with_standalone_claude_credential(Arc::new(ClaudeApiKey(key)));
    }
    let config = config
        .with_standalone_listener(port, client_token)
        .map_err(|error| error.to_string())?;
    BridgeHandle::start(config).map_err(|error| error.to_string())
}

//! Action-first provider credential commands (issue #898).
//!
//! This module owns only the CLI grammar and secret-free aggregate status view.
//! Provider-specific vault and OAuth behavior remains in its existing module.

use std::sync::atomic::AtomicBool;

use crate::args::{AuthProvider, AuthSubcommand, CodexAuthSubcommand, DeepseekAuthSubcommand};
use crate::backend::ModelProvider;
use crate::provider_auth::{CredentialBackend, NativeSecretStore, SecretStore};
use crate::provider_registry::{self, AnthropicCompatProvider};

/// Maps the auth-command's provider selector onto the model-provider space
/// the registry is keyed by. Exhaustive: a new `AuthProvider` variant fails
/// to compile here until it is routed somewhere.
fn model_provider_for(provider: AuthProvider) -> ModelProvider {
    match provider {
        AuthProvider::Claude => ModelProvider::Claude,
        AuthProvider::Codex => ModelProvider::Codex,
        AuthProvider::Deepseek => ModelProvider::DeepSeek,
        AuthProvider::Kimi => ModelProvider::Kimi,
        AuthProvider::Openrouter => ModelProvider::OpenRouter,
    }
}

/// Anthropic-compat descriptor for an `AuthProvider`, when it has one.
/// `None` for Claude (externally managed) and Codex (its own translation-
/// bridge auth path, not a vault-backed Anthropic-compat provider).
fn anthropic_compat_descriptor(provider: AuthProvider) -> Option<&'static AnthropicCompatProvider> {
    provider_registry::descriptor_for(model_provider_for(provider))
}

/// Inverse of [`model_provider_for`]. Exhaustive for the same reason.
fn auth_provider_for(provider: ModelProvider) -> AuthProvider {
    match provider {
        ModelProvider::Claude => AuthProvider::Claude,
        ModelProvider::Codex => AuthProvider::Codex,
        ModelProvider::DeepSeek => AuthProvider::Deepseek,
        ModelProvider::Kimi => AuthProvider::Kimi,
        ModelProvider::OpenRouter => AuthProvider::Openrouter,
    }
}

/// Exact action-first spelling for a deprecated Codex alias invocation.
pub fn codex_alias_replacement(subcommand: &CodexAuthSubcommand) -> String {
    match subcommand {
        CodexAuthSubcommand::Login {
            acknowledge_experimental,
            no_browser,
        } => {
            let mut command = "clud auth login codex".to_string();
            if *acknowledge_experimental {
                command.push_str(" --acknowledge-experimental");
            }
            if *no_browser {
                command.push_str(" --no-browser");
            }
            command
        }
        CodexAuthSubcommand::Status { json } => replacement_with_json("status", "codex", *json),
        CodexAuthSubcommand::Logout { json } => replacement_with_json("logout", "codex", *json),
    }
}

/// Exact action-first spelling for a deprecated DeepSeek alias invocation.
pub fn deepseek_alias_replacement(subcommand: &DeepseekAuthSubcommand) -> String {
    let descriptor = anthropic_compat_descriptor(AuthProvider::Deepseek)
        .expect("DeepSeek has an Anthropic-compat descriptor");
    match subcommand {
        DeepseekAuthSubcommand::Login => descriptor.login_command.to_string(),
        DeepseekAuthSubcommand::Status { json } => {
            replacement_with_json("status", descriptor.settings_id, *json)
        }
        DeepseekAuthSubcommand::Logout { json } => {
            replacement_with_json("logout", descriptor.settings_id, *json)
        }
    }
}

fn replacement_with_json(action: &str, provider: &str, json: bool) -> String {
    format!(
        "clud auth {action} {provider}{}",
        if json { " --json" } else { "" }
    )
}

pub fn run(subcommand: Option<&AuthSubcommand>, interrupted: &AtomicBool) -> i32 {
    match subcommand {
        Some(AuthSubcommand::Login(options)) => {
            let crate::args::AuthLoginArgs {
                provider,
                acknowledge_experimental,
                no_browser,
            } = options.as_ref();
            login(
                *provider,
                *acknowledge_experimental,
                *no_browser,
                interrupted,
            )
        }
        Some(AuthSubcommand::Status { provider, json }) => status(*provider, *json),
        Some(AuthSubcommand::Logout { provider, json }) => logout(*provider, *json),
        None => {
            println!("Usage: clud auth <login|status|logout> <provider>");
            // Built from the same registry `status()` iterates, so a future
            // vault-backed provider (or a removed one) can't drift this
            // usage line out of sync with what the command actually accepts.
            let vault_backed: Vec<&str> = ModelProvider::ALL
                .iter()
                .copied()
                .filter(|provider| provider_registry::descriptor_for(*provider).is_some())
                .map(|provider| provider.as_str())
                .collect();
            let codex_and_vault_backed = std::iter::once("codex")
                .chain(vault_backed)
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "Providers: {codex_and_vault_backed} (Claude authentication is externally managed)"
            );
            0
        }
    }
}

fn login(
    provider: AuthProvider,
    acknowledge_experimental: bool,
    no_browser: bool,
    interrupted: &AtomicBool,
) -> i32 {
    match provider {
        AuthProvider::Codex => crate::codex_auth::run(
            &CodexAuthSubcommand::Login {
                acknowledge_experimental,
                no_browser,
            },
            interrupted,
        ),
        AuthProvider::Deepseek | AuthProvider::Kimi | AuthProvider::Openrouter => {
            crate::provider_auth::run_for(
                anthropic_compat_descriptor(provider)
                    .expect("vault-backed auth provider has an Anthropic-compat descriptor"),
                &DeepseekAuthSubcommand::Login,
            )
        }
        AuthProvider::Claude => externally_managed("login"),
    }
}

fn logout(provider: AuthProvider, json: bool) -> i32 {
    match provider {
        AuthProvider::Codex => crate::codex_auth::run(
            &CodexAuthSubcommand::Logout { json },
            &AtomicBool::new(false),
        ),
        AuthProvider::Deepseek | AuthProvider::Kimi | AuthProvider::Openrouter => {
            crate::provider_auth::run_for(
                anthropic_compat_descriptor(provider)
                    .expect("vault-backed auth provider has an Anthropic-compat descriptor"),
                &DeepseekAuthSubcommand::Logout { json },
            )
        }
        AuthProvider::Claude => externally_managed("logout"),
    }
}

fn externally_managed(action: &str) -> i32 {
    eprintln!(
        "Claude authentication is managed by Claude Code; clud cannot {action} Claude credentials"
    );
    2
}

fn status(provider: Option<AuthProvider>, json: bool) -> i32 {
    let providers = match provider {
        Some(provider) => vec![provider],
        // `ModelProvider::ALL` is the registry's ordered source of truth;
        // mapping through it (rather than a second hand-written array here)
        // keeps this listing's order in lockstep with every other
        // provider-enumeration surface. Its order happens to already be
        // Claude, Codex, DeepSeek -- this listing's existing output order.
        None => ModelProvider::ALL
            .iter()
            .copied()
            .map(auth_provider_for)
            .collect(),
    };
    let rows: Vec<serde_json::Value> = providers.into_iter().map(status_row).collect();
    if json {
        println!("{}", serde_json::json!({"providers": rows}));
    } else {
        for row in &rows {
            println!(
                "{:<9} {:<18} {}",
                row["provider"].as_str().unwrap_or("unknown"),
                row["source"].as_str().unwrap_or("unknown"),
                row["status"].as_str().unwrap_or("unknown"),
            );
        }
    }
    if provider.is_some() && !rows[0]["configured"].as_bool().unwrap_or(false) {
        1
    } else {
        0
    }
}

fn status_row(provider: AuthProvider) -> serde_json::Value {
    match provider {
        AuthProvider::Claude => serde_json::json!({
            "provider": provider.as_str(),
            "source": "claude_code",
            "status": "externally_managed",
            "configured": true,
        }),
        AuthProvider::Codex => {
            let configured = crate::home::user_home()
                .and_then(|home| crate::codex_auth::load_at(&home).ok().flatten())
                .is_some()
                || std::env::var("OPENAI_API_KEY")
                    .ok()
                    .is_some_and(|value| !value.trim().is_empty());
            serde_json::json!({
                "provider": provider.as_str(),
                "source": "clud_chatgpt_subscription_or_openai_api_key",
                "status": if configured { "configured" } else { "login_required" },
                "configured": configured,
            })
        }
        AuthProvider::Deepseek | AuthProvider::Kimi | AuthProvider::Openrouter => {
            let descriptor = anthropic_compat_descriptor(provider)
                .expect("vault-backed auth provider has an Anthropic-compat descriptor");
            let store =
                NativeSecretStore::new_for(descriptor.vault_service, descriptor.vault_account).ok();
            let configured = store
                .as_ref()
                .and_then(|store| store.get().ok().flatten())
                .is_some();
            let source = store.map_or(CredentialBackend::NativeVault, |store| store.backend());
            serde_json::json!({
                "provider": provider.as_str(),
                "source": source.as_str(),
                "status": if configured { "configured" } else { "login_required" },
                "configured": configured,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_status_is_secret_free_and_externally_managed() {
        let row = status_row(AuthProvider::Claude).to_string();
        assert!(row.contains("externally_managed"));
        assert!(!row.contains("token"));
    }

    #[test]
    fn claude_credential_mutation_is_rejected() {
        assert_eq!(externally_managed("login"), 2);
    }

    #[test]
    fn kimi_status_row_uses_the_native_vault_source() {
        let row = status_row(AuthProvider::Kimi);
        assert_eq!(row["provider"], "kimi");
        assert_eq!(row["source"], "native_vault");
    }

    #[test]
    fn kimi_and_deepseek_status_rows_never_share_a_configured_vault_entry() {
        // Both descriptors resolve independently through the registry; this
        // just proves the auth surface routes Kimi through its own
        // descriptor rather than accidentally reusing DeepSeek's.
        assert_ne!(
            anthropic_compat_descriptor(AuthProvider::Kimi)
                .unwrap()
                .vault_service,
            anthropic_compat_descriptor(AuthProvider::Deepseek)
                .unwrap()
                .vault_service,
        );
    }

    #[test]
    fn openrouter_auth_reuses_the_vault_path_with_an_isolated_record() {
        let openrouter = anthropic_compat_descriptor(AuthProvider::Openrouter).unwrap();
        assert_eq!(openrouter.vault_service, "clud.openrouter");
        assert_eq!(openrouter.login_command, "clud auth login openrouter");
        assert_ne!(
            openrouter.vault_service,
            anthropic_compat_descriptor(AuthProvider::Deepseek)
                .unwrap()
                .vault_service
        );
        assert_eq!(
            status_row(AuthProvider::Openrouter)["source"],
            "native_vault"
        );
    }

    #[test]
    fn deprecated_alias_replacements_preserve_every_flag() {
        assert_eq!(
            codex_alias_replacement(&CodexAuthSubcommand::Login {
                acknowledge_experimental: true,
                no_browser: true,
            }),
            "clud auth login codex --acknowledge-experimental --no-browser"
        );
        assert_eq!(
            codex_alias_replacement(&CodexAuthSubcommand::Status { json: true }),
            "clud auth status codex --json"
        );
        assert_eq!(
            deepseek_alias_replacement(&DeepseekAuthSubcommand::Logout { json: true }),
            "clud auth logout deepseek --json"
        );
        assert_eq!(
            deepseek_alias_replacement(&DeepseekAuthSubcommand::Login),
            "clud auth login deepseek"
        );
    }
}

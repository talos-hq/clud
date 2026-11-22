//! The durable client token for `clud serve`.
//!
//! `clud serve` authenticates its local callers with one client token. An
//! operator can pass it per run in `CLUD_SERVE_CLIENT_TOKEN`; a product that
//! clud configured (`clud install hermes`) needs a token that survives
//! restarts, so clud keeps one in its own credential store. The environment
//! variable wins when set, so an explicit per-run token is never overridden.
//! See docs/architecture/standalone-gateway.md.

use base64::Engine;

use crate::provider_auth::{NativeSecretStore, SecretStore};

pub const ENV: &str = "CLUD_SERVE_CLIENT_TOKEN";
const VAULT_SERVICE: &str = "clud-serve";
const VAULT_ACCOUNT: &str = "client-token";

fn vault() -> Result<NativeSecretStore, String> {
    NativeSecretStore::new_for(VAULT_SERVICE, VAULT_ACCOUNT).map_err(|error| error.to_string())
}

/// A fresh random token: 32 bytes from the OS CSPRNG, URL-safe base64.
fn generate() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| format!("OS CSPRNG unavailable: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The token `clud serve` checks: the environment first, then the stored one.
pub fn resolve() -> Result<String, String> {
    resolve_with(std::env::var(ENV).ok(), &vault()?)
}

fn resolve_with(env: Option<String>, store: &dyn SecretStore) -> Result<String, String> {
    if let Some(token) = env.filter(|value| !value.trim().is_empty()) {
        return Ok(token);
    }
    store
        .get()
        .map_err(|error| error.to_string())?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "set {ENV} to a random client token, or run `clud install hermes --configure-only` to store one"
            )
        })
}

/// The stored token, created on first use. Used by the configure step that
/// hands the token to a product.
pub fn stored_or_create() -> Result<String, String> {
    stored_or_create_with(&vault()?, generate)
}

fn stored_or_create_with(
    store: &dyn SecretStore,
    generate: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    if let Some(token) = store
        .get()
        .map_err(|error| error.to_string())?
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(token);
    }
    let token = generate()?;
    store.set(&token).map_err(|error| error.to_string())?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_auth::SecretStoreError;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Memory(RefCell<Option<String>>);

    impl SecretStore for Memory {
        fn get(&self) -> Result<Option<String>, SecretStoreError> {
            Ok(self.0.borrow().clone())
        }
        fn set(&self, secret: &str) -> Result<(), SecretStoreError> {
            *self.0.borrow_mut() = Some(secret.to_string());
            Ok(())
        }
        fn delete(&self) -> Result<(), SecretStoreError> {
            *self.0.borrow_mut() = None;
            Ok(())
        }
    }

    #[test]
    fn the_environment_wins_over_the_stored_token() {
        let store = Memory(RefCell::new(Some("stored".into())));
        assert_eq!(resolve_with(Some("env".into()), &store).unwrap(), "env");
    }

    #[test]
    fn the_stored_token_is_used_without_the_environment() {
        let store = Memory(RefCell::new(Some("stored".into())));
        assert_eq!(resolve_with(None, &store).unwrap(), "stored");
        assert_eq!(resolve_with(Some("  ".into()), &store).unwrap(), "stored");
    }

    #[test]
    fn no_token_anywhere_names_both_remedies() {
        let error = resolve_with(None, &Memory::default()).unwrap_err();
        assert!(error.contains(ENV), "{error}");
        assert!(error.contains("clud install hermes"), "{error}");
    }

    #[test]
    fn a_token_is_created_once_and_then_reused() {
        let store = Memory::default();
        let first = stored_or_create_with(&store, || Ok("generated".into())).unwrap();
        assert_eq!(first, "generated");
        let second =
            stored_or_create_with(&store, || panic!("must not regenerate an existing token"))
                .unwrap();
        assert_eq!(second, "generated");
    }

    #[test]
    fn generated_tokens_meet_the_serve_minimum_and_differ() {
        let one = generate().unwrap();
        let two = generate().unwrap();
        assert!(one.len() >= 32 && one.bytes().all(|b| b.is_ascii_graphic()));
        assert_ne!(one, two);
    }
}

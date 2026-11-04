//! The configure step of `clud install <product>` (#2609): offer to migrate
//! clud's provider credentials into the product that was just installed.
//!
//! Two layers, so the interesting half is testable without a credential store:
//!
//! - [`stored`] reads presence (and, only when migrating, the value) from the
//!   same vault `clud auth` uses;
//! - [`plan`] is a pure text edit of the product's `config.yaml`: insert-only,
//!   so comments, unknown keys and other providers survive, and a document that
//!   does not parse is refused before anything is written.
//!
//! The boundary: a secret is never printed, only the file and the provider are
//! named. See docs/architecture/install-products.md.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::products::Product;
use crate::provider_auth::SecretStore;

/// One provider credential clud holds and can hand to a product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    /// `openrouter`, `deepseek`, `kimi` — the product's provider key.
    pub id: &'static str,
    pub display: &'static str,
    /// OpenAI-compatible base URL the product's config wants.
    pub base_url: String,
    /// The secret. Never printed; written straight into the product's file.
    pub key: String,
}

/// What the offer found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offer {
    pub providers: Vec<String>,
    pub models: Vec<String>,
}

impl Offer {
    /// One report line, secret-free.
    pub fn summary(&self) -> String {
        if self.providers.is_empty() {
            return "providers: nothing stored in clud; nothing to migrate".to_string();
        }
        let mut line = format!("providers: {}", self.providers.join(", "));
        if !self.models.is_empty() {
            line.push_str(&format!(" (models: {})", self.models.join(", ")));
        }
        line
    }
}

/// An unrecoverable refusal: the config is left exactly as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The OpenAI-compatible endpoint for a provider, derived from the
/// Anthropic-compatible one the registry already publishes: drop the
/// `/<vendor>` tail OpenRouter-style base URLs carry, then use `/v1`.
pub fn openai_base_url(anthropic_base_url: &str) -> String {
    let trimmed = anthropic_base_url.trim_end_matches('/');
    let without_tail = trimmed.strip_suffix("/anthropic").unwrap_or(trimmed);
    format!("{without_tail}/v1")
}

/// Presence check for every provider clud can migrate. Reads the vault only;
/// never prints a value.
pub fn offer() -> Offer {
    let mut found = Offer::default();
    for descriptor in crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS {
        let present = crate::provider_auth::has_well_formed_stored_key(descriptor).unwrap_or(false);
        if present {
            found.providers.push(descriptor.settings_id.to_string());
        }
    }
    found
}

/// Read the credentials to migrate. A provider whose key cannot be read is
/// skipped rather than failing the step.
pub fn stored() -> Vec<Credential> {
    let mut found = Vec::new();
    for descriptor in crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS {
        let Ok(store) = crate::provider_auth::NativeSecretStore::new_for(
            descriptor.vault_service,
            descriptor.vault_account,
        ) else {
            continue;
        };
        let Ok(Some(key)) = store.get() else {
            continue;
        };
        if key.trim().is_empty() {
            continue;
        }
        found.push(Credential {
            id: descriptor.settings_id,
            display: descriptor.display_name,
            base_url: openai_base_url(descriptor.anthropic_base_url),
            key,
        });
    }
    found
}

/// The product's config file inside its home.
pub fn config_path(home: &Path, product: &Product) -> PathBuf {
    home.join(product.config_file)
}

/// Apply `credentials` to `document`, returning the new text.
///
/// Insert-only: existing lines (comments, other providers, unknown keys) are
/// kept. A provider that already exists gets its `base_url`/`api_key` lines
/// replaced in place; a new one is added to the `providers:` mapping.
pub fn plan(document: &str, credentials: &[Credential]) -> Result<String, ConfigError> {
    if !document.trim().is_empty() {
        let _: serde_yaml::Value = serde_yaml::from_str(document)
            .map_err(|error| ConfigError(format!("config does not parse: {error}")))?;
    }
    let mut lines: Vec<String> = document.lines().map(|line| line.to_string()).collect();
    let mut added_provider_key = false;
    for credential in credentials {
        lines = upsert_provider(&lines, credential);
        added_provider_key = true;
    }
    let mut text = lines.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    // The document must parse *and* carry what we set, or nothing is written.
    let parsed: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|error| ConfigError(format!("edited config does not parse: {error}")))?;
    for credential in credentials {
        let value = parsed
            .get("providers")
            .and_then(|providers| providers.get(credential.id))
            .ok_or_else(|| ConfigError(format!("providers.{} did not land", credential.id)))?;
        let base = value.get("base_url").and_then(|v| v.as_str());
        if base != Some(credential.base_url.as_str()) {
            return Err(ConfigError(format!(
                "providers.{}.base_url did not land",
                credential.id
            )));
        }
        if added_provider_key && value.get("api_key").and_then(|v| v.as_str()).is_none() {
            return Err(ConfigError(format!(
                "providers.{}.api_key did not land",
                credential.id
            )));
        }
    }
    Ok(text)
}

/// Insert or update one provider inside the `providers:` mapping.
fn upsert_provider(lines: &[String], credential: &Credential) -> Vec<String> {
    let block = vec![
        format!("  {}:", credential.id),
        format!("    base_url: \"{}\"", credential.base_url),
        format!("    api_key: \"{}\"", credential.key),
    ];
    let Some(providers_at) = lines.iter().position(|line| line == "providers:") else {
        let mut out = lines.to_vec();
        if !out.is_empty() && out.last().is_some_and(|line| !line.trim().is_empty()) {
            out.push(String::new());
        }
        out.push("providers:".to_string());
        out.extend(block);
        return out;
    };
    let mut end = lines.len();
    for (index, line) in lines.iter().enumerate().skip(providers_at + 1) {
        if !line.is_empty() && !line.starts_with(' ') {
            end = index;
            break;
        }
    }
    let provider_header = format!("  {}:", credential.id);
    let existing =
        (providers_at + 1..end).find(|index| lines[*index].trim_end() == provider_header);
    let Some(existing) = existing else {
        let mut out = lines.to_vec();
        out.splice(end..end, block);
        return out;
    };
    let provider_end = (existing + 1..end)
        .find(|index| !lines[*index].is_empty() && !lines[*index].starts_with("    "))
        .unwrap_or(end);
    let mut out = lines.to_vec();
    let mut replaced_base = false;
    let mut replaced_key = false;
    let mut index = existing + 1;
    while index < provider_end {
        let trimmed = out[index].trim_start();
        if trimmed.starts_with("base_url:") {
            out[index] = format!("    base_url: \"{}\"", credential.base_url);
            replaced_base = true;
        } else if trimmed.starts_with("api_key:") {
            out[index] = format!("    api_key: \"{}\"", credential.key);
            replaced_key = true;
        }
        index += 1;
    }
    let mut insert_at = provider_end;
    if !replaced_key {
        out.insert(insert_at, format!("    api_key: \"{}\"", credential.key));
        insert_at += 1;
    }
    if !replaced_base {
        out.insert(
            insert_at,
            format!("    base_url: \"{}\"", credential.base_url),
        );
    }
    out
}

/// Outcome of the configure step, for the caller's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing stored in clud: nothing to offer.
    NothingStored,
    /// The user (or a flag) declined.
    Declined,
    /// No terminal and no flag: the step never prompts.
    NoTerminal,
    /// Written; names the file and the providers, never a value.
    Migrated {
        path: PathBuf,
        providers: Vec<String>,
    },
    /// The product's config could not be edited; nothing was written.
    Refused(String),
}

/// Run the step against the real credential store and terminal.
pub fn run(
    product: &Product,
    product_home: &Path,
    migrate: bool,
    yes: bool,
    interactive: bool,
    out: &mut dyn Write,
) -> i32 {
    let found = offer();
    let _ = writeln!(out, "{}", found.summary());
    if found.providers.is_empty() {
        return 0;
    }
    let wanted = if migrate {
        true
    } else if yes || !interactive {
        let _ = writeln!(
            out,
            "configure: leaving {} alone; rerun with --migrate-credentials to copy them",
            product.display
        );
        return 0;
    } else {
        let _ = write!(
            out,
            "Would you like to migrate clud credentials for providers into {}? [y/N] ",
            product.display
        );
        let _ = out.flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return 0;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    if !wanted {
        let _ = writeln!(out, "configure: left {} unchanged", product.display);
        return 0;
    }
    match apply(product, product_home, &stored(), out) {
        Outcome::Migrated { path, providers } => {
            let _ = writeln!(
                out,
                "configure: wrote {} provider(s) to {}",
                providers.join(", "),
                path.display()
            );
            0
        }
        Outcome::NothingStored => 0,
        Outcome::Refused(reason) => {
            eprintln!("configure: {reason}");
            1
        }
        Outcome::Declined | Outcome::NoTerminal => 0,
    }
}

/// Write `credentials` into the product's config, atomically.
pub fn apply(
    product: &Product,
    product_home: &Path,
    credentials: &[Credential],
    out: &mut dyn Write,
) -> Outcome {
    if credentials.is_empty() {
        return Outcome::NothingStored;
    }
    let path = config_path(product_home, product);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Outcome::Refused(format!("cannot read {}: {error}", path.display())),
    };
    let text = match plan(&existing, credentials) {
        Ok(text) => text,
        Err(error) => {
            return Outcome::Refused(format!("{} left untouched: {error}", path.display()))
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return Outcome::Refused(format!("cannot create {}: {error}", parent.display()));
        }
    }
    let staged = path.with_extension("yaml.clud-new");
    if let Err(error) = std::fs::write(&staged, &text) {
        return Outcome::Refused(format!("cannot write {}: {error}", staged.display()));
    }
    if let Err(error) = std::fs::rename(&staged, &path) {
        let _ = std::fs::remove_file(&staged);
        return Outcome::Refused(format!("cannot replace {}: {error}", path.display()));
    }
    let _ = out;
    Outcome::Migrated {
        path,
        providers: credentials.iter().map(|c| c.id.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(id: &'static str, base: &str, key: &str) -> Credential {
        Credential {
            id,
            display: id,
            base_url: base.to_string(),
            key: key.to_string(),
        }
    }

    fn openrouter() -> Credential {
        credential("openrouter", "https://openrouter.ai/api/v1", "sk-or-secret")
    }

    #[test]
    fn openai_urls_are_derived_from_the_anthropic_ones() {
        assert_eq!(
            openai_base_url("https://openrouter.ai/api"),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            openai_base_url("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(
            openai_base_url("https://api.moonshot.ai/anthropic/"),
            "https://api.moonshot.ai/v1"
        );
    }

    #[test]
    fn an_empty_document_gains_a_providers_block() {
        let text = plan("", &[openrouter()]).expect("plan");
        assert_eq!(
            text,
            "providers:\n  openrouter:\n    base_url: \"https://openrouter.ai/api/v1\"\n    api_key: \"sk-or-secret\"\n"
        );
    }

    #[test]
    fn a_new_provider_joins_an_existing_mapping_and_keeps_comments() {
        let existing = "# my agent\nmodel:\n  default: \"x/y\"\nproviders:\n  # keep me\n  other:\n    base_url: \"https://other/v1\"\n    api_key: \"k\"\n";
        let text = plan(existing, &[openrouter()]).expect("plan");
        assert!(text.contains("# my agent"), "{text}");
        assert!(text.contains("# keep me"), "{text}");
        assert!(
            text.contains("  other:\n    base_url: \"https://other/v1\""),
            "{text}"
        );
        assert!(text.contains("  openrouter:\n"), "{text}");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&text).expect("yaml");
        assert_eq!(
            parsed["providers"]["openrouter"]["base_url"].as_str(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[test]
    fn an_existing_provider_has_its_values_replaced_in_place() {
        let existing = "providers:\n  openrouter:\n    base_url: \"https://old/v1\"\n    api_key: \"old\"\n    # keep this comment\n    extra: 1\n";
        let text = plan(existing, &[openrouter()]).expect("plan");
        assert!(!text.contains("https://old/v1"), "{text}");
        assert!(!text.contains("\"old\""), "{text}");
        assert!(text.contains("# keep this comment"), "{text}");
        assert!(text.contains("extra: 1"), "{text}");
    }

    #[test]
    fn a_missing_api_key_is_added_to_an_existing_provider() {
        let existing =
            "providers:\n  openrouter:\n    base_url: \"https://openrouter.ai/api/v1\"\n";
        let text = plan(existing, &[openrouter()]).expect("plan");
        assert!(text.contains("api_key: \"sk-or-secret\""), "{text}");
    }

    #[test]
    fn a_document_that_does_not_parse_is_refused() {
        let error = plan("providers: [unterminated\n", &[openrouter()]).expect_err("refused");
        assert!(error.to_string().contains("does not parse"), "{error}");
    }

    #[test]
    fn the_offer_summary_is_secret_free_and_names_the_emptiness() {
        assert_eq!(
            Offer::default().summary(),
            "providers: nothing stored in clud; nothing to migrate"
        );
        let offer = Offer {
            providers: vec!["openrouter".to_string()],
            models: Vec::new(),
        };
        assert_eq!(offer.summary(), "providers: openrouter");
        assert!(!offer.summary().contains("sk-"));
    }

    #[test]
    fn apply_writes_the_file_and_creates_the_home() {
        let home = tempdir();
        let product = super::super::products::find("hermes").expect("hermes");
        let mut out = Vec::new();
        let outcome = apply(product, &home, &[openrouter()], &mut out);
        match outcome {
            Outcome::Migrated { path, providers } => {
                assert_eq!(providers, vec!["openrouter".to_string()]);
                let text = std::fs::read_to_string(&path).expect("read");
                assert!(text.contains("sk-or-secret"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn apply_leaves_an_unparseable_file_byte_identical() {
        let home = tempdir();
        std::fs::create_dir_all(&home).expect("home");
        let path = home.join("config.yaml");
        std::fs::write(&path, "providers: [oops\n").expect("seed");
        let product = super::super::products::find("hermes").expect("hermes");
        let mut out = Vec::new();
        let outcome = apply(product, &home, &[openrouter()], &mut out);
        assert!(matches!(outcome, Outcome::Refused(_)), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "providers: [oops\n"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn nothing_stored_writes_nothing() {
        let home = tempdir();
        let product = super::super::products::find("hermes").expect("hermes");
        let mut out = Vec::new();
        assert_eq!(apply(product, &home, &[], &mut out), Outcome::NothingStored);
        assert!(!home.join("config.yaml").exists());
        let _ = std::fs::remove_dir_all(home);
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "clud-configure-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("tempdir");
        base
    }
}

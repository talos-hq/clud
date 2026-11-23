//! The registry of server settings sections, and the sections themselves.
//!
//! To add a setting: define a `Deserialize` struct, implement [`Section`] with
//! its key and semantic checks, list it in [`SECTIONS`], and add its built-in
//! value under `sections` in `assets/server-settings.json`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use super::{Section, SectionSpec};
use crate::backend::ModelProvider;
use crate::provider_catalog;

/// Every section this build validates. A section missing here is treated as
/// unknown: kept verbatim in the cache, never decoded.
pub(crate) const SECTIONS: &[SectionSpec] = &[
    SectionSpec::of::<CodexCompatibility>(),
    SectionSpec::of::<DeepSeekSettings>(),
    SectionSpec::of::<ModelContexts>(),
    SectionSpec::of::<PerTurnEffort>(),
];

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CodexCompatibility {
    pub(crate) client_version: String,
}

impl Section for CodexCompatibility {
    const KEY: &'static str = "codex_compatibility";

    fn validate(&self) -> Result<(), String> {
        let mut parts = self.client_version.split('.');
        let valid = (0..3).all(|_| {
            parts.next().is_some_and(|part| {
                part.parse::<u16>()
                    .is_ok_and(|number| number.to_string() == part)
            })
        }) && parts.next().is_none();
        valid
            .then_some(())
            .ok_or_else(|| "expected a numeric Codex CLI release version".to_string())
    }
}

#[cfg(test)]
#[test]
fn codex_compatibility_accepts_only_canonical_release_versions() {
    for (version, valid) in [
        ("0.171.0", true),
        ("1.0.0", true),
        ("0.171", false),
        ("0.0171.0", false),
        ("0.171.0-beta.1", false),
        ("0.171.0\r\nHost: example.test", false),
    ] {
        let settings = CodexCompatibility {
            client_version: version.to_string(),
        };
        assert_eq!(settings.validate().is_ok(), valid, "{version:?}");
    }

    let settings: CodexCompatibility =
        serde_json::from_str(r#"{"client_version":"0.171.0","future_field":true}"#).unwrap();
    assert!(settings.validate().is_ok());
}

/// DeepSeek model names. DeepSeek renames API slugs in place
/// (`deepseek-v4-flash` became `deepseek-flash` with V4.1-Flash), so these
/// names must be changeable without a clud release. The values can only name
/// `deepseek-*` models: never another provider, a URL, or a credential.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeepSeekSettings {
    /// Direct-launch default when neither `--model` nor a saved profile names
    /// one. Resolved through the catalog exactly like a CLI spelling.
    pub default_model: String,
    /// Wire ID for Claude Code's haiku and subagent slots.
    pub subagent_model: String,
}

const MAX_MODEL_ID_LEN: usize = 64;

impl Section for DeepSeekSettings {
    const KEY: &'static str = "deepseek";

    fn validate(&self) -> Result<(), String> {
        for (field, value) in [
            ("default_model", &self.default_model),
            ("subagent_model", &self.subagent_model),
        ] {
            if !is_deepseek_model_id(value) {
                return Err(format!("{field} is not a DeepSeek model ID"));
            }
        }
        Ok(())
    }
}

fn is_deepseek_model_id(value: &str) -> bool {
    let base = value.strip_suffix("[1m]").unwrap_or(value);
    !base.is_empty()
        && base.len() <= MAX_MODEL_ID_LEN
        && base
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && provider_catalog::infer_provider(base) == Some(ModelProvider::DeepSeek)
}

/// DeepSeek's settings for this process.
pub fn deepseek() -> &'static DeepSeekSettings {
    static SETTINGS: OnceLock<DeepSeekSettings> = OnceLock::new();
    SETTINGS.get_or_init(|| super::snapshot().section())
}

/// Served default model for `provider`, if its model names are served.
pub fn provider_default_model(provider: ModelProvider) -> Option<&'static str> {
    (provider == ModelProvider::DeepSeek).then(|| deepseek().default_model.as_str())
}

/// Served haiku/subagent wire ID for `provider`, if its model names are served.
pub fn provider_subagent_model(provider: ModelProvider) -> Option<&'static str> {
    (provider == ModelProvider::DeepSeek).then(|| deepseek().subagent_model.as_str())
}

/// Exact context windows by wire ID, served from OpenRouter's public model
/// datasheet (#1258). The static catalog stays reviewed (DD-054); this map is
/// machine-published on a schedule so newly listed models get their real
/// window without a clud release. Keys are exact wire IDs (never a `[1m]`
/// suffix — the harness interprets that itself), values are the datasheet's
/// `context_length` in tokens.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelContexts {
    /// Flat `{ "<wire-id>": <context window in tokens> }`.
    #[serde(flatten)]
    pub windows: BTreeMap<String, u32>,
}

/// Datasheet bounds, mirrored by `ci/refresh_model_contexts.py`; change both
/// together. Anything outside this range is corrupt data, not a window.
const MIN_CONTEXT_TOKENS: u32 = 1_000;
const MAX_CONTEXT_TOKENS: u32 = 10_000_000;
const MAX_CONTEXT_MODEL_ID_LEN: usize = 128;

impl Section for ModelContexts {
    const KEY: &'static str = "model_contexts";

    fn validate(&self) -> Result<(), String> {
        for (model_id, tokens) in &self.windows {
            if model_id.is_empty() || model_id.len() > MAX_CONTEXT_MODEL_ID_LEN {
                return Err(format!(
                    "model id {model_id:?} is empty or longer than \
                     {MAX_CONTEXT_MODEL_ID_LEN} bytes"
                ));
            }
            if !model_id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':' | b'~')
            }) {
                return Err(format!(
                    "model id {model_id:?} contains characters outside the wire-ID charset"
                ));
            }
            if !(MIN_CONTEXT_TOKENS..=MAX_CONTEXT_TOKENS).contains(tokens) {
                return Err(format!(
                    "{model_id}: context window {tokens} outside \
                     {MIN_CONTEXT_TOKENS}..={MAX_CONTEXT_TOKENS}"
                ));
            }
        }
        Ok(())
    }
}

/// Exact served context windows by wire ID (#1258).
pub fn model_contexts() -> &'static ModelContexts {
    static SETTINGS: OnceLock<ModelContexts> = OnceLock::new();
    SETTINGS.get_or_init(|| super::snapshot().section())
}

/// The window to teach the harness for `wire_id` (#1258): the catalog's
/// reviewed `claude_max_context_tokens` wins when it has one, otherwise the
/// served datasheet row, and `None` when neither source knows the ID.
pub fn effective_context_window(wire_id: &str) -> Option<u32> {
    effective_context_window_with_source(wire_id).map(|(tokens, _)| tokens)
}

/// Which source [`effective_context_window`] took its value from (#1675).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWindowSource {
    /// A reviewed `claude_max_context_tokens` catalog row.
    Catalog,
    /// The served `model_contexts` datasheet section (#1258).
    Served,
}

/// [`effective_context_window`] plus the source it came from, so the
/// launch-context record (#1675) can name it without a second lookup path.
pub fn effective_context_window_with_source(wire_id: &str) -> Option<(u32, ContextWindowSource)> {
    if let Some(tokens) = provider_catalog::model_by_wire_id(wire_id)
        .and_then(|entry| entry.claude_max_context_tokens)
    {
        return Some((tokens, ContextWindowSource::Catalog));
    }
    if wire_id.starts_with("gpt-") {
        let choice = crate::codex_runtime::active_choice();
        if let Some(tokens) = dynamic_codex_context_window(wire_id, choice) {
            return Some((tokens, ContextWindowSource::Catalog));
        }
    }
    model_contexts()
        .windows
        .get(wire_id)
        .copied()
        .map(|tokens| (tokens, ContextWindowSource::Served))
}

fn dynamic_codex_context_window(
    wire_id: &str,
    choice: &crate::codex_runtime::Choice,
) -> Option<u32> {
    let cli_id = if wire_id == choice.sol {
        "codex-sol"
    } else if wire_id == choice.luna {
        "codex-luna"
    } else if wire_id == choice.terra {
        "codex-terra"
    } else {
        return None;
    };
    provider_catalog::model_by_cli_id(cli_id).and_then(|entry| entry.claude_max_context_tokens)
}

/// The flip for the direct-route per-turn-effort injection (#1528).
///
/// Claude Code keeps its prompt cache across a mid-session effort change only
/// when its client-side `per_turn_effort` capability is on for the launched
/// wire ID. clud can supply that through `CLAUDE_CODE_MODEL_CAPABILITIES`, but
/// the artifact's `supports_reasoning_effort` gate only says the model accepts
/// an effort setting -- OpenRouter publishes nothing about per-turn *delivery*,
/// so the claim is unverified until a live check of its `/api/v1/messages`
/// endpoint records both acceptance and the honored level (issue #1528, open
/// items 1 and 2). This section is that switch, so the follow-up flip lands
/// without a clud release. It ships `false`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PerTurnEffort {
    /// Inject `CLAUDE_CODE_MODEL_CAPABILITIES=<wire>=per_turn_effort` on a
    /// direct OpenRouter Anthropic-compat launch.
    pub enabled: bool,
}

impl Section for PerTurnEffort {
    const KEY: &'static str = "per_turn_effort";

    fn validate(&self) -> Result<(), String> {
        // Data-only and fully typed: the decode is the whole contract, and a
        // `true` here only re-enables a harness capability clud already
        // forwards. There is nothing semantic left to check.
        Ok(())
    }
}

/// The per-turn-effort flip for this process (#1528).
pub fn per_turn_effort() -> &'static PerTurnEffort {
    static SETTINGS: OnceLock<PerTurnEffort> = OnceLock::new();
    SETTINGS.get_or_init(|| super::snapshot().section())
}

/// Whether the direct-route per-turn-effort injection is enabled (#1528).
pub fn per_turn_effort_enabled() -> bool {
    per_turn_effort().enabled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_settings::built_in;

    #[test]
    fn future_codex_wire_id_uses_reviewed_context() {
        let choice = crate::codex_runtime::Choice {
            sol: "gpt-7-sol".to_string(),
            luna: "gpt-7-luna".to_string(),
            terra: "gpt-7-terra".to_string(),
            sol_source: crate::codex_runtime::Source::Published,
            luna_source: crate::codex_runtime::Source::Published,
            terra_source: crate::codex_runtime::Source::Published,
        };
        assert_eq!(
            dynamic_codex_context_window("gpt-7-sol", &choice),
            Some(1_050_000)
        );
        assert_eq!(dynamic_codex_context_window("gpt-7-sol-pro", &choice), None);
        assert_eq!(
            dynamic_codex_context_window("gpt-7-terra", &choice),
            Some(1_050_000)
        );
    }

    fn deepseek_section(default_model: &str, subagent_model: &str) -> Result<(), String> {
        DeepSeekSettings {
            default_model: default_model.to_string(),
            subagent_model: subagent_model.to_string(),
        }
        .validate()
    }

    /// The built-in copy is what ships offline, so it must agree with the
    /// catalog's reviewed default and the registry's compiled-in subagent.
    #[test]
    fn built_in_deepseek_names_match_the_catalog_default_and_registry_fallback() {
        let settings = built_in::<DeepSeekSettings>();
        let selection = provider_catalog::resolve(
            Some(ModelProvider::DeepSeek),
            Some(&settings.default_model),
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let catalog_default =
            provider_catalog::reviewed_default_model(ModelProvider::DeepSeek).unwrap();
        assert_eq!(selection.model.as_deref(), Some(catalog_default.cli_id));
        assert_eq!(
            selection.wire_model.as_deref(),
            Some(catalog_default.wire_id)
        );
        assert_eq!(
            settings.subagent_model,
            crate::provider_registry::descriptor_for(ModelProvider::DeepSeek)
                .unwrap()
                .subagent_wire_id
        );
    }

    #[test]
    fn served_model_names_are_deepseek_flash_and_apply_only_to_deepseek() {
        assert_eq!(
            provider_default_model(ModelProvider::DeepSeek),
            Some("deepseek-flash")
        );
        assert_eq!(
            provider_subagent_model(ModelProvider::DeepSeek),
            Some("deepseek-flash[1m]")
        );
        for provider in ModelProvider::ALL
            .iter()
            .copied()
            .filter(|provider| *provider != ModelProvider::DeepSeek)
        {
            assert_eq!(provider_default_model(provider), None, "{provider}");
            assert_eq!(provider_subagent_model(provider), None, "{provider}");
        }
    }

    #[test]
    fn deepseek_section_accepts_future_deepseek_names() {
        assert_eq!(
            deepseek_section("deepseek-flash-2[1m]", "deepseek-v5.0_lite"),
            Ok(())
        );
    }

    #[test]
    fn deepseek_section_rejects_anything_but_a_deepseek_model_name() {
        let too_long = format!("deepseek-{}", "a".repeat(MAX_MODEL_ID_LEN));
        for value in [
            "gpt-5.6-terra",
            "kimi-k3[1m]",
            "openrouter-claude-sonnet",
            "claude-opus",
            "https://example.invalid/deepseek-flash",
            "deepseek-flash@high",
            "deepseek flash",
            "deepseek-flash\n",
            "",
            "[1m]",
            too_long.as_str(),
        ] {
            assert!(
                deepseek_section(value, "deepseek-flash").is_err(),
                "default {value:?}"
            );
            assert!(
                deepseek_section("deepseek-flash", value).is_err(),
                "subagent {value:?}"
            );
        }
    }

    fn model_contexts_section(windows: serde_json::Value) -> Result<(), String> {
        serde_json::from_value::<ModelContexts>(windows)
            .map_err(|error| error.to_string())?
            .validate()
    }

    #[test]
    fn built_in_model_contexts_cover_the_live_inventory() {
        let windows = &built_in::<ModelContexts>().windows;
        assert!(!windows.is_empty());
        // Coverage, not values: the scheduled refresh (#1258) rewrites the
        // windows from OpenRouter's live datasheet, so pinning a number here
        // turns every upstream change into a red `main`.
        for id in ["xiaomi/mimo-v2.6-flash", "~anthropic/claude-sonnet-latest"] {
            assert!(windows.contains_key(id), "seed must cover {id}");
        }
        assert!(windows
            .values()
            .all(|tokens| (1_000..=10_000_000).contains(tokens)));
    }

    #[test]
    fn model_context_section_accepts_real_wire_ids_and_windows() {
        assert_eq!(
            model_contexts_section(serde_json::json!({
                "xiaomi/mimo-v2.6-flash": 1_048_576u32,
                "~anthropic/claude-sonnet-latest": 1_000_000u32,
                "inclusionai/ling-3.0-flash-vl:free": 262_144u32,
                "openai/gpt-3.5-turbo-0613": 4_095u32,
                "openrouter/auto-beta": 2_000_000u32,
                "deepseek/deepseek-chat:free": 131_072u32,
            })),
            Ok(())
        );
    }

    #[test]
    fn model_context_section_bounds_windows_and_rejects_non_model_ids() {
        // Bounds: the mirror of ci/refresh_model_contexts.py's constants.
        let valid = serde_json::json!({"xiaomi/mimo-v2.6-flash": 1_048_576u32});
        for tokens in [0u32, 999, 10_000_001] {
            let mut windows = valid.clone();
            windows["xiaomi/mimo-v2.6-flash"] = serde_json::json!(tokens);
            assert!(
                model_contexts_section(windows).is_err(),
                "window {tokens} must be rejected"
            );
        }
        // Keys that are not wire IDs.
        for bad_id in [
            "",
            "bad id",
            "bad!",
            "bracket[1m]",
            "x".repeat(129).as_str(),
        ] {
            let mut windows = serde_json::json!({});
            windows[bad_id] = serde_json::json!(200_000u32);
            assert!(
                model_contexts_section(windows).is_err(),
                "id {bad_id:?} must be rejected"
            );
        }
        // Wrong value types fail the typed decode before validate() runs.
        for bad_value in [
            serde_json::json!("200000"),
            serde_json::json!(1_048_576.5),
            serde_json::json!(-5),
        ] {
            let windows = serde_json::json!({"xiaomi/mimo-v2.6-flash": bad_value});
            assert!(model_contexts_section(windows).is_err());
        }
    }

    /// #1528: the injection ships dark. `true` here would turn on an
    /// unverified cache-preservation claim, so the built-in value must be
    /// flipped deliberately, not by accident.
    #[test]
    fn built_in_per_turn_effort_ships_disabled() {
        assert!(!built_in::<PerTurnEffort>().enabled);
    }

    #[test]
    fn per_turn_effort_section_is_a_plain_boolean_switch() {
        let section = serde_json::from_value::<PerTurnEffort>(serde_json::json!({"enabled": true}))
            .expect("a boolean switch decodes");
        assert!(section.enabled);
        assert_eq!(section.validate(), Ok(()));
        assert_eq!(PerTurnEffort::KEY, "per_turn_effort");
    }

    #[test]
    fn effective_context_window_prefers_the_catalog_then_the_served_map() {
        // Catalog wins: the Codex row carries a reviewed
        // `claude_max_context_tokens`, and the served map has no such key.
        assert_eq!(effective_context_window("gpt-6-sol"), Some(1_050_000));
        assert!(!model_contexts().windows.contains_key("gpt-6-sol"));
        // Served map: uncataloged OpenRouter IDs resolve from the datasheet.
        // Compared against the seed itself: the scheduled refresh rewrites it.
        let served = model_contexts()
            .windows
            .get("xiaomi/mimo-v2.6-flash")
            .copied()
            .expect("seed covers the reported model");
        assert_eq!(
            effective_context_window("xiaomi/mimo-v2.6-flash"),
            Some(served)
        );
        // Neither source: nothing, which leaves today's behavior unchanged.
        assert_eq!(
            effective_context_window("definitely/not-a-real-model"),
            None
        );
    }
}

//! Cached OpenRouter model pricing catalog (#1256).
//!
//! The scheduled producer publishes a data-only JSON document. This consumer
//! only requests the fixed GitHub raw-content origin, validates the complete
//! document before replacing its daemon-state cache, and keeps the embedded
//! catalog available when the network or document is unavailable.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

pub const CATALOG_URL: &str = "https://raw.githubusercontent.com/zackees/clud/main/crates/clud-bin/assets/openrouter-catalog.json";
const EMBEDDED_JSON: &str = include_str!("../assets/openrouter-catalog.json");
const CACHE_FILE: &str = "openrouter-catalog.json";
const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const CACHE_FRESHNESS: Duration = Duration::from_secs(6 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
const SCHEMA_VERSION: u32 = 1;
const TOP_LIMIT: usize = 20;

/// Token-mix assumptions used for the displayed weighted estimate.
pub const INPUT_WEIGHT: f64 = 0.70;
pub const OUTPUT_WEIGHT: f64 = 0.20;
pub const CACHED_INPUT_WEIGHT: f64 = 0.10;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Catalog {
    schema_version: u32,
    generated_at: String,
    source: String,
    models: Vec<Model>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub context_length: u32,
    pub input_price_per_token: Option<f64>,
    pub output_price_per_token: Option<f64>,
    /// OpenRouter's cached-input read rate, falling back to normal input rate
    /// when it does not publish a separate cached-input price.
    pub cached_input_price_per_token: Option<f64>,
    pub supports_tools: bool,
    pub supports_text_input: bool,
    pub supports_text_output: bool,
    pub supports_reasoning: bool,
    /// OpenRouter's `reasoning_effort` token in `supported_parameters`:
    /// whether this row accepts a per-request effort setting at all (#1528).
    /// `#[serde(default)]` keeps the field optional, so a clud build older
    /// than the artifact (and an artifact older than this field) still parses.
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// OpenRouter's own `reasoning` object, reduced to the four keys clud
    /// consumes (#1528). `None` for an artifact that predates the field.
    #[serde(default)]
    pub reasoning: Option<ReasoningDetail>,
    pub supports_vision: bool,
    pub eligible_for_coding: bool,
    pub ineligibility_reasons: Vec<String>,
}

/// OpenRouter's published reasoning vocabulary for one model (#1528), under
/// OpenRouter's own names. Every field is optional because OpenRouter omits
/// what it does not publish; the producer normalizes an absent object to four
/// explicit nulls so the artifact's shape is stable.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct ReasoningDetail {
    #[serde(default)]
    pub supported_efforts: Option<Vec<String>>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub default_enabled: Option<bool>,
    #[serde(default)]
    pub mandatory: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PricedModel {
    pub model: Model,
    /// Weighted estimated cost, USD per million equivalent tokens.
    pub weighted_usd_per_million_tokens: f64,
}

impl Model {
    pub fn weighted_usd_per_million_tokens(&self) -> f64 {
        let input = self.input_price_per_token.unwrap_or(0.0);
        let output = self.output_price_per_token.unwrap_or(0.0);
        let cached_input = self.cached_input_price_per_token.unwrap_or(input);
        (input * INPUT_WEIGHT + output * OUTPUT_WEIGHT + cached_input * CACHED_INPUT_WEIGHT)
            * 1_000_000.0
    }
}

impl Catalog {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(format!("catalog exceeds {MAX_CATALOG_BYTES} bytes"));
        }
        let value = crate::server_settings::parse_strict_json(bytes, MAX_CATALOG_BYTES)?;
        let mut catalog: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        if catalog.schema_version == 0 || catalog.schema_version > SCHEMA_VERSION {
            return Err(format!(
                "unsupported catalog schema_version {}",
                catalog.schema_version
            ));
        }
        if catalog.generated_at.is_empty()
            || catalog.source != "https://openrouter.ai/api/v1/models"
        {
            return Err("catalog has invalid timestamp or source attribution".to_string());
        }
        for model in &catalog.models {
            model.validate()?;
        }
        if catalog.models.is_empty() {
            return Err("catalog has no model rows".to_string());
        }
        catalog.models.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(catalog)
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    /// The row for an exact wire ID, or `None` (#1528).
    ///
    /// Deliberately exact, with no `[1m]` suffix stripping: the sibling
    /// `server_settings::effective_context_window` lookup is exact too, and a
    /// capability claim must name the row OpenRouter actually publishes rather
    /// than a same-family neighbor's.
    pub fn model_by_id(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Rank eligible rows using the fixed documented token mix. This is a
    /// lowest-priced eligible shortlist, not a claim that price measures code
    /// quality.
    pub fn cheapest_programming(&self) -> Vec<PricedModel> {
        let mut rows: Vec<_> = self
            .models
            .iter()
            .filter(|row| row.eligible_for_coding)
            .map(|model| PricedModel {
                model: model.clone(),
                weighted_usd_per_million_tokens: model.weighted_usd_per_million_tokens(),
            })
            .collect();
        rows.sort_by(|left, right| {
            left.weighted_usd_per_million_tokens
                .total_cmp(&right.weighted_usd_per_million_tokens)
                .then_with(|| left.model.id.cmp(&right.model.id))
        });
        rows.truncate(TOP_LIMIT);
        rows
    }
}

impl Model {
    fn validate(&self) -> Result<(), String> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-/:~".contains(&byte))
        {
            return Err(format!("invalid OpenRouter wire ID {:?}", self.id));
        }
        if self.name.is_empty()
            || self.provider.is_empty()
            || !(1_000..=10_000_000).contains(&self.context_length)
        {
            return Err(format!(
                "{} has invalid name, provider, or context length",
                self.id
            ));
        }
        for (field, price) in [
            ("input", self.input_price_per_token),
            ("output", self.output_price_per_token),
            ("cached input", self.cached_input_price_per_token),
        ] {
            if price.is_some_and(|price| !price.is_finite() || price < 0.0) {
                return Err(format!("{} has invalid {field} price", self.id));
            }
        }
        if self.eligible_for_coding
            && (!self.supports_tools
                || !self.supports_text_input
                || !self.supports_text_output
                || self.context_length < 16_000)
        {
            return Err(format!("{} has inconsistent coding eligibility", self.id));
        }
        if self.eligible_for_coding
            && (self.input_price_per_token.is_none() || self.output_price_per_token.is_none())
        {
            return Err(format!(
                "{} is eligible without known input and output prices",
                self.id
            ));
        }
        Ok(())
    }
}

/// User-facing `clud models cheapest` query. This is the production consumer
/// of the scheduled catalog; the model picker remains harness-owned (DD-054).
pub fn run_cheapest(json: bool) -> i32 {
    let rows = catalog().cheapest_programming();
    if json {
        match serde_json::to_writer(std::io::stdout().lock(), &rows) {
            Ok(()) => println!(),
            Err(error) => {
                eprintln!("error: failed to render OpenRouter catalog: {error}");
                return 1;
            }
        }
    } else {
        println!("Lowest-priced eligible OpenRouter models (not a quality ranking):");
        for row in rows {
            println!(
                "{}\t${:.6}/M weighted tokens\t{}",
                row.model.id, row.weighted_usd_per_million_tokens, row.model.name
            );
        }
    }
    0
}

/// Get the last-known-good catalog from the daemon state cache, refreshing it
/// synchronously with a bounded request when it is stale. The built-in copy
/// is always returned if no valid cache or response is available.
pub fn catalog() -> Catalog {
    let cache_path = crate::daemon::default_state_dir()
        .ok()
        .map(|dir| dir.join("cache").join(CACHE_FILE));
    catalog_at(cache_path.as_deref(), SystemTime::now(), fetch)
}

/// The catalog as a launch can see it, with **no network access** (#1528).
///
/// [`catalog`] refreshes a stale cache with a bounded fetch, which is right for
/// `clud models cheapest` and wrong for the direct-launch overlay: that route
/// is the one shape where clud is not in the request path, so it must not add
/// egress or a launch delay. This resolves the last-known-good daemon-state
/// cache when it parses and the always-present embedded copy otherwise.
pub fn catalog_cached_or_embedded() -> Catalog {
    let cache_path = crate::daemon::default_state_dir()
        .ok()
        .map(|dir| dir.join("cache").join(CACHE_FILE));
    catalog_at(cache_path.as_deref(), SystemTime::now(), || {
        Err("the launch path never refreshes the catalog".to_string())
    })
}

fn catalog_at(
    cache_path: Option<&Path>,
    now: SystemTime,
    fetch: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Catalog {
    let cached = cache_path.and_then(read_cache);
    if let (Some(path), Some(catalog)) = (cache_path, cached.as_ref()) {
        if is_fresh(path, now) {
            return catalog.clone();
        }
    }
    if let Ok(body) = fetch() {
        if let Ok(refreshed) = Catalog::parse(&body) {
            if let Some(path) = cache_path {
                write_cache(path, &body);
            }
            return refreshed;
        }
    }
    cached.unwrap_or_else(built_in)
}

fn built_in() -> Catalog {
    Catalog::parse(EMBEDDED_JSON.as_bytes()).expect("embedded OpenRouter catalog is valid")
}

fn fetch() -> Result<Vec<u8>, String> {
    let response = ureq::AgentBuilder::new()
        .timeout(FETCH_TIMEOUT)
        .redirects(0)
        .build()
        .get(CATALOG_URL)
        .call()
        .map_err(|error| error.to_string())?;
    if response.get_url() != CATALOG_URL {
        return Err("catalog response did not come from the allowlisted origin".to_string());
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_CATALOG_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| error.to_string())?;
    Ok(body)
}

fn read_cache(path: &Path) -> Option<Catalog> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_CATALOG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    Catalog::parse(&bytes).ok()
}

fn is_fresh(path: &Path, now: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age < CACHE_FRESHNESS)
}

fn write_cache(path: &Path, bytes: &[u8]) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let staging = path.with_extension("json.partial");
    if std::fs::write(&staging, bytes)
        .and_then(|()| std::fs::rename(&staging, path))
        .is_err()
    {
        let _ = std::fs::remove_file(staging);
    }
}

/// Cached location, exposed within the crate to keep the path contract easy
/// to test without touching a developer's real daemon state.
#[allow(dead_code)]
fn cache_path(state_dir: &Path) -> PathBuf {
    state_dir.join("cache").join(CACHE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture() -> Vec<u8> {
        include_bytes!("../../../tests/fixtures/openrouter_catalog_sample.json").to_vec()
    }

    #[test]
    fn weighted_prices_can_change_the_input_only_ranking() {
        let catalog = Catalog::parse(&fixture()).unwrap();
        let ranked = catalog.cheapest_programming();
        assert_eq!(ranked[0].model.id, "provider/weighted-cheap");
        assert_eq!(ranked[1].model.id, "provider/input-cheap");
        let input_only = catalog
            .models
            .iter()
            .filter(|row| row.eligible_for_coding)
            .min_by(|a, b| {
                a.input_price_per_token
                    .unwrap()
                    .total_cmp(&b.input_price_per_token.unwrap())
            })
            .unwrap();
        assert_eq!(input_only.id, "provider/input-cheap");
    }

    #[test]
    fn success_updates_cache_and_missing_or_malformed_fetch_keeps_stale_cache() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cache = cache_path(temp.path());
        let body = fixture();
        let loaded = catalog_at(Some(&cache), SystemTime::now(), || Ok(body.clone()));
        assert_eq!(loaded.models.len(), 3);
        assert!(cache.exists());

        let old_time = SystemTime::now() - CACHE_FRESHNESS - Duration::from_secs(1);
        filetime::set_file_mtime(&cache, filetime::FileTime::from_system_time(old_time)).unwrap();
        let stale = catalog_at(Some(&cache), SystemTime::now(), || Err("offline".into()));
        assert_eq!(stale, loaded);
        filetime::set_file_mtime(&cache, filetime::FileTime::from_system_time(old_time)).unwrap();
        let malformed = catalog_at(
            Some(&cache),
            SystemTime::now(),
            || Ok(b"{not json".to_vec()),
        );
        assert_eq!(malformed, loaded);
    }

    #[test]
    fn malformed_required_data_is_rejected_but_newer_additive_fields_are_ignored() {
        let mut json: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        json["future_field"] = serde_json::json!({"new": true});
        json["models"][0]["future_model_field"] = serde_json::json!("accepted");
        assert!(Catalog::parse(json.to_string().as_bytes()).is_ok());
        let mut missing_timestamp: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        missing_timestamp["generated_at"] = serde_json::Value::Null;
        assert!(Catalog::parse(missing_timestamp.to_string().as_bytes()).is_err());

        let mut missing_id: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        missing_id["models"][0]["id"] = serde_json::Value::Null;
        assert!(Catalog::parse(missing_id.to_string().as_bytes()).is_err());

        let mut bad_price: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        bad_price["models"][0]["input_price_per_token"] = serde_json::json!("bad");
        assert!(Catalog::parse(bad_price.to_string().as_bytes()).is_err());

        let mut unpriced_eligible: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        unpriced_eligible["models"][0]["input_price_per_token"] = serde_json::Value::Null;
        assert!(Catalog::parse(unpriced_eligible.to_string().as_bytes()).is_err());

        let mut non_text_eligible: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        non_text_eligible["models"][0]["supports_text_output"] = serde_json::json!(false);
        assert!(Catalog::parse(non_text_eligible.to_string().as_bytes()).is_err());

        let duplicate_key = String::from_utf8(fixture()).unwrap().replacen(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
            1,
        );
        assert!(Catalog::parse(duplicate_key.as_bytes()).is_err());
        assert!(Catalog::parse(
            br#"{"schema_version":1,"source":"https://openrouter.ai/api/v1/models","models":[]}"#
        )
        .is_err());
    }

    #[test]
    fn unavailable_catalog_uses_the_baked_in_fallback() {
        let fallback = catalog_at(None, SystemTime::now(), || Err("offline".into()));
        assert_eq!(fallback, built_in());
        assert!(!fallback.cheapest_programming().is_empty());
    }

    /// #1528: the artifact's published reasoning vocabulary decodes, and a row
    /// that omits it entirely keeps today's shape through `#[serde(default)]`.
    #[test]
    fn reasoning_effort_fields_decode_and_absent_fields_default() {
        let catalog = Catalog::parse(&fixture()).unwrap();

        let published = catalog.model_by_id("provider/weighted-cheap").unwrap();
        assert!(published.supports_reasoning_effort);
        assert_eq!(
            published.reasoning,
            Some(ReasoningDetail {
                supported_efforts: Some(vec![
                    "max".to_string(),
                    "high".to_string(),
                    "low".to_string()
                ]),
                default_effort: Some("high".to_string()),
                default_enabled: Some(true),
                mandatory: Some(false),
            })
        );

        // The producer's all-null shape for a row OpenRouter describes only
        // partially still decodes, and its gate stays off.
        let normalized_empty = catalog.model_by_id("provider/not-eligible").unwrap();
        assert!(!normalized_empty.supports_reasoning_effort);
        assert_eq!(normalized_empty.reasoning, Some(ReasoningDetail::default()));

        // A row that predates the field at all: the artifact clud was built
        // with, and any older published document, still parses.
        let omitted = catalog.model_by_id("provider/input-cheap").unwrap();
        assert!(!omitted.supports_reasoning_effort);
        assert_eq!(omitted.reasoning, None);

        assert_eq!(catalog.model_by_id("provider/does-not-exist"), None);
    }

    /// #1528: a whole document written before the field existed must parse.
    #[test]
    fn an_artifact_without_the_reasoning_fields_still_parses() {
        let mut json: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        for row in json["models"].as_array_mut().unwrap() {
            let row = row.as_object_mut().unwrap();
            row.remove("supports_reasoning_effort");
            row.remove("reasoning");
        }
        let catalog = Catalog::parse(json.to_string().as_bytes()).unwrap();
        assert_eq!(catalog.models().len(), 3);
        assert!(catalog
            .model_by_id("provider/weighted-cheap")
            .unwrap()
            .reasoning
            .is_none());
    }

    /// #1528: the launch path must never reach the network. A fresh cache
    /// short-circuits before the refresh closure runs, and the closure
    /// `catalog_cached_or_embedded` supplies can only ever return an error, so
    /// a stale or absent cache still resolves without a request.
    #[test]
    fn launch_catalog_never_refreshes_a_stale_cache() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cache = cache_path(temp.path());
        let loaded = catalog_at(Some(&cache), SystemTime::now(), || Ok(fixture()));
        assert_eq!(loaded.models().len(), 3);

        // A fresh cache never calls the refresh closure: it panics rather than
        // returning an error, so a request here would fail the test outright.
        let fresh = catalog_at(Some(&cache), SystemTime::now(), || {
            panic!("the launch path must not fetch")
        });
        assert_eq!(fresh, loaded);

        // Stale or absent: the launch accessor's closure is an error, so the
        // last-known-good cache (and then the embedded copy) still answers.
        let old = SystemTime::now() - CACHE_FRESHNESS - Duration::from_secs(1);
        filetime::set_file_mtime(&cache, filetime::FileTime::from_system_time(old)).unwrap();
        let stale = catalog_at(Some(&cache), SystemTime::now(), || {
            Err("the launch path never refreshes the catalog".to_string())
        });
        assert_eq!(stale, loaded);
        let absent = catalog_at(None, SystemTime::now(), || {
            Err("the launch path never refreshes the catalog".to_string())
        });
        assert_eq!(absent, built_in());
        assert!(!absent.models().is_empty());
    }
}

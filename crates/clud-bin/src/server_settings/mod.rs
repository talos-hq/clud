//! Server-side settings (#1192, DD-072).
//!
//! Values clud must be able to change without a release live in
//! `assets/server-settings.json`. The build embeds that file as the built-in
//! copy, and installed builds fetch the same path from `main`. A document is
//! split into independently validated sections, so one malformed section never
//! affects another. Each section resolves to the first valid value among the
//! copy served now, the last cached valid copy, and the built-in copy.
//! See [`docs/architecture/server-settings.md`](../../../../docs/architecture/server-settings.md).
//!
//! Adding a setting takes a [`Section`] type, one entry in `sections::SECTIONS`,
//! and its built-in value in the embedded JSON; the guard tests fail until all
//! three agree.

mod json;
mod sections;
mod store;

use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

pub use sections::{
    deepseek, effective_context_window, effective_context_window_with_source, model_contexts,
    per_turn_effort, per_turn_effort_enabled, provider_default_model, provider_subagent_model,
    ContextWindowSource, DeepSeekSettings, ModelContexts, PerTurnEffort,
};

/// The built-in copy, and the single source of the file served from `main`.
pub const EMBEDDED_JSON: &str = include_str!("../../assets/server-settings.json");

/// Where installed builds read the live copy. Pinned to `main` on purpose: the
/// file is a server-side switch, not a release artifact.
pub const SERVER_URL: &str =
    "https://raw.githubusercontent.com/zackees/clud/main/crates/clud-bin/assets/server-settings.json";

/// `0`, `false`, `off` or `no` uses the built-in copy only: no cache, no
/// network.
pub const DISABLE_ENV: &str = "CLUD_SERVER_SETTINGS";

/// Replaces [`SERVER_URL`] to try a draft or use a mirror. It bypasses the
/// cache, so a draft never overwrites the real file's last good copy.
pub const URL_ENV: &str = "CLUD_SERVER_SETTINGS_URL";

/// Any value except empty or `0` prints refresh and fallback diagnostics to
/// stderr, including why a section was rejected.
pub const VERBOSE_ENV: &str = "CLUD_VERBOSE_SERVER_SETTINGS";

/// Highest `schema_version` this build understands. Additive changes (a new
/// section, a new optional field) never bump it; a bump tells older builds to
/// ignore the document and keep their last good copy.
pub const SCHEMA_VERSION: u64 = 1;

const CACHE_RELATIVE_PATH: &str = ".clud/cache/server-settings/server-settings.json";

/// Strict JSON parser shared with other bounded remote-data consumers.
pub(crate) fn parse_strict_json(bytes: &[u8], max_bytes: usize) -> Result<Value, String> {
    json::parse_strict_with_limit(bytes, max_bytes)
}

/// One independently validated block of server settings, stored under
/// `sections.<KEY>`. Unknown fields are ignored so a later build can add
/// optional ones without breaking this one.
pub trait Section: DeserializeOwned + 'static {
    const KEY: &'static str;

    /// Semantic checks beyond the typed decode. A failure rejects this section
    /// only; every other section is unaffected.
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Type-erased registry entry, so a merge can validate every known section
/// without holding its type.
#[derive(Clone, Copy)]
pub struct SectionSpec {
    pub key: &'static str,
    checker: fn(&Value) -> Result<(), String>,
}

impl SectionSpec {
    pub const fn of<T: Section>() -> Self {
        Self {
            key: T::KEY,
            checker: check_section::<T>,
        }
    }

    fn check(&self, value: &Value) -> Result<(), String> {
        (self.checker)(value)
    }
}

fn decode_section<T: Section>(value: &Value) -> Result<T, String> {
    let section = T::deserialize(value).map_err(|error| error.to_string())?;
    section.validate()?;
    Ok(section)
}

fn check_section<T: Section>(value: &Value) -> Result<(), String> {
    decode_section::<T>(value).map(drop)
}

/// A strictly parsed document whose `schema_version` this build supports.
/// Sections are still unvalidated JSON here.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Document {
    pub sections: Map<String, Value>,
}

impl Document {
    /// Whole-document checks. Any failure here discards the entire document,
    /// which keeps the previous cache untouched.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, String> {
        let Value::Object(mut root) = json::parse_strict(bytes)? else {
            return Err("the document root is not an object".to_string());
        };
        let version = root
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or("schema_version is missing or not a non-negative integer")?;
        if version == 0 || version > SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {version}; this build supports 1..={SCHEMA_VERSION}"
            ));
        }
        match root.remove("sections") {
            Some(Value::Object(sections)) => Ok(Self { sections }),
            _ => Err("sections is missing or not an object".to_string()),
        }
    }

    pub(crate) fn to_json(&self) -> String {
        let document = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "sections": self.sections,
        });
        serde_json::to_string_pretty(&document).expect("a JSON value always serializes")
    }
}

/// Per-section last-known-good merge of a served document over the previous
/// cache. Returns the document to cache and each rejected section with its
/// reason.
///
/// - A valid served section replaces the cached one.
/// - An invalid served section keeps the previous valid value, or is dropped
///   so the built-in copy applies.
/// - An absent or `null` served section is dropped: removing a section on the
///   server is how an override is reset to the built-in value.
/// - A section this build does not know is kept verbatim, so a newer clud
///   sharing the cache still receives it.
pub(crate) fn merge(
    served: &Document,
    previous: Option<&Document>,
    specs: &[SectionSpec],
) -> (Document, Vec<(String, String)>) {
    let mut sections = Map::new();
    let mut rejected = Vec::new();
    for (key, value) in &served.sections {
        if value.is_null() {
            continue;
        }
        let Some(spec) = specs.iter().find(|spec| spec.key == key) else {
            sections.insert(key.clone(), value.clone());
            continue;
        };
        match spec.check(value) {
            Ok(()) => {
                sections.insert(key.clone(), value.clone());
            }
            Err(reason) => {
                if let Some(good) = previous
                    .and_then(|document| document.sections.get(key))
                    .filter(|good| spec.check(good).is_ok())
                {
                    sections.insert(key.clone(), good.clone());
                }
                rejected.push((key.clone(), reason));
            }
        }
    }
    (Document { sections }, rejected)
}

/// Where a process's settings document came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    BuiltIn,
    Cached,
    Served,
}

/// The settings one process resolved. Loaded once, so every consumer in a
/// process sees the same values even if a refresh lands later.
#[derive(Debug, Clone)]
pub struct Snapshot {
    document: Option<Document>,
    origin: Origin,
}

impl Snapshot {
    pub(crate) fn built_in_only() -> Self {
        Self {
            document: None,
            origin: Origin::BuiltIn,
        }
    }

    pub(crate) fn from_document(document: Document, origin: Origin) -> Self {
        Self {
            document: Some(document),
            origin,
        }
    }

    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// The section's cached or served value when it validates, otherwise the
    /// built-in value.
    pub fn section<T: Section>(&self) -> T {
        if let Some(value) = self
            .document
            .as_ref()
            .and_then(|document| document.sections.get(T::KEY))
        {
            match decode_section::<T>(value) {
                Ok(section) => return section,
                Err(reason) => trace(format_args!(
                    "{:?} section `{}` is invalid ({reason}); using the built-in copy",
                    self.origin,
                    T::KEY
                )),
            }
        }
        built_in()
    }
}

/// The built-in value of a registered section. The guard tests prove every
/// registered section has a valid built-in value, so this cannot fail in a
/// shipped build.
pub fn built_in<T: Section>() -> T {
    let document = Document::parse(EMBEDDED_JSON.as_bytes())
        .expect("assets/server-settings.json is validated by the server_settings tests");
    let value = document
        .sections
        .get(T::KEY)
        .unwrap_or_else(|| panic!("assets/server-settings.json has no `{}` section", T::KEY));
    decode_section(value)
        .unwrap_or_else(|reason| panic!("built-in `{}` section is invalid: {reason}", T::KEY))
}

/// The settings for this process, resolved on first use.
pub fn snapshot() -> &'static Snapshot {
    static SNAPSHOT: OnceLock<Snapshot> = OnceLock::new();
    SNAPSHOT.get_or_init(load_for_process)
}

fn load_for_process() -> Snapshot {
    // Library unit tests assert on built-in values, so they never read a
    // developer's cache or the network.
    if cfg!(test) || is_disabled(std::env::var(DISABLE_ENV).ok().as_deref()) {
        return Snapshot::built_in_only();
    }
    let url_override = std::env::var(URL_ENV)
        .ok()
        .filter(|url| !url.trim().is_empty());
    let cache_path = match url_override {
        Some(_) => None,
        None => crate::home::user_home().map(|home| home.join(CACHE_RELATIVE_PATH)),
    };
    let url = url_override.unwrap_or_else(|| SERVER_URL.to_string());
    let loaded = store::load(store::Sources {
        cache_path,
        now: SystemTime::now(),
        fetch: Arc::new(move || store::fetch_url(&url)),
        wait: store::REFRESH_WAIT,
    });
    trace(format_args!("using the {:?} copy", loaded.snapshot.origin));
    // Detach the refresh: it finishes writing the cache for the next launch
    // even if this one moves on or exits first.
    drop(loaded.refresh);
    loaded.snapshot
}

fn is_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

pub(crate) fn trace(message: std::fmt::Arguments<'_>) {
    if std::env::var_os(VERBOSE_ENV).is_some_and(|value| !value.is_empty() && value != "0") {
        crate::loop_console::diagnostic!("[clud] server settings: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Alpha {
        count: u32,
    }

    impl Section for Alpha {
        const KEY: &'static str = "alpha";

        fn validate(&self) -> Result<(), String> {
            (self.count > 0)
                .then_some(())
                .ok_or_else(|| "count must be positive".to_string())
        }
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Beta {
        name: String,
    }

    impl Section for Beta {
        const KEY: &'static str = "beta";
    }

    const SPECS: &[SectionSpec] = &[SectionSpec::of::<Alpha>(), SectionSpec::of::<Beta>()];

    fn document(sections: Value) -> Document {
        let Value::Object(sections) = sections else {
            panic!("sections must be an object");
        };
        Document { sections }
    }

    #[test]
    fn document_parse_requires_a_supported_schema_and_a_sections_object() {
        let ok = Document::parse(
            br#"{"schema_version":1,"sections":{"alpha":{"count":1}},"note":"ignored"}"#,
        )
        .unwrap();
        assert_eq!(ok.sections.get("alpha"), Some(&json!({"count": 1})));

        for body in [
            r#"[]"#,
            r#""text""#,
            r#"{"sections":{}}"#,
            r#"{"schema_version":"1","sections":{}}"#,
            r#"{"schema_version":-1,"sections":{}}"#,
            r#"{"schema_version":1.5,"sections":{}}"#,
            r#"{"schema_version":0,"sections":{}}"#,
            r#"{"schema_version":2,"sections":{}}"#,
            r#"{"schema_version":1}"#,
            r#"{"schema_version":1,"sections":[]}"#,
            r#"{"schema_version":1,"sections":{"alpha":{"count":1},"alpha":{"count":2}}}"#,
        ] {
            assert!(Document::parse(body.as_bytes()).is_err(), "accepted {body}");
        }
    }

    #[test]
    fn a_valid_served_section_replaces_the_cached_one() {
        let previous = document(json!({"alpha": {"count": 1}, "beta": {"name": "old"}}));
        let served = document(json!({"alpha": {"count": 2}, "beta": {"name": "new"}}));
        let (merged, rejected) = merge(&served, Some(&previous), SPECS);
        assert_eq!(merged, served);
        assert!(rejected.is_empty());
    }

    #[test]
    fn an_invalid_section_keeps_its_last_good_value_without_touching_the_others() {
        let previous = document(json!({"alpha": {"count": 1}, "beta": {"name": "old"}}));
        let served = document(json!({"alpha": {"count": 0}, "beta": {"name": "new"}}));
        let (merged, rejected) = merge(&served, Some(&previous), SPECS);
        assert_eq!(
            merged,
            document(json!({"alpha": {"count": 1}, "beta": {"name": "new"}}))
        );
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].0, "alpha");
        assert!(rejected[0].1.contains("positive"));

        // A wrong type is rejected the same way as a failed semantic check.
        let served = document(json!({"alpha": {"count": "three"}, "beta": {"name": "new"}}));
        let (merged, _) = merge(&served, Some(&previous), SPECS);
        assert_eq!(merged.sections.get("alpha"), Some(&json!({"count": 1})));
    }

    #[test]
    fn an_invalid_section_without_a_good_previous_value_falls_back_to_built_in() {
        let served = document(json!({"alpha": {"count": 0}}));
        let (merged, rejected) = merge(&served, None, SPECS);
        assert!(merged.sections.is_empty());
        assert_eq!(rejected.len(), 1);

        let corrupt_previous = document(json!({"alpha": {"count": 0}}));
        let (merged, _) = merge(&served, Some(&corrupt_previous), SPECS);
        assert!(merged.sections.is_empty(), "an invalid cache is never kept");
    }

    #[test]
    fn an_absent_or_null_section_resets_to_built_in() {
        let previous = document(json!({"alpha": {"count": 1}, "beta": {"name": "old"}}));
        let served = document(json!({"beta": null}));
        let (merged, rejected) = merge(&served, Some(&previous), SPECS);
        assert!(merged.sections.is_empty());
        assert!(rejected.is_empty());
    }

    #[test]
    fn unknown_sections_are_preserved_for_newer_builds() {
        let served = document(json!({"gamma": {"anything": [1, 2]}, "beta": {"name": "x"}}));
        let (merged, rejected) = merge(&served, None, SPECS);
        assert_eq!(merged, served);
        assert!(rejected.is_empty());
    }

    #[test]
    fn a_cached_document_round_trips_through_its_own_serialization() {
        let original = document(json!({"alpha": {"count": 3}, "gamma": [true]}));
        assert_eq!(
            Document::parse(original.to_json().as_bytes()).unwrap(),
            original
        );
    }

    #[test]
    fn snapshot_sections_fall_back_to_built_in_one_at_a_time() {
        let snapshot = Snapshot::from_document(
            document(
                json!({"deepseek": {"default_model": "gpt-5.6-terra", "subagent_model": "deepseek-flash"}}),
            ),
            Origin::Cached,
        );
        assert_eq!(snapshot.section::<DeepSeekSettings>(), built_in());

        let snapshot = Snapshot::from_document(
            document(
                json!({"deepseek": {"default_model": "deepseek-flash-2[1m]", "subagent_model": "deepseek-flash-2", "later_field": 1}}),
            ),
            Origin::Served,
        );
        assert_eq!(
            snapshot.section::<DeepSeekSettings>().default_model,
            "deepseek-flash-2[1m]"
        );
        assert_eq!(snapshot.origin(), Origin::Served);

        assert_eq!(
            Snapshot::built_in_only().section::<DeepSeekSettings>(),
            built_in()
        );
    }

    /// The embedded file is both the offline fallback and the copy served from
    /// `main`, so a broken edit must fail here before it can reach an install.
    #[test]
    fn the_embedded_document_is_strict_complete_and_valid() {
        let document = Document::parse(EMBEDDED_JSON.as_bytes()).unwrap();
        for spec in sections::SECTIONS {
            let value = document
                .sections
                .get(spec.key)
                .unwrap_or_else(|| panic!("no built-in `{}` section", spec.key));
            spec.check(value)
                .unwrap_or_else(|reason| panic!("built-in `{}` is invalid: {reason}", spec.key));
        }
        for key in document.sections.keys() {
            assert!(
                sections::SECTIONS.iter().any(|spec| spec.key == key),
                "built-in section `{key}` is not registered in sections::SECTIONS"
            );
        }
    }

    #[test]
    fn registered_section_keys_are_unique() {
        for (index, spec) in sections::SECTIONS.iter().enumerate() {
            assert!(
                sections::SECTIONS[index + 1..]
                    .iter()
                    .all(|other| other.key != spec.key),
                "section `{}` is registered twice",
                spec.key
            );
        }
    }

    /// Existence is proven at compile time by `include_str!`; a runtime
    /// source-tree check would fail in CI, where tests run from a prebuilt
    /// bundle with no checkout.
    #[test]
    fn the_served_url_names_the_embedded_asset_on_main() {
        assert!(SERVER_URL.ends_with("/main/crates/clud-bin/assets/server-settings.json"));
    }

    #[test]
    fn process_snapshot_under_test_is_built_in() {
        assert_eq!(snapshot().origin(), Origin::BuiltIn);
    }

    #[test]
    fn only_explicit_off_values_disable_server_settings() {
        for value in ["0", "false", "OFF", " no "] {
            assert!(is_disabled(Some(value)), "{value}");
        }
        for value in ["1", "true", "", "yes"] {
            assert!(!is_disabled(Some(value)), "{value}");
        }
        assert!(!is_disabled(None));
    }
}

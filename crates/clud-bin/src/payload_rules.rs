//! JSON-path payload rules for a gateway request body.
//!
//! Ports CLIProxyAPI's `requests.payload` (`config.example.yaml:266-334`,
//! applied by its request pipeline): an operator can rewrite the upstream
//! request body per model. Three rule kinds, applied in this order:
//!
//! * `default` — set a parameter only when the caller left it out;
//! * `override` — always set it, replacing whatever the caller sent;
//! * `filter` — remove the parameter.
//!
//! A rule selects on a model name (`*` wildcards) and optionally on the
//! protocol clud is serving that request on. Parameters are addressed with
//! dotted JSON paths (`thinking.budget_tokens`, `messages.0.content`).
//!
//! clud had no such seam: a gateway launch forwarded the harness bytes
//! unchanged, so A/B probing of one parameter meant editing the harness. Rules
//! are read from the JSON file named by [`RULES_ENV`], re-read when the file
//! changes, and applied to `POST /v1/messages` before a route is chosen.
//!
//! Paths are the grammar in [`crate::payload_query`]: dotted keys, numeric
//! array indices, and gjson query selectors (`tools.#(name=="apply_patch")`,
//! `...#.tools.#(name=="functions")#`).
//!
//! A selector can also carry the applicability conditions upstream evaluates
//! before a rule fires (`config.example.yaml:276-287`): request `headers`
//! (value patterns with `*`), `match`/`not-match` (payload JSON paths that must
//! or must not equal a value), and `exist`/`not-exist` (paths that must or must
//! not resolve to a non-null value). All of them must hold.
//!
//! `default-raw` / `override-raw` are the same two kinds whose parameter
//! values are JSON *text* (`"{\"type\":\"object\"}"`) parsed into a value when
//! the file loads, so an operator can paste a document the rules file has no
//! shape for. They run after their typed counterpart, as upstream does
//! (`payload_helpers.go`: `Default` then `DefaultRaw`, `Override` then
//! `OverrideRaw`).
//!
//! A selector can also be narrowed to the protocol the *client* spoke
//! (`from-protocol`), which is how one rules file distinguishes a Claude-shaped
//! `POST /v1/messages` caller from an OpenAI-shaped `POST /v1/chat/completions`
//! one that clud itself translates (`payload_helpers.go`: `protocol` is the
//! executor's target protocol, `fromProtocol` the source format).
//!
//! Rules come from one of two places. An explicit file named by
//! [`RULES_ENV`] wins, which is what an A/B experiment or a test uses. With the
//! variable unset the document lives in the user-level settings file under
//! [`SETTINGS_KEY`], mirroring upstream, where `requests.payload` sits beside
//! its other request knobs. Only `~/.clud/settings.json` is read — never a
//! repo's `.clud/settings.json`, for the same reason DD-137 gives for
//! `safe_rm`: an agent can edit files in its checkout, so a repo layer would
//! let it rewrite its own requests.
//!
//! A rule that cannot be applied never fails a client request: the payload is
//! forwarded as far as the good rules got it and the problem is reported to
//! the bridge log by [`apply`]'s [`Outcome::problems`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::payload_query::{self, Step};

/// Points at the JSON rules file. Unset means "no rules".
pub const RULES_ENV: &str = "CLUD_PAYLOAD_RULES";

/// Dotted key inside `~/.clud/settings.json` that carries the same document
/// when [`RULES_ENV`] is unset. Named after upstream's own config section
/// (`config.example.yaml` `requests.payload`), so an operator's rules file
/// can be pasted under this key unchanged.
pub const SETTINGS_KEY: &str = "requests.payload";

/// A rules file larger than this is refused rather than parsed: the file is a
/// hand-edited switch, not a data feed.
pub const MAX_RULES_BYTES: u64 = 256 * 1024;

/// One `default`/`override` rule: which models it selects and what it sets.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SetRule {
    /// Empty selects every model.
    #[serde(default)]
    pub models: Vec<ModelMatch>,
    /// Dotted JSON path -> the value to set, verbatim.
    #[serde(default)]
    pub params: Map<String, Value>,
}

/// One `default-raw`/`override-raw` rule: the same selection as [`SetRule`],
/// but every parameter value is JSON text parsed into a value when the file
/// loads (`sjson.SetRawBytes` upstream, `payload_helpers.go`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawSetRule {
    /// Empty selects every model.
    #[serde(default)]
    pub models: Vec<ModelMatch>,
    /// Dotted JSON path -> JSON text. Invalid JSON is refused at load time, so
    /// a typo cannot silently forward a string where an object was meant.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

/// One `filter` rule: which models it selects and which paths it removes.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterRule {
    /// Empty selects every model.
    #[serde(default)]
    pub models: Vec<ModelMatch>,
    /// Dotted JSON paths to remove.
    #[serde(default)]
    pub params: Vec<String>,
}

/// A model selector: a name with `*` wildcards, optionally narrowed to one
/// protocol, plus the applicability conditions upstream evaluates before a rule
/// fires (`headers`, `match`, `not-match`, `exist`, `not-exist`).
///
/// All conditions must hold for the selector to select — upstream's "all
/// configured request headers must match" / "all payload JSON paths must
/// equal", `config.example.yaml:276-287`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelMatch {
    pub name: String,
    /// Matched against the protocol clud serves the request on (`claude` for
    /// `POST /v1/messages`). Absent matches every protocol.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Matched against the protocol the *client* spoke: `claude` for a
    /// `POST /v1/messages` caller, `openai` for `POST /v1/chat/completions`
    /// (which clud translates before forwarding). Absent matches every source.
    #[serde(default, rename = "from-protocol")]
    pub from_protocol: Option<String>,
    /// Request header name -> value pattern with `*` wildcards. A header that
    /// is missing from the request cannot match.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Payload JSON path -> value; every path must resolve to that value.
    #[serde(default, rename = "match")]
    pub matches: Vec<BTreeMap<String, Value>>,
    /// Payload JSON path -> value; no path may resolve to that value. A path
    /// that is absent satisfies `not-match`.
    #[serde(default, rename = "not-match")]
    pub not_matches: Vec<BTreeMap<String, Value>>,
    /// Paths that must resolve to a non-null value.
    #[serde(default)]
    pub exist: Vec<String>,
    /// Paths that must be missing or null.
    #[serde(default, rename = "not-exist")]
    pub not_exist: Vec<String>,
}

impl ModelMatch {
    /// True when this selector names `request` and every condition holds.
    ///
    /// `Err` means a condition path is malformed; the caller reports it and
    /// skips the rule, because a bad condition must never fire a rewrite.
    fn accepts(&self, request: &Request<'_>, payload: &Value) -> Result<bool, String> {
        let protocol_fits = self
            .protocol
            .as_deref()
            .is_none_or(|wanted| wanted.eq_ignore_ascii_case(request.protocol));
        if !(protocol_fits
            && from_protocol_matches(self.from_protocol.as_deref(), request.from_protocol)
            && wildcard_matches(&self.name, request.model))
        {
            return Ok(false);
        }
        for (name, pattern) in &self.headers {
            let Some(value) = header_value(request.headers, name) else {
                return Ok(false);
            };
            if !wildcard_matches(pattern, value.trim()) {
                return Ok(false);
            }
        }
        for group in &self.matches {
            for (path, expected) in group {
                let steps = segments(path)?;
                if payload_query::resolve(payload, &steps) != Some(expected) {
                    return Ok(false);
                }
            }
        }
        for group in &self.not_matches {
            for (path, expected) in group {
                let steps = segments(path)?;
                if payload_query::resolve(payload, &steps) == Some(expected) {
                    return Ok(false);
                }
            }
        }
        for path in &self.exist {
            if !path_present(payload, path) {
                return Ok(false);
            }
        }
        for path in &self.not_exist {
            if path_present(payload, path) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Upstream's `payloadFromProtocolMatches` + `normalizePayloadFromProtocol`
/// (`payload_helpers.go:436-455`): absent matches every source, and a source
/// clud does not know cannot satisfy a pinned pattern.
fn from_protocol_matches(pattern: Option<&str>, from_protocol: &str) -> bool {
    let Some(pattern) = pattern.map(normalize_from_protocol) else {
        return true;
    };
    if pattern.is_empty() {
        return true;
    }
    let from_protocol = normalize_from_protocol(from_protocol);
    !from_protocol.is_empty() && pattern.eq_ignore_ascii_case(&from_protocol)
}

/// Upstream spells the OpenAI Responses source three ways; they are one source.
fn normalize_from_protocol(protocol: &str) -> String {
    let protocol = protocol.trim().to_ascii_lowercase();
    match protocol.as_str() {
        "openai-response" | "openai-responses" | "response" => "responses".to_string(),
        _ => protocol,
    }
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// What a rule selects against: the model, the protocol being served, the
/// protocol the client spoke, and the caller's own headers (for `headers`
/// conditions).
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub model: &'a str,
    pub protocol: &'a str,
    /// `""` means "unknown source": every selector that pins `from-protocol`
    /// then declines to fire (upstream's behaviour for an empty source).
    pub from_protocol: &'a str,
    pub headers: &'a [(String, String)],
}

impl<'a> Request<'a> {
    pub fn new(model: &'a str, protocol: &'a str) -> Self {
        Self {
            model,
            protocol,
            from_protocol: "",
            headers: &[],
        }
    }

    pub fn with_headers(
        model: &'a str,
        protocol: &'a str,
        headers: &'a [(String, String)],
    ) -> Self {
        Self {
            model,
            protocol,
            from_protocol: "",
            headers,
        }
    }

    /// A request whose client-facing protocol is known, so `from-protocol`
    /// selectors can tell an OpenAI-shaped caller from a Claude-shaped one.
    pub fn with_source(
        model: &'a str,
        protocol: &'a str,
        from_protocol: &'a str,
        headers: &'a [(String, String)],
    ) -> Self {
        Self {
            model,
            protocol,
            from_protocol,
            headers,
        }
    }
}

/// The whole rules document. Every kind is optional, so `{}` is a valid file
/// that changes nothing.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PayloadRules {
    #[serde(default)]
    pub default: Vec<SetRule>,
    #[serde(default, rename = "default-raw")]
    pub default_raw: Vec<RawSetRule>,
    #[serde(default, rename = "override")]
    pub override_rules: Vec<SetRule>,
    #[serde(default, rename = "override-raw")]
    pub override_raw: Vec<RawSetRule>,
    #[serde(default)]
    pub filter: Vec<FilterRule>,
}

impl PayloadRules {
    /// True when no rule could ever fire, so a caller can skip the work.
    pub fn is_empty(&self) -> bool {
        self.default.is_empty()
            && self.default_raw.is_empty()
            && self.override_rules.is_empty()
            && self.override_raw.is_empty()
            && self.filter.is_empty()
    }
}

/// What [`apply`] did, and what it could not do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Parameters set because the caller left them out.
    pub defaulted: usize,
    /// Parameters overwritten.
    pub overridden: usize,
    /// Paths removed.
    pub filtered: usize,
    /// Rule entries that could not be applied. The payload keeps every change
    /// that landed; these are for the operator, not for the client.
    pub problems: Vec<String>,
}

impl Outcome {
    /// True when the payload differs from what the caller sent.
    pub fn changed(&self) -> bool {
        self.defaulted + self.overridden + self.filtered > 0
    }
}

/// True when one of `models` names `request` and its conditions hold.
///
/// `Err` is a malformed condition path: reported per rule, never fatal.
fn selected(models: &[ModelMatch], request: &Request<'_>, payload: &Value) -> Result<bool, String> {
    if models.is_empty() {
        return Ok(true);
    }
    for candidate in models {
        if candidate.accepts(request, payload)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Evaluate a rule's selector, recording a malformed condition as a problem.
/// A rule whose condition cannot be read never rewrites anything.
fn rule_applies(
    models: &[ModelMatch],
    request: &Request<'_>,
    payload: &Value,
    outcome: &mut Outcome,
    kind: &str,
) -> bool {
    match selected(models, request, payload) {
        Ok(selected) => selected,
        Err(reason) => {
            outcome.problems.push(format!("{kind}: {reason}"));
            false
        }
    }
}

/// Case-insensitive `*` glob, which is the whole wildcard language upstream
/// supports for model names.
pub fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let text = text.to_ascii_lowercase();
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    let (mut star, mut resume) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            // Remember the most recent star: on a later mismatch, resume from
            // the byte after the previous attempt at it.
            star = Some(p);
            resume = t;
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some(at) = star {
            p = at + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn segments(path: &str) -> Result<Vec<Step>, String> {
    payload_query::steps(path)
}

/// True when `path` resolves to a value that is present and not null. A null
/// counts as missing, which is what makes a `default` rule able to fill it.
pub fn path_present(payload: &Value, path: &str) -> bool {
    match segments(path) {
        Ok(steps) => payload_query::present(payload, &steps),
        Err(_) => false,
    }
}

fn set_in(current: &mut Value, parts: &[Step], value: Value) -> Result<usize, String> {
    payload_query::set(current, parts, value)
}

fn remove_in(current: &mut Value, parts: &[Step]) -> usize {
    payload_query::remove(current, parts)
}

/// Apply every rule that selects `model` on `protocol`, in `default` ->
/// `override` -> `filter` order, to `payload`, without request headers.
pub fn apply(rules: &PayloadRules, model: &str, protocol: &str, payload: &mut Value) -> Outcome {
    apply_request(rules, &Request::new(model, protocol), payload)
}

/// Apply every rule that selects `request`, in `default` -> `override` ->
/// `filter` order, to `payload`.
///
/// `payload` must be the JSON object clud is about to forward. Conditions see
/// the payload as it stands when their rule is evaluated, so an `override`
/// rule sees what an earlier `default` rule wrote — the same order the caller's
/// body would be rewritten in. A rule entry that cannot be applied is recorded
/// in [`Outcome::problems`] and skipped; nothing here can fail the request.
pub fn apply_request(rules: &PayloadRules, request: &Request<'_>, payload: &mut Value) -> Outcome {
    let mut outcome = Outcome::default();
    outcome.defaulted += apply_set_rules(
        &rules.default,
        "default",
        true,
        request,
        payload,
        &mut outcome,
    );
    outcome.defaulted += apply_raw_rules(
        &rules.default_raw,
        "default-raw",
        true,
        request,
        payload,
        &mut outcome,
    );
    outcome.overridden += apply_set_rules(
        &rules.override_rules,
        "override",
        false,
        request,
        payload,
        &mut outcome,
    );
    outcome.overridden += apply_raw_rules(
        &rules.override_raw,
        "override-raw",
        false,
        request,
        payload,
        &mut outcome,
    );
    outcome.filtered += apply_filter_rules(&rules.filter, request, payload, &mut outcome);
    outcome
}

/// Set every parameter of every matching rule; `fill_missing` makes this a
/// `default` (only when the path is absent or null) rather than an `override`.
fn apply_set_rules(
    rules: &[SetRule],
    kind: &str,
    fill_missing: bool,
    request: &Request<'_>,
    payload: &mut Value,
    outcome: &mut Outcome,
) -> usize {
    let mut applied = 0;
    for rule in rules {
        if !rule_applies(&rule.models, request, payload, outcome, kind) {
            continue;
        }
        for (path, value) in &rule.params {
            if fill_missing && path_present(payload, path) {
                continue;
            }
            match segments(path).and_then(|parts| set_in(payload, &parts, value.clone())) {
                Ok(count) => applied += count,
                Err(reason) => outcome.problems.push(format!("{kind} {path}: {reason}")),
            }
        }
    }
    applied
}

/// [`apply_set_rules`] for `default-raw`/`override-raw`: the parameter value is
/// JSON text, set as the parsed value. `validate` already refused an unparsable
/// value at load time; a bad value here is still reported per rule, never fatal.
fn apply_raw_rules(
    rules: &[RawSetRule],
    kind: &str,
    fill_missing: bool,
    request: &Request<'_>,
    payload: &mut Value,
    outcome: &mut Outcome,
) -> usize {
    let mut applied = 0;
    for rule in rules {
        if !rule_applies(&rule.models, request, payload, outcome, kind) {
            continue;
        }
        for (path, text) in &rule.params {
            if fill_missing && path_present(payload, path) {
                continue;
            }
            let value = match serde_json::from_str::<Value>(text) {
                Ok(value) => value,
                Err(error) => {
                    outcome
                        .problems
                        .push(format!("{kind} {path}: raw value is not JSON: {error}"));
                    continue;
                }
            };
            match segments(path).and_then(|parts| set_in(payload, &parts, value)) {
                Ok(count) => applied += count,
                Err(reason) => outcome.problems.push(format!("{kind} {path}: {reason}")),
            }
        }
    }
    applied
}

/// Remove every path of every matching `filter` rule.
fn apply_filter_rules(
    rules: &[FilterRule],
    request: &Request<'_>,
    payload: &mut Value,
    outcome: &mut Outcome,
) -> usize {
    let mut removed = 0;
    for rule in rules {
        if !rule_applies(&rule.models, request, payload, outcome, "filter") {
            continue;
        }
        for path in &rule.params {
            match segments(path) {
                Ok(parts) => removed += remove_in(payload, &parts),
                Err(reason) => outcome.problems.push(format!("filter {path}: {reason}")),
            }
        }
    }
    removed
}

/// Parse a rules document. Refuses a non-object and any unknown key, so a typo
/// (`overide`) is reported instead of silently doing nothing.
pub fn parse(bytes: &[u8]) -> Result<PayloadRules, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("rules are not JSON: {error}"))?;
    // serde would happily read a struct out of a positional JSON array; a rules
    // document is an object or nothing.
    if !value.is_object() {
        return Err("a rules document is a JSON object".to_string());
    }
    let rules: PayloadRules =
        serde_json::from_value(value).map_err(|error| format!("rules do not parse: {error}"))?;
    validate(&rules)?;
    Ok(rules)
}

fn validate(rules: &PayloadRules) -> Result<(), String> {
    let set_rules = rules.default.iter().chain(&rules.override_rules);
    let raw_rules = rules.default_raw.iter().chain(&rules.override_raw);
    let filter_models = rules.filter.iter().flat_map(|rule| rule.models.iter());
    for model in set_rules
        .clone()
        .flat_map(|rule| rule.models.iter())
        .chain(raw_rules.clone().flat_map(|rule| rule.models.iter()))
        .chain(filter_models)
    {
        if model.name.trim().is_empty() {
            return Err("a model selector has an empty name".to_string());
        }
        if model
            .protocol
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("a model selector has an empty protocol".to_string());
        }
        if model
            .from_protocol
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("a model selector has an empty from-protocol".to_string());
        }
    }
    // Only `default`/`override` (typed and raw) paths must be well formed here:
    // a filter path that is not well formed removes nothing, and is reported
    // per request.
    for path in set_rules
        .flat_map(|rule| rule.params.keys())
        .chain(raw_rules.flat_map(|rule| rule.params.keys()))
    {
        let steps = segments(path).map_err(|reason| format!("rules: {reason}"))?;
        // A selector names an array element, not a member of it: without a
        // trailing key there is nothing to set, and the operator should hear
        // about it when the file loads, not per request.
        if matches!(steps.last(), Some(Step::Query(_))) {
            return Err(format!(
                "rules: `{path}` ends in a selector; add the field to set"
            ));
        }
    }
    // A `*-raw` value is JSON text: refuse it now, when the operator is looking,
    // rather than forwarding a string where an object was meant.
    for rule in rules.default_raw.iter().chain(&rules.override_raw) {
        for (path, text) in &rule.params {
            if serde_json::from_str::<Value>(text).is_err() {
                return Err(format!("rules: `{path}` raw value is not JSON: {text}"));
            }
        }
    }
    Ok(())
}

/// Read and parse one rules file.
pub fn from_path(path: &Path) -> Result<PayloadRules, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("{} is unreadable: {error}", path.display()))?;
    if metadata.len() > MAX_RULES_BYTES {
        return Err(format!(
            "{} is {} bytes, over the {MAX_RULES_BYTES}-byte limit",
            path.display(),
            metadata.len()
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("{} is unreadable: {error}", path.display()))?;
    parse(&bytes)
}

/// Read the rules document out of a parsed settings file.
///
/// `Ok(None)` when the key is absent or null: an install that never configured
/// payload rules must behave exactly like one without the feature. A present
/// but malformed value is an error the caller reports once, not per request.
pub fn from_settings_document(document: &Value) -> Result<Option<PayloadRules>, String> {
    let mut node = document;
    for segment in SETTINGS_KEY.split('.') {
        match node.get(segment) {
            Some(next) => node = next,
            None => return Ok(None),
        }
    }
    if node.is_null() {
        return Ok(None);
    }
    if !node.is_object() {
        return Err(format!("settings `{SETTINGS_KEY}` is not an object"));
    }
    serde_json::from_value::<PayloadRules>(node.clone())
        .map(Some)
        .map_err(|error| format!("settings `{SETTINGS_KEY}` do not parse: {error}"))
}

/// Where [`active`] reads the rules document from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// The file named by [`RULES_ENV`].
    EnvFile(PathBuf),
    /// [`SETTINGS_KEY`] inside the user-level settings file.
    Settings(PathBuf),
}

impl Source {
    fn path(&self) -> &Path {
        match self {
            Source::EnvFile(path) | Source::Settings(path) => path,
        }
    }

    /// A settings file that was never created means "no rules" silently;
    /// only an explicitly named env file earns a complaint when it is missing.
    fn is_env(&self) -> bool {
        matches!(self, Source::EnvFile(_))
    }

    fn load(&self) -> Result<Option<PayloadRules>, String> {
        match self {
            Source::EnvFile(path) => from_path(path).map(Some),
            Source::Settings(path) => {
                let document = crate::clud_settings::read_settings_json_file(path)
                    .map_err(|error| format!("{} is unreadable: {error}", path.display()))?;
                from_settings_document(&document)
            }
        }
    }
}

/// [`RULES_ENV`] when it names a file, else the user-level settings file.
/// Seamed so the precedence rule can be tested without touching the process
/// environment.
fn source_for(env: Option<std::ffi::OsString>, home: Option<&Path>) -> Option<Source> {
    match env {
        Some(raw) if !raw.is_empty() => return Some(Source::EnvFile(PathBuf::from(raw))),
        _ => {}
    }
    let home = home?;
    Some(Source::Settings(crate::clud_settings::settings_path_at(
        home,
    )))
}

fn resolve_source() -> Option<Source> {
    source_for(
        std::env::var_os(RULES_ENV),
        crate::home::user_home().as_deref(),
    )
}

/// The rules [`active`] last parsed, keyed by what it observed on disk.
struct Cached {
    source: Source,
    modified: Option<SystemTime>,
    len: u64,
    rules: Arc<PayloadRules>,
}

static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();

fn fingerprint(path: &Path) -> (Option<SystemTime>, u64) {
    match std::fs::metadata(path) {
        Ok(metadata) => (metadata.modified().ok(), metadata.len()),
        Err(_) => (None, 0),
    }
}

/// The active rules, re-read whenever their document changes.
///
/// `None` means "no rules": no source could be resolved, the settings file has
/// no [`SETTINGS_KEY`], or the document could not be read or parsed. A problem
/// is reported on stderr once per change rather than per request, and the
/// previous rules stay out of the way — forwarding the caller's own bytes is
/// the safe behavior.
pub fn active() -> Option<Arc<PayloadRules>> {
    let source = resolve_source()?;
    let (modified, len) = fingerprint(source.path());
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().ok()?;
    if let Some(cached) = guard.as_ref() {
        if cached.source == source && cached.modified == modified && cached.len == len {
            return Some(Arc::clone(&cached.rules));
        }
    }
    if modified.is_none() {
        if source.is_env() {
            eprintln!(
                "payload rules: {} is unreadable; forwarding request bodies unchanged",
                source.path().display()
            );
        }
        *guard = None;
        return None;
    }
    match source.load() {
        Ok(Some(rules)) => {
            let rules = Arc::new(rules);
            *guard = Some(Cached {
                source,
                modified,
                len,
                rules: Arc::clone(&rules),
            });
            Some(rules)
        }
        Ok(None) => {
            *guard = None;
            None
        }
        Err(reason) => {
            eprintln!("payload rules: {reason}; forwarding request bodies unchanged");
            *guard = None;
            None
        }
    }
}

/// Apply the active rules to one request body, returning the body to forward.
///
/// The bytes are returned untouched when no rule fired, so a launch without
/// [`RULES_ENV`] is byte-for-byte the pre-existing behavior.
pub fn rewrite_body(
    model: &str,
    protocol: &str,
    body: Vec<u8>,
    payload: &mut Value,
) -> (Vec<u8>, Outcome) {
    rewrite_body_with_headers(model, protocol, &[], body, payload)
}

/// [`rewrite_body`] for a request whose headers a `headers` condition needs.
pub fn rewrite_body_with_headers(
    model: &str,
    protocol: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    payload: &mut Value,
) -> (Vec<u8>, Outcome) {
    rewrite_body_with_source(model, protocol, "", headers, body, payload)
}

/// [`rewrite_body_with_headers`] for a request whose *client* protocol is known
/// (`openai` for a translated `POST /v1/chat/completions`, `claude` for a
/// Messages caller), so `from-protocol` selectors can fire.
pub fn rewrite_body_with_source(
    model: &str,
    protocol: &str,
    from_protocol: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    payload: &mut Value,
) -> (Vec<u8>, Outcome) {
    let Some(rules) = active() else {
        return (body, Outcome::default());
    };
    if rules.is_empty() {
        return (body, Outcome::default());
    }
    let outcome = apply_request(
        &rules,
        &Request::with_source(model, protocol, from_protocol, headers),
        payload,
    );
    if !outcome.changed() {
        return (body, outcome);
    }
    match serde_json::to_vec(payload) {
        Ok(rewritten) => (rewritten, outcome),
        Err(reason) => {
            let mut outcome = outcome;
            outcome
                .problems
                .push(format!("re-serialize failed: {reason}"));
            (body, outcome)
        }
    }
}

/// Split a `Name: value` command-line header. A header without a colon is
/// refused: silently dropping it would make a `headers` condition select
/// nothing and look like a bug in the rules.
fn parse_header(raw: &str) -> Result<(String, String), String> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| format!("`{raw}` is not a header; write `Name: value`"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("`{raw}` has an empty header name"));
    }
    Ok((name.to_string(), value.trim().to_string()))
}

/// `clud __apply-payload-rules [--rules <path> | --settings-file <path>]
/// [--model M] [--protocol P] [--from-protocol P] [--header "Name: value"]...`
/// — the process seam behind the Python subprocess test. Reads
/// `{"rules": {...}, "payload": {...}}` on stdin (or the rules from `--rules`,
/// or [`SETTINGS_KEY`] from `--settings-file`, and just the payload on stdin)
/// and writes `{"payload": ..., "outcome": ...}`.
pub fn run_apply_stdin(
    rules_path: Option<&Path>,
    settings_file: Option<&Path>,
    model: &str,
    protocol: &str,
    from_protocol: &str,
    headers: Vec<String>,
) -> i32 {
    use std::io::{Read, Write};

    let mut body = Vec::new();
    if std::io::stdin().read_to_end(&mut body).is_err() {
        eprintln!("could not read stdin");
        return 2;
    }
    let document: Value = match serde_json::from_slice(&body) {
        Ok(document) => document,
        Err(error) => {
            eprintln!("stdin is not JSON: {error}");
            return 2;
        }
    };
    let rules = match (rules_path, settings_file) {
        (Some(path), _) => match from_path(path) {
            Ok(rules) => rules,
            Err(reason) => {
                eprintln!("{reason}");
                return 2;
            }
        },
        (None, Some(path)) => {
            let settings = match crate::clud_settings::read_settings_json_file(path) {
                Ok(settings) => settings,
                Err(error) => {
                    eprintln!("{} is unreadable: {error}", path.display());
                    return 2;
                }
            };
            match from_settings_document(&settings) {
                Ok(Some(rules)) => rules,
                Ok(None) => {
                    eprintln!("{} has no `{SETTINGS_KEY}` object", path.display());
                    return 2;
                }
                Err(reason) => {
                    eprintln!("{reason}");
                    return 2;
                }
            }
        }
        (None, None) => match document.get("rules") {
            Some(rules) => match serde_json::from_value::<PayloadRules>(rules.clone()) {
                Ok(rules) => rules,
                Err(error) => {
                    eprintln!("rules do not parse: {error}");
                    return 2;
                }
            },
            None => {
                eprintln!("stdin carries no `rules` object");
                return 2;
            }
        },
    };
    let mut parsed_headers = Vec::with_capacity(headers.len());
    for raw in &headers {
        match parse_header(raw) {
            Ok(header) => parsed_headers.push(header),
            Err(reason) => {
                eprintln!("{reason}");
                return 2;
            }
        }
    }
    let mut payload = document.get("payload").cloned().unwrap_or(Value::Null);
    let outcome = apply_request(
        &rules,
        &Request::with_source(model, protocol, from_protocol, &parsed_headers),
        &mut payload,
    );
    let rendered = serde_json::json!({
        "payload": payload,
        "outcome": {
            "defaulted": outcome.defaulted,
            "overridden": outcome.overridden,
            "filtered": outcome.filtered,
            "problems": outcome.problems,
            "changed": outcome.changed(),
        },
    });
    let mut bytes = serde_json::to_vec(&rendered).unwrap_or_default();
    bytes.push(b'\n');
    match std::io::stdout().write_all(&bytes) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("could not write the result: {error}");
            2
        }
    }
}

#[cfg(test)]
#[path = "payload_rules_tests.rs"]
mod tests;

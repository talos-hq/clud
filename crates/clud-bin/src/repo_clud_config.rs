//! `.clud/settings.json` discovery + parser (repo-level AND user-level).
//!
//! Mirrors the `.claude/settings.json` convention so the two repo-scoped
//! config systems read symmetrically. When clud starts a session inside
//! a repo that ships a `.clud/settings.json` declaring
//! `"rust": { "use_soldr": true }`, clud transparently routes Rust
//! toolchain calls through soldr by prepending soldr's shim dir to the
//! session `PATH` (see [`crate::soldr_activate`] and zackees/clud#343).
//!
//! ## Three-level layout (DD-014, extended by issue #525)
//!
//! - **User-level** `~/.clud/settings.json` — defaults that apply to
//!   every repo the user opens. Lives next to the existing
//!   `~/.clud/settings.toml` (DD'd separately as the user-edited dev
//!   settings, owned by [`crate::clud_settings`]).
//! - **Repo-level** `<repo-root>/.clud/settings.json` — per-repo
//!   overrides. Lands in version control alongside other repo configs.
//! - **Repo-local** `<repo-root>/.clud/settings.local.json` — the
//!   gitignored per-developer layer. Documented and gitignored from the
//!   start but never actually read until #525, so rules placed there were
//!   silently ignored.
//!
//! Merge semantics: **repo-local > repo > user**, per field. Most fields unset
//! at one level fall through to the next; unset everywhere uses the baked-in
//! default. The exception is Soldr's version policy: once an upper layer has
//! any Rust directive, omitting its version explicitly selects rolling latest
//! instead of inheriting a lower layer's numeric pin. `bad_commands` and
//! `bad_pipelines` instead concatenate across layers, deduplicated by `id` with
//! the higher layer winning, so a local override can replace one shared rule
//! without restating the rest.
//! This mirrors how `.claude/settings.json` layers with
//! `~/.claude/settings.json` in Claude Code.
//!
//! Schema (v1):
//!
//! ```json
//! {
//!   "rust": {
//!     "use_soldr": true,        // route cargo/rustc/rustfmt/clippy-driver/
//!                               // rustdoc through soldr (default: true when
//!                               // a settings file is present).
//!     "install":   true,        // auto-install soldr if missing (default: true).
//!     "porcelain": true,        // export SOLDR_PORCELAIN=1 to the agent session so soldr
//!                               // prints one JSON line (default: false, #2443).
//!     "version":   "0.7.55"     // IGNORED with a yellow warning: soldr
//!                               // cannot be pinned (DD-173); the installed
//!                               // soldr is used and kept at latest.
//!   },
//!   "bash": {
//!     "block_cd": "auto"        // "auto" | true | false — pin the session
//!                               // cwd to a registered repo root. See
//!                               // `block_bad_cmd_cd.rs` and DD-047.
//!   },
//!   "gh": {
//!     "backend": "ghd"          // "gh" (default) | "ghd" — the session's
//!                               // `gh` shim runs `ghd gh ARGS` (#2352).
//!   }
//! }
//! ```
//!
//! The current `clud optimize rust` command writes the equivalent shape under
//! `"optimize": { "rust": { "use_soldr_shims": ..., "install_soldr": ...,
//! "soldr_version": ... } }`. This parser accepts both forms. Direct `rust`
//! keys win over `optimize.rust` keys within a file.

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------
// Raw parse types — every field is Option so the merge step can tell
// "user set this to false" from "user didn't set this".
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RawRepoCludConfig {
    pub rust: RawRustConfig,
    pub optimize: RawOptimizeConfig,
    pub bash: RawBashConfig,
    pub gh: RawGhConfig,
    pub hook_roots: RawHookRootsConfig,
    #[serde(deserialize_with = "deserialize_bad_commands")]
    pub bad_commands: Vec<BadCommandRule>,
    #[serde(deserialize_with = "deserialize_bad_pipelines")]
    pub bad_pipelines: Vec<BadPipelineRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RawRustConfig {
    pub use_soldr: Option<bool>,
    pub install: Option<bool>,
    pub version: Option<String>,
    pub porcelain: Option<bool>,
}

/// The `bash` section. `block_cd` is deliberately typed as a raw
/// [`Value`] rather than the [`BlockCd`] enum: a typo like
/// `"block_cd": "strictt"` would otherwise fail the whole document, and
/// `read_and_parse_raw` drops a document it cannot parse — silently taking
/// the file's `bad_commands` rules down with it. `parse_raw_repo_clud_config`
/// normalizes the value and warns instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RawBashConfig {
    pub block_cd: Option<Value>,
}

/// The `gh` section. `backend` is a raw [`Value`] for the same reason as
/// [`RawBashConfig::block_cd`]: a typo is dropped with a warning instead of
/// discarding the whole document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RawGhConfig {
    pub backend: Option<Value>,
}

/// The `hook_roots` section — which other repos this session's hooks know
/// about (#966 §5, #967 Phase 3).
///
/// Only `children` is declarable: `extern` roots are the implicit
/// `.extern-repos/` convention, and a nested git repo is deliberately *not*
/// auto-detected as a child, because declaration is the consent that makes
/// the child tier's no-prompt trust sound (#966 D8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RawHookRootsConfig {
    pub children: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RawOptimizeConfig {
    pub rust: RawOptimizeRustConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RawOptimizeRustConfig {
    pub use_soldr_shims: Option<bool>,
    pub install_soldr: Option<bool>,
    pub soldr_version: Option<String>,
}

// ---------------------------------------------------------------------
// `bad_commands` — generic "bad command -> blessed replacement" rules
// (zackees/clud#519). Each entry is fully validated at parse time: a
// rule with a bad shape or an invalid glob/regex pattern is skipped
// with a warning rather than failing the whole file, mirroring
// `read_and_parse_raw`'s malformed-JSON handling.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    Glob,
    Regex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadCommandRule {
    pub id: Option<String>,
    pub pattern: String,
    pub match_mode: MatchMode,
    pub replacement: String,
    pub reason: String,
    pub passthrough_prefixes: Vec<String>,
    pub allow_override: bool,
    pub through_wrappers: Vec<String>,
    pub arguments: Option<ArgumentMatcher>,
    /// Where this rule came from (#525). `None` for rules parsed from a bare
    /// string (tests) or constructed programmatically; `Some` once a rule has
    /// been read from a settings file, so a denial can name its exact origin.
    pub source: Option<RuleSource>,
}

/// Non-serialized provenance for a [`BadCommandRule`]: which settings file,
/// layer, and array slot defined it. Survives merging so a denial can cite an
/// unambiguous `<file>#/bad_commands/<index>` reference even after rules from
/// three layers are concatenated (#525).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSource {
    /// 0-based index of this rule in its file's `/bad_commands` array, counting
    /// malformed-and-skipped entries so the JSON pointer stays accurate.
    pub index: usize,
    /// Canonical settings file path. `None` when the rule was parsed from a
    /// string with no backing file (tests, programmatic rules).
    pub file: Option<PathBuf>,
    /// Source layer: `"user"`, `"repo"`, or `"repo-local"`. `None` when `file`
    /// is `None`.
    pub layer: Option<String>,
}

impl RuleSource {
    /// A compact, unambiguous reference like
    /// `C:\repo\.clud\settings.local.json#/bad_commands/0`, or just
    /// `#/bad_commands/0` when no backing file is known.
    pub fn reference(&self) -> String {
        match &self.file {
            Some(file) => format!("{}#/bad_commands/{}", file.display(), self.index),
            None => format!("#/bad_commands/{}", self.index),
        }
    }

    /// The JSON pointer to this rule within its file (`/bad_commands/<index>`).
    pub fn pointer(&self) -> String {
        format!("/bad_commands/{}", self.index)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPattern {
    pub pattern: String,
    pub match_mode: MatchMode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArgumentMatcher {
    pub prefix: Vec<MatchPattern>,
    pub ordered: Vec<MatchPattern>,
    pub contiguous: Vec<MatchPattern>,
    pub any: Vec<MatchPattern>,
    pub all: Vec<MatchPattern>,
    pub none: Vec<MatchPattern>,
    pub short_flags_any: Vec<char>,
    pub short_flags_all: Vec<char>,
    pub any_of: Vec<ArgumentMatcher>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMatcher {
    pub pattern: String,
    pub match_mode: MatchMode,
    pub through_wrappers: Vec<String>,
    pub arguments: Option<ArgumentMatcher>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadPipelineRule {
    pub id: Option<String>,
    pub stages: Vec<CommandMatcher>,
    pub replacement: String,
    pub reason: String,
    pub allow_override: bool,
}

/// Compile a rule's `pattern` (glob or regex, per `mode`) into a
/// `Regex` anchored to match the *whole* normalized program-name
/// token, never a substring/prefix. Both syntaxes are auto-anchored:
/// callers never need to write `^`/`$` themselves.
pub fn compile_match_pattern(pattern: &str, mode: MatchMode) -> Result<Regex, String> {
    let body = match mode {
        MatchMode::Regex => pattern.to_string(),
        MatchMode::Glob => glob_to_regex_source(pattern)?,
    };
    Regex::new(&format!("(?i)^(?:{body})$")).map_err(|e| e.to_string())
}

fn glob_to_regex_source(glob: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut bracket_depth = 0i32;
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if bracket_depth == 0 => out.push_str(".*"),
            '?' if bracket_depth == 0 => out.push('.'),
            '[' => {
                bracket_depth += 1;
                out.push('[');
                if let Some('!') = chars.peek() {
                    out.push('^');
                    chars.next();
                }
            }
            ']' => {
                if bracket_depth == 0 {
                    return Err("unmatched ']' in glob pattern".to_string());
                }
                bracket_depth -= 1;
                out.push(']');
            }
            c if bracket_depth > 0 => out.push(c),
            c => {
                if "\\.+^$(){}|".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    if bracket_depth != 0 {
        return Err("unmatched '[' in glob pattern".to_string());
    }
    Ok(out)
}

fn parse_bad_command_rule(
    value: &serde_json::Value,
    index: usize,
) -> Result<BadCommandRule, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "bad_commands entry is not a JSON object".to_string())?;
    let pattern = object
        .get("match")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "bad_commands entry missing required string field \"match\"".to_string())?
        .to_string();
    let replacement = object
        .get("replacement")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            "bad_commands entry missing required string field \"replacement\"".to_string()
        })?
        .to_string();
    let reason = object
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let id = object
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let match_mode_raw = object.get("match_mode").and_then(serde_json::Value::as_str);
    let match_mode = match match_mode_raw {
        None | Some("glob") => MatchMode::Glob,
        Some("regex") => MatchMode::Regex,
        Some(other) => {
            let msg = format!(
                "bad_commands entry has unknown match_mode {other:?}; expected \"glob\" or \"regex\""
            );
            return Err(msg);
        }
    };
    let passthrough_prefixes = object
        .get("passthrough_prefixes")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let allow_override = object
        .get("allow_override")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let through_wrappers = parse_wrappers(object.get("through_wrappers"))?;
    let arguments = object
        .get("arguments")
        .map(|value| parse_argument_matcher(value, 0))
        .transpose()?;

    compile_match_pattern(&pattern, match_mode)?;

    Ok(BadCommandRule {
        id,
        pattern,
        match_mode,
        replacement,
        reason,
        passthrough_prefixes,
        allow_override,
        through_wrappers,
        arguments,
        // Index is captured now; the file + layer are stamped later by
        // `read_and_parse_raw`, which is the only caller that knows them.
        source: Some(RuleSource {
            index,
            file: None,
            layer: None,
        }),
    })
}

const MAX_ARGUMENT_MATCHER_DEPTH: usize = 8;

fn parse_match_mode(value: Option<&serde_json::Value>, context: &str) -> Result<MatchMode, String> {
    match value.and_then(serde_json::Value::as_str) {
        None | Some("glob") => Ok(MatchMode::Glob),
        Some("regex") => Ok(MatchMode::Regex),
        Some(other) => Err(format!(
            "{context} has unknown match_mode {other:?}; expected \"glob\" or \"regex\""
        )),
    }
}

fn parse_match_pattern(value: &serde_json::Value, context: &str) -> Result<MatchPattern, String> {
    let (pattern, match_mode) = if let Some(pattern) = value.as_str() {
        (pattern.to_string(), MatchMode::Glob)
    } else {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{context} pattern must be a string or JSON object"))?;
        for key in object.keys() {
            if !["match", "match_mode"].contains(&key.as_str()) {
                return Err(format!("{context} pattern has unknown field {key:?}"));
            }
        }
        let pattern = object
            .get("match")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{context} pattern missing required string field \"match\""))?
            .to_string();
        let mode = parse_match_mode(object.get("match_mode"), context)?;
        (pattern, mode)
    };
    compile_match_pattern(&pattern, match_mode)
        .map_err(|err| format!("{context} has invalid pattern {pattern:?}: {err}"))?;
    Ok(MatchPattern {
        pattern,
        match_mode,
    })
}

fn parse_pattern_array(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Vec<MatchPattern>, String> {
    let Some(value) = object.get(field) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("arguments.{field} must be an array"))?;
    array
        .iter()
        .map(|item| parse_match_pattern(item, &format!("arguments.{field}")))
        .collect()
}

fn parse_short_flags(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Vec<char>, String> {
    let Some(value) = object.get(field) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("arguments.{field} must be an array"))?;
    array
        .iter()
        .map(|item| {
            let raw = item
                .as_str()
                .ok_or_else(|| format!("arguments.{field} entries must be strings"))?;
            let mut chars = raw.chars();
            let flag = chars
                .next()
                .ok_or_else(|| format!("arguments.{field} entries must be one flag character"))?;
            if chars.next().is_some() || flag == '-' {
                return Err(format!(
                    "arguments.{field} entry {raw:?} must be one flag character without '-'"
                ));
            }
            Ok(flag)
        })
        .collect()
}

fn parse_argument_matcher(
    value: &serde_json::Value,
    depth: usize,
) -> Result<ArgumentMatcher, String> {
    if depth > MAX_ARGUMENT_MATCHER_DEPTH {
        return Err(format!(
            "arguments.any_of nesting exceeds {MAX_ARGUMENT_MATCHER_DEPTH} levels"
        ));
    }
    let object = value
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;
    const FIELDS: &[&str] = &[
        "prefix",
        "ordered",
        "contiguous",
        "any",
        "all",
        "none",
        "short_flags_any",
        "short_flags_all",
        "any_of",
    ];
    for key in object.keys() {
        if !FIELDS.contains(&key.as_str()) {
            return Err(format!("arguments has unknown field {key:?}"));
        }
    }
    let any_of = match object.get("any_of") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| "arguments.any_of must be an array".to_string())?
            .iter()
            .map(|branch| parse_argument_matcher(branch, depth + 1))
            .collect::<Result<Vec<_>, _>>()?,
    };
    let matcher = ArgumentMatcher {
        prefix: parse_pattern_array(object, "prefix")?,
        ordered: parse_pattern_array(object, "ordered")?,
        contiguous: parse_pattern_array(object, "contiguous")?,
        any: parse_pattern_array(object, "any")?,
        all: parse_pattern_array(object, "all")?,
        none: parse_pattern_array(object, "none")?,
        short_flags_any: parse_short_flags(object, "short_flags_any")?,
        short_flags_all: parse_short_flags(object, "short_flags_all")?,
        any_of,
    };
    let has_predicate = !matcher.prefix.is_empty()
        || !matcher.ordered.is_empty()
        || !matcher.contiguous.is_empty()
        || !matcher.any.is_empty()
        || !matcher.all.is_empty()
        || !matcher.none.is_empty()
        || !matcher.short_flags_any.is_empty()
        || !matcher.short_flags_all.is_empty()
        || !matcher.any_of.is_empty();
    if !has_predicate {
        return Err("arguments must contain at least one non-empty predicate".to_string());
    }
    Ok(matcher)
}

fn parse_wrappers(value: Option<&serde_json::Value>) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| "through_wrappers must be an array".to_string())?;
    let wrappers = array
        .iter()
        .map(|item| {
            item.as_str()
                .map(|value| value.to_ascii_lowercase())
                .ok_or_else(|| "through_wrappers entries must be strings".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    for wrapper in &wrappers {
        if !["sudo", "env", "command", "exec"].contains(&wrapper.as_str()) {
            return Err(format!(
                "unsupported through_wrappers entry {wrapper:?}; expected sudo, env, command, or exec"
            ));
        }
    }
    Ok(wrappers)
}

fn parse_command_matcher(
    value: &serde_json::Value,
    context: &str,
) -> Result<CommandMatcher, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{context} must be a JSON object"))?;
    let pattern = object
        .get("match")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{context} missing required string field \"match\""))?
        .to_string();
    let match_mode = parse_match_mode(object.get("match_mode"), context)?;
    compile_match_pattern(&pattern, match_mode)
        .map_err(|err| format!("{context} has invalid pattern {pattern:?}: {err}"))?;
    Ok(CommandMatcher {
        pattern,
        match_mode,
        through_wrappers: parse_wrappers(object.get("through_wrappers"))?,
        arguments: object
            .get("arguments")
            .map(|value| parse_argument_matcher(value, 0))
            .transpose()?,
    })
}

fn parse_bad_pipeline_rule(value: &serde_json::Value) -> Result<BadPipelineRule, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "bad_pipelines entry is not a JSON object".to_string())?;
    let stages = object
        .get("stages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "bad_pipelines entry missing required array field \"stages\"".to_string())?
        .iter()
        .enumerate()
        .map(|(index, stage)| parse_command_matcher(stage, &format!("bad_pipelines stage {index}")))
        .collect::<Result<Vec<_>, _>>()?;
    if stages.len() < 2 {
        return Err("bad_pipelines entry must contain at least two stages".to_string());
    }
    let replacement = object
        .get("replacement")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            "bad_pipelines entry missing required string field \"replacement\"".to_string()
        })?
        .to_string();
    Ok(BadPipelineRule {
        id: object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        stages,
        replacement,
        reason: object
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        allow_override: object
            .get("allow_override")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn deserialize_bad_commands<'de, D>(deserializer: D) -> Result<Vec<BadCommandRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;
    let mut rules = Vec::with_capacity(raw.len());
    // Enumerate over the original array so a rule's recorded index counts
    // malformed-and-skipped entries — the JSON pointer must address the real
    // slot in the file, not the rule's position after filtering.
    for (index, entry) in raw.iter().enumerate() {
        match parse_bad_command_rule(entry, index) {
            Ok(rule) => rules.push(rule),
            Err(err) => {
                eprintln!("clud: skipping malformed bad_commands rule: {err}; ignoring");
            }
        }
    }
    Ok(rules)
}

/// Stamp each `bad_commands` rule with the file + layer it was read from
/// (#525). Called once per settings file after parsing; the array index was
/// already captured at parse time. The layer name is the compact form used in
/// denials (`user` / `repo` / `repo-local`), mapped from the internal scope
/// label.
fn stamp_bad_command_sources(raw: &mut RawRepoCludConfig, path: &Path, scope: &str) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let layer = layer_name_for_scope(scope);
    for rule in &mut raw.bad_commands {
        if let Some(source) = rule.source.as_mut() {
            source.file = Some(canonical.clone());
            source.layer = Some(layer.to_string());
        }
    }
}

/// Map the internal scope label used by discovery to the compact provenance
/// layer name surfaced in denials.
fn layer_name_for_scope(scope: &str) -> &'static str {
    match scope {
        "repo-local" => "repo-local",
        "repo-level" => "repo",
        _ => "user",
    }
}

fn deserialize_bad_pipelines<'de, D>(deserializer: D) -> Result<Vec<BadPipelineRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;
    let mut rules = Vec::with_capacity(raw.len());
    for entry in raw {
        match parse_bad_pipeline_rule(&entry) {
            Ok(rule) => rules.push(rule),
            Err(err) => {
                eprintln!("clud: skipping malformed bad_pipelines rule: {err}; ignoring");
            }
        }
    }
    Ok(rules)
}

/// Concatenate `upper` (e.g. repo-level) over `lower` (e.g. user-level)
/// rules. Unlike the scalar rust-config fields, arrays add rather than
/// override: every rule from both levels is active, except that a
/// `lower` rule sharing an `id` with an `upper` rule is dropped in
/// favor of the `upper` definition (id-less rules never dedupe).
fn concat_dedupe_bad_commands(
    upper: Vec<BadCommandRule>,
    lower: Vec<BadCommandRule>,
) -> Vec<BadCommandRule> {
    let upper_ids: HashSet<&str> = upper.iter().filter_map(|r| r.id.as_deref()).collect();
    let mut result: Vec<BadCommandRule> = lower
        .into_iter()
        .filter(|r| match &r.id {
            Some(id) => !upper_ids.contains(id.as_str()),
            None => true,
        })
        .collect();
    result.extend(upper);
    result
}

fn concat_dedupe_bad_pipelines(
    upper: Vec<BadPipelineRule>,
    lower: Vec<BadPipelineRule>,
) -> Vec<BadPipelineRule> {
    let upper_ids: HashSet<&str> = upper.iter().filter_map(|r| r.id.as_deref()).collect();
    let mut result: Vec<BadPipelineRule> = lower
        .into_iter()
        .filter(|r| match &r.id {
            Some(id) => !upper_ids.contains(id.as_str()),
            None => true,
        })
        .collect();
    result.extend(upper);
    result
}

// ---------------------------------------------------------------------
// Resolved types — what the rest of the binary actually consumes.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoCludConfig {
    pub rust: RustConfig,
    pub bash: BashConfig,
    pub gh: GhConfig,
    pub hook_roots: HookRootsConfig,
    pub bad_commands: Vec<BadCommandRule>,
    pub bad_pipelines: Vec<BadPipelineRule>,
}

/// The `hook_roots` section of `.clud/settings.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookRootsConfig {
    /// Declared organizational children of this repo.
    pub children: Vec<String>,
}

/// The `gh` section of `.clud/settings.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhConfig {
    pub backend: GhBackend,
}

/// `gh.backend` (#2352): what the session's `gh` shim runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GhBackend {
    /// The real gh (plus the read broker and the watch upgrade).
    #[default]
    Gh,
    /// Every call goes to `ghd gh ARGS`, which serves what it can and
    /// relays the rest to the real gh. ghd is then required.
    Ghd,
}

impl GhBackend {
    pub fn from_json(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "gh" => Some(Self::Gh),
            "ghd" => Some(Self::Ghd),
            _ => None,
        }
    }
}

/// The `bash` section of `.clud/settings.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BashConfig {
    pub block_cd: BlockCd,
}

/// `bash.block_cd` — how strictly a session-mutating `cd` is policed
/// (zackees/clud#966 §8).
///
/// `"auto"` is the default because the right answer depends on the repo:
/// it resolves at hook-fire time against the hooks actually in scope. See
/// `block_bad_cmd_cd::resolve_policy` for the resolution table and DD-047
/// for why this is a first-class key rather than a `bad_commands` rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BlockCd {
    /// Resolve against the environment at hook-fire time.
    #[default]
    Auto,
    /// Always pin the session cwd to a registered repo root.
    Always,
    /// Never police `cd`.
    Never,
}

impl BlockCd {
    /// Parse the JSON spelling: `true` / `false` / `"auto"`, plus the
    /// `"true"` / `"false"` string forms a hand-edited file tends to grow.
    /// `None` for anything else, which the caller reports and ignores.
    pub fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(true) => Some(Self::Always),
            Value::Bool(false) => Some(Self::Never),
            Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "auto" => Some(Self::Auto),
                "true" | "always" | "strict" => Some(Self::Always),
                "false" | "never" | "off" => Some(Self::Never),
                _ => None,
            },
            _ => None,
        }
    }

    /// The canonical JSON spelling, for writers.
    #[must_use]
    pub fn as_json(self) -> Value {
        match self {
            Self::Auto => Value::String("auto".to_string()),
            Self::Always => Value::Bool(true),
            Self::Never => Value::Bool(false),
        }
    }

    /// The label `clud settings` shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustConfig {
    pub use_soldr: bool,
    pub install: bool,
    pub version: Option<String>,
    /// Export `SOLDR_PORCELAIN=1` into agent sessions (#2443).
    pub porcelain: bool,
}

impl Default for RustConfig {
    fn default() -> Self {
        Self {
            use_soldr: true,
            install: true,
            version: None,
            porcelain: false,
        }
    }
}

// ---------------------------------------------------------------------
// Public discovery API.
// ---------------------------------------------------------------------

/// Resolve the effective config for a session starting at `start`.
///
/// Loads user-level `~/.clud/settings.json` first, then repo-level
/// `<repo-root>/.clud/settings.json` (walking up from `start` to the
/// `.git/` boundary). Merges with repo winning per-field. Returns
/// `None` when neither file exists.
pub fn discover_effective_clud_config(start: &Path) -> Option<RepoCludConfig> {
    let user = discover_user_clud_config_raw();
    let repo = discover_repo_clud_config_raw(start);
    resolve_effective_config(repo, user)
}

/// Public single-source variant used by tests + future direct
/// callers that don't want the merge. Walks up from `start` looking
/// for a repo-level `.clud/settings.json`. See module docs for the
/// resolution rules.
pub fn discover_repo_clud_config(start: &Path) -> Option<RepoCludConfig> {
    discover_repo_clud_config_raw(start)
        .map(|raw| resolve(merge(raw, RawRepoCludConfig::default())))
}

/// Read user-level `~/.clud/settings.json`, if present.
pub fn discover_user_clud_config() -> Option<RepoCludConfig> {
    discover_user_clud_config_raw()
        .filter(has_directive)
        .map(|raw| resolve(merge(raw, RawRepoCludConfig::default())))
}

fn resolve_effective_config(
    repo: Option<RawRepoCludConfig>,
    user: Option<RawRepoCludConfig>,
) -> Option<RepoCludConfig> {
    match (repo, user) {
        (None, None) => None,
        (None, Some(user)) if !has_directive(&user) => None,
        (None, Some(user)) => Some(resolve(merge(user, RawRepoCludConfig::default()))),
        (Some(repo), None) => Some(resolve(merge(repo, RawRepoCludConfig::default()))),
        (Some(repo), Some(user)) => {
            let user = if has_directive(&user) {
                user
            } else {
                RawRepoCludConfig::default()
            };
            Some(resolve(merge(repo, user)))
        }
    }
}

fn has_directive(raw: &RawRepoCludConfig) -> bool {
    raw.rust.use_soldr.is_some()
        || raw.rust.install.is_some()
        || raw.rust.version.is_some()
        || raw.optimize.rust.use_soldr_shims.is_some()
        || raw.optimize.rust.install_soldr.is_some()
        || raw.optimize.rust.soldr_version.is_some()
        || raw.bash.block_cd.is_some()
        || raw.gh.backend.is_some()
        || raw.hook_roots.children.is_some()
        || !raw.bad_commands.is_empty()
        || !raw.bad_pipelines.is_empty()
}

// ---------------------------------------------------------------------
// Raw discovery (Option-shaped) — used by the merge.
// ---------------------------------------------------------------------

/// Gitignored per-developer override, layered above `settings.json` in the
/// same `.clud/` directory (issue #525).
pub const LOCAL_SETTINGS_FILE: &str = "settings.local.json";

fn discover_repo_clud_config_raw(start: &Path) -> Option<RawRepoCludConfig> {
    let mut cursor: PathBuf = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    if let Ok(real) = cursor.canonicalize() {
        cursor = real;
    }

    loop {
        let clud_dir = cursor.join(".clud");
        let shared = clud_dir.join("settings.json");
        let local = clud_dir.join(LOCAL_SETTINGS_FILE);
        // Issue #525: `.clud/settings.local.json` is documented and gitignored
        // as the user-local override layer but was never read, so a
        // `bad_commands` rule placed there was silently ignored.
        //
        // Both files are consulted at the *same* directory, and either one is
        // enough to stop the walk. Requiring `settings.json` to exist first
        // would make a local-only override invisible, which is the very shape
        // a gitignored override file is for.
        let found_local = local
            .is_file()
            .then(|| read_and_parse_raw(&local, "repo-local"));
        let found_shared = shared
            .is_file()
            .then(|| read_and_parse_raw(&shared, "repo-level"));
        if found_local.is_some() || found_shared.is_some() {
            // Local wins per field; `merge` already concatenates and dedupes
            // `bad_commands` by id with the upper layer taking precedence.
            return match (found_local.flatten(), found_shared.flatten()) {
                (Some(local), Some(shared)) => Some(merge(local, shared)),
                (Some(local), None) => Some(local),
                (None, shared) => shared,
            };
        }
        if cursor.join(".git").exists() {
            return None;
        }
        if !cursor.pop() {
            return None;
        }
    }
}

fn discover_user_clud_config_raw() -> Option<RawRepoCludConfig> {
    let home = crate::home::user_home()?;
    let candidate = home.join(".clud").join("settings.json");
    if !candidate.is_file() {
        return None;
    }
    read_and_parse_raw(&candidate, "user-level")
}

fn read_and_parse_raw(path: &Path, scope: &str) -> Option<RawRepoCludConfig> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!(
                "clud: failed to read {} ({scope}): {err}; ignoring",
                path.display()
            );
            return None;
        }
    };
    match parse_raw_repo_clud_config(&text) {
        Ok(mut raw) => {
            stamp_bad_command_sources(&mut raw, path, scope);
            Some(raw)
        }
        Err(err) => {
            eprintln!(
                "clud: {scope} settings file at {} is malformed: {err}; ignoring",
                path.display()
            );
            None
        }
    }
}

// ---------------------------------------------------------------------
// Parsing.
// ---------------------------------------------------------------------

/// Parse a `.clud/settings.json` body into the raw (Option-shaped) form.
///
/// Empty file = all-None (= all-defaults at resolve time).
/// Empty / whitespace-only `version` is normalized to `None`. The
/// case-insensitive `latest` alias is canonicalized so merge can distinguish an
/// explicit rolling policy from a field that was not specified.
pub fn parse_raw_repo_clud_config(text: &str) -> Result<RawRepoCludConfig, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(RawRepoCludConfig::default());
    }
    let mut parsed: RawRepoCludConfig =
        serde_json::from_str(text).map_err(|e: serde_json::Error| e.to_string())?;
    if let Some(v) = parsed.rust.version.as_deref() {
        if v.trim().is_empty() {
            parsed.rust.version = None;
        } else if v.eq_ignore_ascii_case("latest") {
            parsed.rust.version = Some("latest".to_string());
        }
    }
    if let Some(v) = parsed.optimize.rust.soldr_version.as_deref() {
        if v.trim().is_empty() {
            parsed.optimize.rust.soldr_version = None;
        } else if v.eq_ignore_ascii_case("latest") {
            parsed.optimize.rust.soldr_version = Some("latest".to_string());
        }
    }
    // An unrecognized `bash.block_cd` is dropped with a warning rather than
    // rejected: the alternative is `read_and_parse_raw` discarding the whole
    // document, which would silently disarm the file's `bad_commands` rules
    // over one typo.
    if let Some(value) = parsed.bash.block_cd.take() {
        match BlockCd::from_json(&value) {
            Some(_) => parsed.bash.block_cd = Some(value),
            None => eprintln!(
                "clud: ignoring unrecognized bash.block_cd value {value}; expected true, false, or \"auto\""
            ),
        }
    }
    if let Some(value) = parsed.gh.backend.take() {
        match GhBackend::from_json(&value) {
            Some(_) => parsed.gh.backend = Some(value),
            None => eprintln!(
                "clud: ignoring unrecognized gh.backend value {value}; expected \"gh\" or \"ghd\""
            ),
        }
    }
    Ok(parsed)
}

/// Convenience wrapper used by tests that want the resolved form
/// straight from a string.
pub fn parse_repo_clud_config(text: &str) -> Result<RepoCludConfig, String> {
    parse_raw_repo_clud_config(text).map(|raw| resolve(merge(raw, RawRepoCludConfig::default())))
}

// ---------------------------------------------------------------------
// Merge + resolve.
// ---------------------------------------------------------------------

/// Layer `lower` (e.g. user-level) under `upper` (e.g. repo-level).
/// `upper` wins per-field where set for the scalar rust fields. Any Rust
/// directive in `upper` makes an omitted version mean rolling latest rather
/// than allowing a lower numeric pin to leak through;
/// `bad_commands` concatenates instead (see `concat_dedupe_bad_commands`).
pub fn merge(upper: RawRepoCludConfig, lower: RawRepoCludConfig) -> RawRepoCludConfig {
    let upper_has_rust_directive = has_rust_directive(&upper);
    let upper_bad_commands = upper.bad_commands.clone();
    let lower_bad_commands = lower.bad_commands.clone();
    let upper_bad_pipelines = upper.bad_pipelines.clone();
    let lower_bad_pipelines = lower.bad_pipelines.clone();
    let upper_bash = upper.bash.clone();
    let lower_bash = lower.bash.clone();
    let upper_gh = upper.gh.clone();
    let lower_gh = lower.gh.clone();
    let upper_hook_roots = upper.hook_roots.clone();
    let lower_hook_roots = lower.hook_roots.clone();
    let upper_rust = normalize_raw_rust(upper);
    let lower_rust = normalize_raw_rust(lower);
    RawRepoCludConfig {
        rust: RawRustConfig {
            use_soldr: upper_rust.use_soldr.or(lower_rust.use_soldr),
            install: upper_rust.install.or(lower_rust.install),
            porcelain: upper_rust.porcelain.or(lower_rust.porcelain),
            version: if upper_has_rust_directive {
                upper_rust.version.or_else(|| Some("latest".to_string()))
            } else {
                lower_rust.version
            },
        },
        optimize: RawOptimizeConfig::default(),
        bash: RawBashConfig {
            block_cd: upper_bash.block_cd.or(lower_bash.block_cd),
        },
        gh: RawGhConfig {
            backend: upper_gh.backend.or(lower_gh.backend),
        },
        hook_roots: RawHookRootsConfig {
            children: upper_hook_roots.children.or(lower_hook_roots.children),
        },
        bad_commands: concat_dedupe_bad_commands(upper_bad_commands, lower_bad_commands),
        bad_pipelines: concat_dedupe_bad_pipelines(upper_bad_pipelines, lower_bad_pipelines),
    }
}

fn has_rust_directive(raw: &RawRepoCludConfig) -> bool {
    raw.rust.use_soldr.is_some()
        || raw.rust.install.is_some()
        || raw.rust.version.is_some()
        || raw.optimize.rust.use_soldr_shims.is_some()
        || raw.optimize.rust.install_soldr.is_some()
        || raw.optimize.rust.soldr_version.is_some()
}

fn normalize_raw_rust(raw: RawRepoCludConfig) -> RawRustConfig {
    let RawRepoCludConfig {
        rust,
        optimize,
        bash: _,
        gh: _,
        hook_roots: _,
        bad_commands: _,
        bad_pipelines: _,
    } = raw;
    RawRustConfig {
        use_soldr: rust.use_soldr.or(optimize.rust.use_soldr_shims),
        install: rust.install.or(optimize.rust.install_soldr),
        version: rust.version.or(optimize.rust.soldr_version),
        porcelain: rust.porcelain,
    }
}

/// Apply baked-in defaults to any remaining None fields.
pub fn resolve(raw: RawRepoCludConfig) -> RepoCludConfig {
    let RawRustConfig {
        use_soldr,
        install,
        version,
        porcelain,
    } = raw.rust;
    RepoCludConfig {
        rust: RustConfig {
            use_soldr: use_soldr.unwrap_or(true),
            install: install.unwrap_or(true),
            version: version.filter(|value| !value.eq_ignore_ascii_case("latest")),
            porcelain: porcelain.unwrap_or(false),
        },
        bash: BashConfig {
            block_cd: raw
                .bash
                .block_cd
                .as_ref()
                .and_then(BlockCd::from_json)
                .unwrap_or_default(),
        },
        gh: GhConfig {
            backend: raw
                .gh
                .backend
                .as_ref()
                .and_then(GhBackend::from_json)
                .unwrap_or_default(),
        },
        hook_roots: HookRootsConfig {
            children: raw.hook_roots.children.clone().unwrap_or_default(),
        },
        bad_commands: raw.bad_commands,
        bad_pipelines: raw.bad_pipelines,
    }
}

// ---------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------

#[cfg(test)]
#[path = "repo_clud_config_tests.rs"]
mod tests;

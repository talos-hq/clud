//! The `meta` block every workflow script exports.

use serde::{Deserialize, Serialize};

/// One `phases[]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `export const meta = {...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub phases: Vec<Phase>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MetaError {
    #[error("workflow script must begin with `export const meta = {{...}}`")]
    Missing,
    #[error("invalid workflow meta: {0}")]
    Invalid(String),
    #[error("phase(\"{0}\") is not declared in meta.phases")]
    UnknownPhase(String),
}

impl Meta {
    /// Validate a meta value produced by the script.
    pub fn from_json(value: serde_json::Value) -> Result<Self, MetaError> {
        let meta: Self =
            serde_json::from_value(value).map_err(|e| MetaError::Invalid(e.to_string()))?;
        if meta.name.trim().is_empty() {
            return Err(MetaError::Invalid("`name` is empty".into()));
        }
        Ok(meta)
    }

    /// `phase(title)` and `agent(.., {phase})` must name a declared phase
    /// when `phases` is non-empty.
    pub fn check_phase(&self, title: &str) -> Result<(), MetaError> {
        if self.phases.is_empty() || self.phases.iter().any(|p| p.title == title) {
            Ok(())
        } else {
            Err(MetaError::UnknownPhase(title.to_string()))
        }
    }
}

/// Rewrite the script into an async function body that reports `meta`
/// to the host before any other statement runs.
pub fn wrap_source(source: &str) -> Result<String, MetaError> {
    let trimmed = source.trim_start();
    let rest = trimmed
        .strip_prefix("export const meta")
        .ok_or(MetaError::Missing)?;
    let eq = rest.find('=').ok_or(MetaError::Missing)?;
    if !rest[..eq].trim().is_empty() {
        return Err(MetaError::Missing);
    }
    let after_eq = &rest[eq + 1..];
    let open = after_eq
        .find(|c: char| !c.is_whitespace())
        .filter(|&i| after_eq[i..].starts_with('{'))
        .ok_or_else(|| MetaError::Invalid("meta must be an object literal".into()))?;
    let close = matching_brace(&after_eq[open..])
        .ok_or_else(|| MetaError::Invalid("unterminated meta literal".into()))?;
    let literal = &after_eq[open..open + close + 1];
    let body = &after_eq[open + close + 1..];
    Ok(format!(
        "globalThis.__clud_main = (async () => {{\nconst meta = {literal};\n__clud_meta(JSON.stringify(meta));\n{body}\n}})().catch((e) => {{ globalThis.__clud_error = (e && e.message !== undefined) ? String(e.message) : String(e); throw e; }});"
    ))
}

/// Byte offset of the `}` matching the `{` at `text[0]`, skipping string,
/// template and comment contents.
fn matching_brace(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

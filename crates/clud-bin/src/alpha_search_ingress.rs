//! `POST /v1/alpha/search` -- Codex's standalone web-search door.
//!
//! Upstream capability: CLIProxyAPI registers `/v1/alpha/search` on the OpenAI
//! compatible group and `/backend-api/codex/alpha/search` on the Codex CLI
//! `chatgpt_base_url` group (`internal/api/server_routes.go:90` and `:127`) and
//! forwards the body to `chatgpt.com/backend-api/codex/alpha/search`
//! (`codexAlphaSearch`, `internal/api/server_routes.go:360`). Unlike
//! `/v1/responses`, the payload is *already* in Codex search format, so it must
//! not pass through a protocol translator: the only local edit upstream makes is
//! dropping the two prompt-cache fields (`sanitizeCodexAlphaSearchBody`), and
//! the answer is relayed byte-for-byte with its upstream status and
//! content type.
//!
//! This module owns that local half: the sanitizer and the local failure
//! envelope. The upstream call and the route live beside the other Codex doors
//! (`codex_upstream::UpstreamClient::alpha_search`, `codex_bridge`).

use serde_json::Value;

/// Fields the Codex search backend rejects on this door.
///
/// The search endpoint shares the prompts body shape but not the prompt cache,
/// so upstream deletes these before forwarding. Sending them is an upstream
/// 400 that looks like a client bug in clud's route table.
pub const STRIPPED_FIELDS: [&str; 2] = ["prompt_cache_key", "prompt_cache_retention"];

/// The body this door forwards upstream.
///
/// A JSON object loses the two cache fields; anything else (a non-JSON body, or
/// a JSON scalar/array) is returned byte-for-byte, exactly as upstream's
/// `sanitizeCodexAlphaSearchBody` does. This door deliberately performs no
/// other validation: the search contract belongs to the Codex backend, and a
/// locally invented stricter rule would refuse a payload the real endpoint
/// accepts.
pub fn prepare_search(body: &[u8]) -> Vec<u8> {
    let Ok(Value::Object(mut payload)) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let mut removed = false;
    for field in STRIPPED_FIELDS {
        removed |= payload.remove(field).is_some();
    }
    if !removed {
        return body.to_vec();
    }
    serde_json::to_vec(&Value::Object(payload)).unwrap_or_else(|_| body.to_vec())
}

/// The local refusal envelope.
///
/// The Codex search client parses a top-level `{"error": "<message>"}`, which
/// is what upstream writes on every local failure on this door; reusing the
/// gateway's Anthropic envelope here would be a shape its client cannot read.
pub fn render_search_error(message: &str) -> Vec<u8> {
    serde_json::json!({ "error": message })
        .to_string()
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_search_strips_the_prompt_cache_fields() {
        let body = br#"{"id":"s1","model":"gpt-5.1-codex","input":"rust borrow checker","prompt_cache_key":"abc","prompt_cache_retention":"24h"}"#;
        let prepared = prepare_search(body);
        let value: Value = serde_json::from_slice(&prepared).unwrap();
        assert!(value.get("prompt_cache_key").is_none(), "{value}");
        assert!(value.get("prompt_cache_retention").is_none(), "{value}");
        // Everything the search backend needs survives untouched.
        assert_eq!(value["id"], "s1");
        assert_eq!(value["model"], "gpt-5.1-codex");
        assert_eq!(value["input"], "rust borrow checker");
    }

    #[test]
    fn prepare_search_leaves_a_clean_body_byte_identical() {
        // A body with nothing to strip is forwarded verbatim (key order and
        // spacing included), so clud cannot perturb a payload it does not own.
        let body = br#"{"model":"gpt-5.1-codex",  "input":"x"}"#;
        assert_eq!(prepare_search(body), body);
    }

    #[test]
    fn prepare_search_forwards_an_unparseable_body_unchanged() {
        // Upstream returns the original bytes on a JSON error; a local 400 here
        // would be clud inventing a refusal the real endpoint does not make.
        let body = b"not json at all";
        assert_eq!(prepare_search(body), body);
    }

    #[test]
    fn prepare_search_forwards_a_json_array_unchanged() {
        let body = br#"[{"prompt_cache_key":"abc"}]"#;
        assert_eq!(prepare_search(body), body);
    }

    #[test]
    fn render_search_error_is_a_top_level_error_string() {
        let body = render_search_error("Codex auth unavailable");
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"], "Codex auth unavailable");
    }
}

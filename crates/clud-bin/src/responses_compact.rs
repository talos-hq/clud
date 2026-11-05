//! `POST /v1/responses/compact` -- the Responses context-compaction door.
//!
//! Upstream capability: CLIProxyAPI registers `POST /v1/responses/compact` on
//! the OpenAI-compatible group (`internal/api/server_routes.go:88`) and on the
//! Codex CLI `chatgpt_base_url` group (`:126`). Its handler
//! (`sdk/api/handlers/openai/openai_responses_handlers.go:917`
//! `OpenAIResponsesAPIHandler.Compact`) is a passthrough: it refuses
//! `stream: true`, deletes a `stream` key it finds, forwards the body to
//! `<codex base>/responses/compact` with the `responses/compact` action, and
//! relays the upstream's answer byte-for-byte.
//!
//! clud already speaks that upstream endpoint for its own auto-compaction
//! (`codex_upstream::UpstreamClient::compact`), but only from inside the launch
//! runtime -- the answer is parsed into a typed `CompactResponse`. A served
//! client had no door at all, so an OpenAI SDK's `responses.compact()` fell
//! through to a bare 404. This module owns the local half of that door: the body
//! contract and the refusal reason. The upstream call and the route live beside
//! the other Codex doors (`codex_upstream`, `codex_bridge`).

use std::fmt;

/// Why this door refuses a compaction body before it is forwarded.
///
/// Both refusals are upstream's own (`openai_responses_handlers.go:917`): a
/// streaming compaction is not a thing the backend serves, and a body that is
/// not a JSON object is not a Responses request. Everything else -- model
/// routing, item validation, the compaction contract itself -- belongs to the
/// Codex backend, and a locally invented stricter rule would refuse a payload
/// the real endpoint accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactRequestError {
    /// The body did not parse as a JSON object.
    InvalidJson,
    /// `stream: true`: compaction answers are always buffered.
    Streaming,
}

impl fmt::Display for CompactRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => formatter.write_str("request body is not valid JSON"),
            Self::Streaming => formatter.write_str("Streaming not supported for compact responses"),
        }
    }
}

/// The body this door forwards upstream.
///
/// A clean body is returned byte-for-byte (key order and spacing included), so
/// clud cannot perturb a payload it does not own. A `stream` key is deleted the
/// way upstream deletes it -- including `stream: false`, which is why the body
/// is re-serialised only when the key was actually present -- and `stream: true`
/// is refused rather than silently narrowed.
pub fn prepare_compact_request(body: &[u8]) -> Result<Vec<u8>, CompactRequestError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| CompactRequestError::InvalidJson)?;
    let serde_json::Value::Object(mut payload) = value else {
        return Err(CompactRequestError::InvalidJson);
    };
    if payload.get("stream") == Some(&serde_json::Value::Bool(true)) {
        return Err(CompactRequestError::Streaming);
    }
    if payload.remove("stream").is_none() {
        return Ok(body.to_vec());
    }
    serde_json::to_vec(&serde_json::Value::Object(payload))
        .map_err(|_| CompactRequestError::InvalidJson)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_compact_request_refuses_a_streaming_body() {
        let body = br#"{"model":"gpt-5.1-codex","input":[],"stream":true}"#;
        assert_eq!(
            prepare_compact_request(body),
            Err(CompactRequestError::Streaming)
        );
        assert_eq!(
            CompactRequestError::Streaming.to_string(),
            "Streaming not supported for compact responses"
        );
    }

    #[test]
    fn prepare_compact_request_drops_a_non_streaming_flag() {
        // Upstream deletes the key whenever it is present, so a client that
        // sends `stream: false` out of habit is answered instead of refused.
        let body = br#"{"model":"gpt-5.1-codex","input":[],"stream":false}"#;
        let prepared = prepare_compact_request(body).expect("prepared body");
        let value: serde_json::Value = serde_json::from_slice(&prepared).unwrap();
        assert!(value.get("stream").is_none(), "{value}");
        assert_eq!(value["model"], "gpt-5.1-codex");
        assert_eq!(value["input"], serde_json::json!([]));
    }

    #[test]
    fn prepare_compact_request_forwards_a_clean_body_byte_identical() {
        let body = br#"{"model":"gpt-5.1-codex",  "input":[{"type":"compaction_trigger"}]}"#;
        assert_eq!(prepare_compact_request(body).unwrap(), body);
    }

    #[test]
    fn prepare_compact_request_refuses_a_body_that_is_not_a_json_object() {
        for body in [
            &b"not json at all"[..],
            &b"[]"[..],
            &b"\"a string\""[..],
            &b""[..],
        ] {
            assert_eq!(
                prepare_compact_request(body),
                Err(CompactRequestError::InvalidJson),
                "{:?}",
                String::from_utf8_lossy(body)
            );
        }
        assert_eq!(
            CompactRequestError::InvalidJson.to_string(),
            "request body is not valid JSON"
        );
    }

    #[test]
    fn prepare_compact_request_keeps_every_field_it_does_not_own() {
        let body = br#"{"model":"gpt-5.1-codex","input":[],"include":["reasoning.encrypted_content"],"parallel_tool_calls":true,"text":{}}"#;
        let prepared = prepare_compact_request(body).unwrap();
        assert_eq!(prepared, body);
    }
}

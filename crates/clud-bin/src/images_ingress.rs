//! `POST /v1/images/generations` ingress for the Codex lane.
//!
//! CLIProxyAPI answers OpenAI's images API from the Codex executor: the direct
//! path (`internal/runtime/executor/codex_openai_images.go:341`,
//! `prepareOpenAICompatImagesPayload` in `openai_compat_executor.go:866`) is a
//! small contract -- validate the JSON, rewrite `model` to a model the upstream
//! accepts, drop `stream`, POST to `{base}/images/generations`, and hand the
//! upstream body back verbatim. This module owns the validation half; the
//! transport half lives in [`crate::codex_upstream`] and the route in
//! [`crate::codex_bridge`].
//!
//! Deliberate differences from upstream:
//!
//! * `stream: true` is refused by name instead of being forwarded. The
//!   streaming images path is a separate upstream implementation
//!   (`executeDirectOpenAIImageStream`) and is not ported yet; silently
//!   forwarding the flag would answer an SSE client with one JSON body.
//! * `POST /v1/images/edits` (multipart) is not routed here. It needs the
//!   multipart-to-JSON rewrite and is a separate slice of the ledger's
//!   `media-generation` row.
//! * An unknown model is a typed 400 naming the supported set rather than a
//!   guess at a default: clud has no image catalog, and inventing a fallback
//!   would silently spend a different model on the client's behalf.

use serde_json::Value;

/// Image models the Codex direct-images endpoint accepts.
///
/// Mirrors `codexIsDirectOpenAIImageModel` in upstream
/// `internal/runtime/executor/codex_openai_images.go:739` plus the public
/// `gpt-image-1` name clients actually send. The list is a client contract, not
/// a catalogue: `/v1/models` on the images door stays the text catalog.
pub const DIRECT_IMAGE_MODELS: [&str; 6] = [
    "gpt-image-1",
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
];

/// A validated image-generation request, ready for the upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageGeneration {
    /// The upstream model name after normalisation (provider prefix stripped).
    pub model: String,
    /// The body the upstream receives: the client's JSON with `model`
    /// normalised and `stream` removed.
    pub body: Vec<u8>,
}

/// Why an image request was refused. Every variant is a 400 the client can act
/// on; none of them costs an upstream call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagesError {
    /// The body is not JSON at all.
    InvalidJson,
    /// The body is JSON but not an object (an array decodes into a struct and
    /// would quietly drop every field, so it is refused explicitly).
    NotAnObject,
    /// No usable `prompt`.
    MissingPrompt,
    /// No usable `model`.
    MissingModel,
    /// A `stream: true` request, which this route cannot answer yet.
    Streaming,
    /// A model outside [`DIRECT_IMAGE_MODELS`].
    UnknownModel(String),
}

impl std::fmt::Display for ImagesError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson => write!(formatter, "request body is not valid JSON"),
            Self::NotAnObject => write!(formatter, "request body must be a JSON object"),
            Self::MissingPrompt => write!(formatter, "request is missing a prompt"),
            Self::MissingModel => write!(
                formatter,
                "request is missing a model; supported: {}",
                DIRECT_IMAGE_MODELS.join(", ")
            ),
            Self::Streaming => write!(
                formatter,
                "streaming image generation is not supported on this route; send \
                 `stream: false`"
            ),
            Self::UnknownModel(model) => write!(
                formatter,
                "model `{model}` is not served by the Codex images route; supported: {}",
                DIRECT_IMAGE_MODELS.join(", ")
            ),
        }
    }
}

impl std::error::Error for ImagesError {}

/// Validate an `images/generations` body and produce the upstream body.
///
/// Only `model` is rewritten and only `stream` is removed; every other field
/// (`n`, `size`, `quality`, `background`, `output_format`,
/// `output_compression`, `moderation`, `response_format`, ...) is forwarded
/// untouched, so the upstream stays the authority on its own options.
pub fn prepare_generation(body: &[u8]) -> Result<ImageGeneration, ImagesError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| ImagesError::InvalidJson)?;
    let Value::Object(mut object) = value else {
        return Err(ImagesError::NotAnObject);
    };
    if object
        .get("prompt")
        .and_then(Value::as_str)
        .is_none_or(|prompt| prompt.trim().is_empty())
    {
        return Err(ImagesError::MissingPrompt);
    }
    let raw_model = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or(ImagesError::MissingModel)?;
    if object.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(ImagesError::Streaming);
    }
    let model = normalise_model(raw_model);
    if !DIRECT_IMAGE_MODELS.contains(&model.as_str()) {
        return Err(ImagesError::UnknownModel(raw_model.to_string()));
    }
    object.insert("model".to_string(), Value::String(model.clone()));
    // The upstream's direct path deletes `stream` outright
    // (`prepareOpenAICompatImagesPayload`), so a `stream: false` that arrives
    // here leaves in the same shape every upstream implementation expects.
    object.remove("stream");
    Ok(ImageGeneration {
        model,
        body: serde_json::to_vec(&Value::Object(object)).unwrap_or_default(),
    })
}

/// Strip a provider prefix (`openai/gpt-image-1` -> `gpt-image-1`) and
/// lower-case the name, which is what upstream's
/// `codexOpenAIImageBaseModel` does before the allowlist check.
pub fn normalise_model(raw: &str) -> String {
    let model = raw.trim();
    let model = match model.rsplit_once('/') {
        Some((_, tail)) if !tail.trim().is_empty() => tail.trim(),
        _ => model,
    };
    model.to_ascii_lowercase()
}

/// The OpenAI error envelope for this door. Clients that speak the images API
/// parse `{"error": {...}}`, which is exactly what the chat translator already
/// renders.
pub fn render_error(status: u16, message: &str) -> Vec<u8> {
    crate::openai_ingress::render_chat_error(status, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(value: Value) -> Vec<u8> {
        serde_json::to_vec(&value).expect("fixture serialises")
    }

    #[test]
    fn accepts_a_minimal_generation_request() {
        let prepared = prepare_generation(&body(serde_json::json!({
            "model": "gpt-image-1.5",
            "prompt": "a red cube on a white table",
            "size": "1024x1024",
        })))
        .expect("valid request");
        assert_eq!(prepared.model, "gpt-image-1.5");
        let forwarded: Value = serde_json::from_slice(&prepared.body).expect("json");
        assert_eq!(forwarded["prompt"], "a red cube on a white table");
        assert_eq!(forwarded["size"], "1024x1024");
        assert!(forwarded.get("stream").is_none());
    }

    #[test]
    fn normalises_a_provider_prefix_and_case() {
        let prepared = prepare_generation(&body(serde_json::json!({
            "model": "OpenAI/GPT-Image-2.5",
            "prompt": "a lighthouse",
        })))
        .expect("valid request");
        assert_eq!(prepared.model, "gpt-image-2.5");
        let forwarded: Value = serde_json::from_slice(&prepared.body).expect("json");
        assert_eq!(forwarded["model"], "gpt-image-2.5");
    }

    #[test]
    fn refuses_a_non_json_body() {
        assert_eq!(
            prepare_generation(b"not json").expect_err("must refuse"),
            ImagesError::InvalidJson
        );
    }

    #[test]
    fn refuses_a_json_array_body() {
        // serde would happily decode a struct from an array; this door must not.
        assert_eq!(
            prepare_generation(b"[]").expect_err("must refuse"),
            ImagesError::NotAnObject
        );
    }

    #[test]
    fn refuses_a_request_without_a_prompt() {
        let error = prepare_generation(&body(serde_json::json!({
            "model": "gpt-image-1.5",
            "prompt": "   ",
        })))
        .expect_err("must refuse");
        assert_eq!(error, ImagesError::MissingPrompt);
        assert!(error.to_string().contains("prompt"));
    }

    #[test]
    fn refuses_a_request_without_a_model_and_names_the_supported_set() {
        let error = prepare_generation(&body(serde_json::json!({ "prompt": "a cat" })))
            .expect_err("must refuse");
        assert_eq!(error, ImagesError::MissingModel);
        for model in DIRECT_IMAGE_MODELS {
            assert!(error.to_string().contains(model), "missing {model}");
        }
    }

    #[test]
    fn refuses_an_unknown_model_by_name() {
        let error = prepare_generation(&body(serde_json::json!({
            "model": "dall-e-3",
            "prompt": "a cat",
        })))
        .expect_err("must refuse");
        assert_eq!(error, ImagesError::UnknownModel("dall-e-3".to_string()));
        assert!(error.to_string().contains("dall-e-3"));
    }

    #[test]
    fn refuses_streaming_instead_of_forwarding_the_flag() {
        let error = prepare_generation(&body(serde_json::json!({
            "model": "gpt-image-1.5",
            "prompt": "a cat",
            "stream": true,
        })))
        .expect_err("must refuse");
        assert_eq!(error, ImagesError::Streaming);
    }

    #[test]
    fn a_false_stream_flag_is_removed_not_forwarded() {
        let prepared = prepare_generation(&body(serde_json::json!({
            "model": "gpt-image-1.5",
            "prompt": "a cat",
            "stream": false,
        })))
        .expect("valid request");
        let forwarded: Value = serde_json::from_slice(&prepared.body).expect("json");
        assert!(forwarded.get("stream").is_none());
    }

    #[test]
    fn unknown_fields_survive_untouched() {
        let prepared = prepare_generation(&body(serde_json::json!({
            "model": "gpt-image-1.5",
            "prompt": "a cat",
            "output_format": "webp",
            "output_compression": 80,
            "background": "transparent",
            "response_format": "b64_json",
        })))
        .expect("valid request");
        let forwarded: Value = serde_json::from_slice(&prepared.body).expect("json");
        assert_eq!(forwarded["output_format"], "webp");
        assert_eq!(forwarded["output_compression"], 80);
        assert_eq!(forwarded["background"], "transparent");
        assert_eq!(forwarded["response_format"], "b64_json");
    }

    #[test]
    fn the_error_envelope_is_openai_shaped() {
        let envelope = render_error(400, "request is missing a prompt");
        let value: Value = serde_json::from_slice(&envelope).expect("json");
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(value["error"]["message"], "request is missing a prompt");
    }
}

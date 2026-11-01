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
//! * `POST /v1/images/edits` accepts a multipart form and rewrites it to the
//!   JSON contract upstream uses (`codexRewriteOpenAIImageEditMultipartToJSON`).
//!   A form with no uploaded image is refused locally instead of spending an
//!   upstream call on a request the API cannot answer.
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
    /// A multipart edit body that is neither JSON nor a multipart form.
    UnsupportedContentType(String),
    /// A multipart body whose framing could not be read.
    MalformedMultipart,
    /// An edit with no uploaded image to edit.
    MissingImage,
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
            Self::UnsupportedContentType(content_type) => write!(
                formatter,
                "image edits need `multipart/form-data` with a boundary (got `{content_type}`)"
            ),
            Self::MalformedMultipart => {
                write!(formatter, "the multipart form could not be read")
            }
            Self::MissingImage => write!(
                formatter,
                "an image edit needs at least one uploaded `image` file"
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

/// One part of a `multipart/form-data` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartPart {
    /// The form field name (`Content-Disposition: form-data; name="..."`).
    pub name: String,
    /// The uploaded filename, when this part is a file rather than a text field.
    pub filename: Option<String>,
    /// The part's own `Content-Type`, when the client sent one.
    pub content_type: Option<String>,
    /// The raw part bytes.
    pub data: Vec<u8>,
}

/// Extract the boundary from a `multipart/form-data` Content-Type header.
///
/// Mirrors upstream's `mime.ParseMediaType` gate in
/// `codexPrepareDirectOpenAIImageEditPayload`: anything that is not
/// `multipart/*` or carries no boundary is refused by name instead of being
/// parsed on a guess.
pub fn multipart_boundary(content_type: &str) -> Option<String> {
    let (media_type, parameters) = content_type.split_once(';')?;
    if !media_type
        .trim()
        .to_ascii_lowercase()
        .starts_with("multipart/")
    {
        return None;
    }
    for parameter in parameters.split(';') {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        let value = value.trim().trim_matches('"');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Split a `multipart/form-data` body into its parts.
///
/// The body is already in memory (the bridge reads one bounded blob), so this
/// is a delimiter scan, not a streaming parser: upstream's `ReadForm` is the
/// equivalent step. Both `\r\n` and a leading boundary at byte 0 are accepted,
/// which is what every real client sends.
fn parse_multipart(body: &[u8], boundary: &str) -> Result<Vec<MultipartPart>, ImagesError> {
    let marker = format!("--{boundary}");
    let marker = marker.as_bytes();
    let mut parts = Vec::new();
    let mut cursor: Option<usize> = None;
    let mut index = 0_usize;
    while index + marker.len() <= body.len() {
        let at_marker = &body[index..index + marker.len()] == marker;
        let at_line_start =
            index == 0 || body.get(index.saturating_sub(2)..index) == Some(&b"\r\n"[..]);
        if !(at_marker && at_line_start) {
            index += 1;
            continue;
        }
        if let Some(start) = cursor {
            let end = index.saturating_sub(2).max(start);
            parts.push(multipart_part(&body[start..end])?);
        }
        let after = index + marker.len();
        if body.get(after..after + 2) == Some(b"--") {
            return Ok(parts);
        }
        let Some(line_end) = body[after..]
            .windows(2)
            .position(|window| window == b"\r\n")
        else {
            return Err(ImagesError::MalformedMultipart);
        };
        index = after + line_end + 2;
        cursor = Some(index);
    }
    Err(ImagesError::MalformedMultipart)
}

/// Parse one part's headers and payload.
fn multipart_part(raw: &[u8]) -> Result<MultipartPart, ImagesError> {
    let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(ImagesError::MalformedMultipart);
    };
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| ImagesError::MalformedMultipart)?;
    let mut name = String::new();
    let mut filename = None;
    let mut content_type = None;
    for line in head.split("\r\n") {
        let Some((header, value)) = line.split_once(':') else {
            continue;
        };
        if header.trim().eq_ignore_ascii_case("content-disposition") {
            for parameter in value.split(';') {
                let Some((key, parameter_value)) = parameter.split_once('=') else {
                    continue;
                };
                let parameter_value = parameter_value.trim().trim_matches('"');
                if key.trim().eq_ignore_ascii_case("name") {
                    name = parameter_value.to_string();
                } else if key.trim().eq_ignore_ascii_case("filename")
                    && !parameter_value.trim().is_empty()
                {
                    filename = Some(parameter_value.to_string());
                }
            }
        } else if header.trim().eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_string());
        }
    }
    Ok(MultipartPart {
        name,
        filename,
        content_type,
        data: raw[split + 4..].to_vec(),
    })
}

/// Validate an `images/edits` body and produce the upstream JSON body.
///
/// Upstream (`codexPrepareDirectOpenAIImageEditPayload`,
/// `codex_openai_images.go:556`) treats a JSON edit body exactly like a
/// generation body and rewrites the multipart form otherwise: text fields are
/// copied (with `n` / `output_compression` / `partial_images` coerced to
/// integers), the `mask` file becomes `mask.image_url`, and every `image` /
/// `image[]` file becomes an `images[]` entry holding a data URL. That is what
/// this function does, then [`prepare_generation`]'s model allowlist applies so
/// a typo cannot spend a different model.
pub fn prepare_edit(body: &[u8], content_type: &str) -> Result<ImageGeneration, ImagesError> {
    if serde_json::from_slice::<Value>(body).is_ok() {
        return prepare_generation(body);
    }
    let boundary = multipart_boundary(content_type)
        .ok_or_else(|| ImagesError::UnsupportedContentType(content_type.trim().to_string()))?;
    let parts = parse_multipart(body, &boundary)?;

    let mut fields: Vec<(String, String)> = Vec::new();
    let mut mask_file: Option<MultipartPart> = None;
    let mut image_files: Vec<MultipartPart> = Vec::new();
    let mut bracketed_images: Vec<MultipartPart> = Vec::new();
    for part in parts {
        if part.filename.is_some() {
            match part.name.as_str() {
                "mask" => mask_file = Some(part),
                "image[]" => bracketed_images.push(part),
                "image" => image_files.push(part),
                _ => {}
            }
            continue;
        }
        fields.push((part.name, String::from_utf8_lossy(&part.data).into_owned()));
    }
    // Upstream prefers `image[]` when a client sends both spellings.
    let image_files = if bracketed_images.is_empty() {
        image_files
    } else {
        bracketed_images
    };

    if fields
        .iter()
        .any(|(name, value)| name == "stream" && value.trim().eq_ignore_ascii_case("true"))
    {
        return Err(ImagesError::Streaming);
    }
    let field = |wanted: &str| {
        fields
            .iter()
            .find(|(name, value)| name == wanted && !value.trim().is_empty())
            .map(|(_, value)| value.trim().to_string())
    };
    let prompt = field("prompt").ok_or(ImagesError::MissingPrompt)?;
    let raw_model = field("model").ok_or(ImagesError::MissingModel)?;
    if image_files.is_empty() {
        return Err(ImagesError::MissingImage);
    }
    let model = normalise_model(&raw_model);
    if !DIRECT_IMAGE_MODELS.contains(&model.as_str()) {
        return Err(ImagesError::UnknownModel(raw_model));
    }

    let mut object = serde_json::Map::new();
    object.insert("model".to_string(), Value::String(model.clone()));
    object.insert("prompt".to_string(), Value::String(prompt));
    let mut seen: Vec<String> = vec![
        "model".to_string(),
        "prompt".to_string(),
        "stream".to_string(),
    ];
    for (name, _value) in &fields {
        if name.trim().is_empty() || seen.iter().any(|kept| kept == name) {
            continue;
        }
        let path = edit_field_path(name);
        let collected: Vec<&String> = fields
            .iter()
            .filter(|(candidate, _)| candidate == name)
            .map(|(_, value)| value)
            .collect();
        let value = if collected.len() == 1 {
            edit_field_value(name, collected[0])
        } else {
            Value::Array(
                collected
                    .iter()
                    .map(|value| edit_field_value(name, value))
                    .collect(),
            )
        };
        insert_field(&mut object, &path, value);
        seen.push(name.clone());
    }
    if let Some(mask) = mask_file {
        // A client may also send the mask by reference (`mask[image_url]`);
        // an actual upload wins, as it does upstream.
        object.insert(
            "mask".to_string(),
            serde_json::json!({ "image_url": data_url(&mask) }),
        );
    }
    let images: Vec<Value> = image_files
        .iter()
        .map(|image| serde_json::json!({ "image_url": data_url(image) }))
        .collect();
    object.insert("images".to_string(), Value::Array(images));

    Ok(ImageGeneration {
        model,
        body: serde_json::to_vec(&Value::Object(object)).unwrap_or_default(),
    })
}

/// Insert a form value at its (possibly dotted) JSON path, the shape
/// `sjson.SetBytes` produces upstream.
fn insert_field(object: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
    match path.split_once('.') {
        Some((head, tail)) => {
            let entry = object
                .entry(head.to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(serde_json::Map::new());
            }
            if let Value::Object(nested) = entry {
                insert_field(nested, tail, value);
            }
        }
        None => {
            object.insert(path.to_string(), value);
        }
    }
}

/// Upstream's form-key remapping (`codexOpenAIImageEditFormJSONPath`): the
/// mask can arrive as a reference rather than an upload.
fn edit_field_path(name: &str) -> String {
    match name.trim() {
        "mask[file_id]" => "mask.file_id".to_string(),
        "mask[image_url]" => "mask.image_url".to_string(),
        other => other.to_string(),
    }
}

/// Upstream coerces exactly these form fields to integers
/// (`codexOpenAIImageEditFormJSONValue`); everything else stays a string.
fn edit_field_value(name: &str, value: &str) -> Value {
    let value = value.trim();
    if matches!(name, "n" | "output_compression" | "partial_images") {
        if let Ok(parsed) = value.parse::<i64>() {
            return Value::Number(parsed.into());
        }
    }
    Value::String(value.to_string())
}

/// Render one uploaded file as a `data:` URL, the shape the direct images
/// endpoint accepts in `images[].image_url` / `mask.image_url`.
fn data_url(part: &MultipartPart) -> String {
    let media_type = part
        .content_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| detect_media_type(&part.data));
    use base64::Engine;
    format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&part.data)
    )
}

/// The subset of `http.DetectContentType` this door needs: image signatures
/// only, so a part without a Content-Type is still labelled correctly.
fn detect_media_type(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "image/gif"
    } else if data.len() > 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
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

    /// (name, filename, part Content-Type, payload)
    type FixturePart<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

    fn multipart(content_type: &str, parts: &[FixturePart<'_>]) -> Vec<u8> {
        let boundary = boundary_of(content_type);
        let mut body = Vec::new();
        for (name, filename, part_type, data) in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
                Some(filename) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n"
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
                ),
            }
            if let Some(part_type) = part_type {
                body.extend_from_slice(format!("Content-Type: {part_type}\r\n").as_bytes());
            }
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    fn boundary_of(content_type: &str) -> String {
        multipart_boundary(content_type).expect("fixture has a boundary")
    }

    const EDIT_CONTENT_TYPE: &str = "multipart/form-data; boundary=----clud-test";

    #[test]
    fn rewrites_a_multipart_edit_into_the_json_contract() {
        let body = multipart(
            EDIT_CONTENT_TYPE,
            &[
                ("model", None, None, b"gpt-image-1.5"),
                ("prompt", None, None, b"make it blue"),
                ("n", None, None, b"2"),
                ("size", None, None, b"1024x1024"),
                ("image", Some("in.png"), Some("image/png"), b"png-bytes"),
                ("mask", Some("mask.png"), None, b"mask-bytes"),
            ],
        );
        let prepared = prepare_edit(&body, EDIT_CONTENT_TYPE).expect("valid edit");
        assert_eq!(prepared.model, "gpt-image-1.5");
        let forwarded: Value = serde_json::from_slice(&prepared.body).expect("json");
        assert_eq!(forwarded["prompt"], "make it blue");
        assert_eq!(forwarded["n"], 2);
        assert_eq!(forwarded["size"], "1024x1024");
        assert_eq!(
            forwarded["images"][0]["image_url"],
            "data:image/png;base64,cG5nLWJ5dGVz"
        );
        // No part Content-Type: the PNG signature is sniffed, like upstream's
        // http.DetectContentType fallback.
        assert_eq!(
            forwarded["mask"]["image_url"],
            "data:application/octet-stream;base64,bWFzay1ieXRlcw=="
        );
        assert!(forwarded.get("stream").is_none());
    }

    #[test]
    fn prefers_bracketed_image_parts_when_both_are_present() {
        let body = multipart(
            EDIT_CONTENT_TYPE,
            &[
                ("model", None, None, b"gpt-image-2"),
                ("prompt", None, None, b"edit"),
                ("image", Some("a.png"), Some("image/png"), b"aaa"),
                ("image[]", Some("b.jpg"), Some("image/jpeg"), b"bbb"),
            ],
        );
        let forwarded: Value =
            serde_json::from_slice(&prepare_edit(&body, EDIT_CONTENT_TYPE).expect("ok").body)
                .expect("json");
        let images = forwarded["images"].as_array().expect("images array");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["image_url"], "data:image/jpeg;base64,YmJi");
    }

    #[test]
    fn accepts_a_json_edit_body_like_a_generation() {
        let prepared = prepare_edit(
            &body(serde_json::json!({
                "model": "gpt-image-1.5",
                "prompt": "edit it",
                "images": [{ "image_url": "https://example.test/a.png" }],
            })),
            "application/json",
        )
        .expect("json edits are forwarded");
        let forwarded: Value = serde_json::from_slice(&prepared.body).expect("json");
        assert_eq!(
            forwarded["images"][0]["image_url"],
            "https://example.test/a.png"
        );
    }

    #[test]
    fn refuses_a_body_that_is_neither_json_nor_multipart() {
        let error = prepare_edit(b"-----", "application/octet-stream").expect_err("must refuse");
        assert_eq!(
            error,
            ImagesError::UnsupportedContentType("application/octet-stream".to_string())
        );
        assert!(error.to_string().contains("multipart"));
    }

    #[test]
    fn refuses_multipart_without_a_boundary() {
        let error = prepare_edit(b"not json", "multipart/form-data").expect_err("must refuse");
        assert_eq!(
            error,
            ImagesError::UnsupportedContentType("multipart/form-data".to_string())
        );
    }

    #[test]
    fn refuses_a_truncated_multipart_body() {
        let error = prepare_edit(b"--x\r\nContent-Disposition: form-data", EDIT_CONTENT_TYPE)
            .expect_err("must refuse");
        assert_eq!(error, ImagesError::MalformedMultipart);
    }

    #[test]
    fn refuses_an_edit_without_an_uploaded_image() {
        let body = multipart(
            EDIT_CONTENT_TYPE,
            &[
                ("model", None, None, b"gpt-image-1.5"),
                ("prompt", None, None, b"edit"),
            ],
        );
        let error = prepare_edit(&body, EDIT_CONTENT_TYPE).expect_err("must refuse");
        assert_eq!(error, ImagesError::MissingImage);
        assert!(error.to_string().contains("image"));
    }

    #[test]
    fn refuses_an_edit_without_a_prompt() {
        let body = multipart(
            EDIT_CONTENT_TYPE,
            &[
                ("model", None, None, b"gpt-image-1.5"),
                ("image", Some("a.png"), Some("image/png"), b"aaa"),
            ],
        );
        assert_eq!(
            prepare_edit(&body, EDIT_CONTENT_TYPE).expect_err("must refuse"),
            ImagesError::MissingPrompt
        );
    }

    #[test]
    fn refuses_an_edit_with_an_unknown_model_and_names_it() {
        let body = multipart(
            EDIT_CONTENT_TYPE,
            &[
                ("model", None, None, b"dall-e-3"),
                ("prompt", None, None, b"edit"),
                ("image", Some("a.png"), Some("image/png"), b"aaa"),
            ],
        );
        let error = prepare_edit(&body, EDIT_CONTENT_TYPE).expect_err("must refuse");
        assert_eq!(error, ImagesError::UnknownModel("dall-e-3".to_string()));
    }

    #[test]
    fn refuses_a_streaming_edit_form() {
        let body = multipart(
            EDIT_CONTENT_TYPE,
            &[
                ("model", None, None, b"gpt-image-1.5"),
                ("prompt", None, None, b"edit"),
                ("stream", None, None, b"true"),
                ("image", Some("a.png"), Some("image/png"), b"aaa"),
            ],
        );
        assert_eq!(
            prepare_edit(&body, EDIT_CONTENT_TYPE).expect_err("must refuse"),
            ImagesError::Streaming
        );
    }

    #[test]
    fn repeated_form_values_become_an_array_and_mask_references_keep_their_path() {
        let body = multipart(
            EDIT_CONTENT_TYPE,
            &[
                ("model", None, None, b"gpt-image-1.5"),
                ("prompt", None, None, b"edit"),
                ("image", Some("a.png"), Some("image/png"), b"aaa"),
                ("mask[image_url]", None, None, b"https://example.test/m.png"),
                ("tags", None, None, b"one"),
                ("tags", None, None, b"two"),
            ],
        );
        let forwarded: Value =
            serde_json::from_slice(&prepare_edit(&body, EDIT_CONTENT_TYPE).expect("ok").body)
                .expect("json");
        assert_eq!(forwarded["tags"], serde_json::json!(["one", "two"]));
        assert_eq!(forwarded["mask"]["image_url"], "https://example.test/m.png");
    }

    #[test]
    fn the_multipart_boundary_parser_is_case_insensitive_and_quoted() {
        assert_eq!(
            multipart_boundary("Multipart/Form-Data; charset=utf-8; Boundary=\"abc123\""),
            Some("abc123".to_string())
        );
        assert_eq!(multipart_boundary("application/json"), None);
    }
}

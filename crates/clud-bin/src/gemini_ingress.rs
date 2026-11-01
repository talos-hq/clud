//! Anthropic Messages → Gemini `generateContent` request translation.
//!
//! This is the *egress* half of Gemini-protocol support: a Messages request the
//! rest of clud already speaks is rewritten into the body Google's
//! `generateContent` endpoint expects, so none of the routing, failover,
//! catalog or credential machinery has to learn a second protocol until a
//! Gemini upstream client exists.
//!
//! Ported from CLIProxyAPI
//! `internal/translator/gemini/claude/gemini_claude_request.go`
//! (`ConvertClaudeRequestToGemini`), plus its shared helpers
//! `internal/translator/common/{gemini,claude_system,parts}.go` and the default
//! safety list in `internal/translator/gemini/common/safety.go`.
//!
//! Deliberate differences, all documented rather than silent:
//!
//! * **`max_tokens` is honoured** (`generationConfig.maxOutputTokens`).
//!   Upstream drops it entirely; a Messages request always carries it, and
//!   silently ignoring a caller's cap is the kind of hidden rewrite clud avoids.
//! * **Unsupported content parts are a typed error naming the part**, not a
//!   silent drop. Upstream drops a `url`/`file` image source and any unknown
//!   block type; clud tells the caller its input did not arrive.
//! * **No `thought` part on the way in.** Upstream can replay an assistant
//!   `thinking` block only because it owns Gemini's thought-signature store.
//!   clud has no such store, and emitting a thought part without a valid
//!   signature makes Gemini reject the turn, so thinking history is dropped.
//! * **No Claude Code attribution stripping and no signature compat shims.**
//!   clud forwards what the caller sent and does not fingerprint requests.
//! * **`adaptive` thinking without an explicit effort becomes
//!   `thinkingLevel: high`.** Upstream asks its model registry for the target's
//!   maximum budget; clud has no Gemini catalog row yet (that is a separate
//!   slice), so it states the level instead of guessing a numeric budget.
//! * **JSON-Schema normalization is not ported.** `input_schema` is forwarded
//!   as `parametersJsonSchema` verbatim; upstream's `CleanJSONSchemaForGemini`
//!   (local `$ref` inlining, union flattening, enum/const coercions) is a named
//!   follow-up slice, not something to half-copy here.
//!
//! The *ingress* half lives further down: [`translate_generate_content_response`]
//! turns a Gemini `generateContent` body back into an Anthropic Messages body and
//! [`GeminiStreamTranslator`] does the same for the SSE stream, ported from
//! CLIProxyAPI `internal/translator/gemini/claude/gemini_claude_response.go`
//! (`ConvertGeminiResponseToClaude` / `ConvertGeminiResponseToClaudeNonStream`).
//! Its deliberate differences:
//!
//! * **Thought signatures are dropped, not replayed as empty carrier blocks.**
//!   Upstream emits a `thinking` block whose only content is a
//!   `signature_delta` so the *next* request can send the thought part back.
//!   clud's request half never replays thinking (see above), so such a block
//!   would reach the client as an empty `thinking` block with no value.
//! * **Tool names are not restored.** The request half sanitises
//!   `Read.File` -> `Read_File` and keeps no reverse map, so a response repeats
//!   the sanitised name it was given instead of inventing one.
//! * **`MALFORMED_FUNCTION_CALL` is a typed error** (`GeminiEgressError`), which
//!   the renderers turn into the same Anthropic error envelope upstream emits -
//!   never a silent `end_turn`.
//! * **Tool arguments are emitted whole** as one `input_json_delta` when a
//!   `functionCall` carries a non-empty object, rather than upstream's deferred
//!   empty-`{}` dance, because clud has no request-side tool declaration to
//!   consult for a parameterless tool.
//!
//! Nothing here changes the launch-scoped gateway contract: there is no route
//! and no lifecycle change, only a pure translator plus its hidden process seam.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

/// Upstream's default safety configuration. Gemini applies its own defaults
/// when the field is absent, and those are stricter than what a coding harness
/// needs, so the explicit list is part of the ported body.
const SAFETY_SETTINGS: [(&str, &str); 5] = [
    ("HARM_CATEGORY_HARASSMENT", "OFF"),
    ("HARM_CATEGORY_HATE_SPEECH", "OFF"),
    ("HARM_CATEGORY_SEXUALLY_EXPLICIT", "OFF"),
    ("HARM_CATEGORY_DANGEROUS_CONTENT", "OFF"),
    ("HARM_CATEGORY_CIVIC_INTEGRITY", "BLOCK_NONE"),
];

/// `clud __claude-gemini-translate` — the process seam behind the Python
/// subprocess test. Reads one Anthropic Messages body from stdin and writes the
/// translated `generateContent` body (compact JSON + newline) to stdout.
///
/// `model` overrides the target Gemini model id; without it the request's own
/// `model` is used. The seam is hidden and takes no session, backend,
/// credential or daemon action, so exercising the translator never starts a
/// harness.
pub fn run_translate_stdin(model: Option<&str>) -> i32 {
    use std::io::{Read, Write};

    let mut body = Vec::new();
    if std::io::stdin().read_to_end(&mut body).is_err() {
        eprintln!("could not read the request body from stdin");
        return 2;
    }
    match translate_messages_request(model.unwrap_or_default(), &body) {
        Ok(translated) => {
            let mut rendered = serde_json::to_vec(&translated).unwrap_or_default();
            rendered.push(b'\n');
            match std::io::stdout().write_all(&rendered) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("could not write the translated request: {error}");
                    2
                }
            }
        }
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}

/// Why a Messages request cannot be served as a `generateContent` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiIngressError {
    /// The body was not a JSON object.
    NotJson,
    /// No `model` string and no `--model` override, so the target is unknown.
    MissingModel,
    /// No usable `messages` array.
    EmptyMessages,
    /// A content part clud has no Gemini spelling for. Names the part.
    UnsupportedPart(String),
}

impl std::fmt::Display for GeminiIngressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson => write!(formatter, "the request body is not a JSON object"),
            Self::MissingModel => write!(formatter, "the request names no model"),
            Self::EmptyMessages => write!(formatter, "the request carries no messages"),
            Self::UnsupportedPart(part) => {
                write!(formatter, "cannot translate {part} to a Gemini part")
            }
        }
    }
}

impl std::error::Error for GeminiIngressError {}

/// Translate one Anthropic Messages request into a Gemini `generateContent`
/// request body.
///
/// `model` is the target Gemini model id; when empty the request's own `model`
/// is kept (upstream takes the resolved model name the same way).
pub fn translate_messages_request(model: &str, body: &[u8]) -> Result<Value, GeminiIngressError> {
    let parsed: Value = serde_json::from_slice(body).map_err(|_| GeminiIngressError::NotJson)?;
    let request = parsed.as_object().ok_or(GeminiIngressError::NotJson)?;

    let requested_model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let target_model = if model.is_empty() {
        requested_model
    } else {
        model
    };
    if target_model.is_empty() {
        return Err(GeminiIngressError::MissingModel);
    }
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
        .ok_or(GeminiIngressError::EmptyMessages)?;

    let mut translation = Translation::default();
    let contents = translation.contents(messages)?;

    let mut out = json!({ "model": target_model, "contents": contents });
    if let Some(instruction) = system_instruction(request.get("system")) {
        out["systemInstruction"] = instruction;
    }

    let (declarations, has_strict_tool) = tool_declarations(request.get("tools"));
    if let Some(declarations) = declarations {
        out["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    if let Some(config) = tool_config(
        request.get("tool_choice"),
        has_strict_tool && out.get("tools").is_some(),
    ) {
        out["toolConfig"] = config;
    }

    let generation = generation_config(request);
    if let Some(generation) = generation.as_object().filter(|map| !map.is_empty()) {
        out["generationConfig"] = Value::Object(generation.clone());
    }

    out["safetySettings"] = safety_settings();
    Ok(out)
}

fn safety_settings() -> Value {
    Value::Array(
        SAFETY_SETTINGS
            .iter()
            .map(|(category, threshold)| json!({ "category": category, "threshold": threshold }))
            .collect(),
    )
}

/// `system` is a string or an array of text blocks in the Messages request.
fn system_instruction(system: Option<&Value>) -> Option<Value> {
    let parts: Vec<Value> = match system? {
        Value::String(text) => text_parts(std::iter::once(text.as_str())),
        Value::Array(blocks) => text_parts(blocks.iter().filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })),
        _ => Vec::new(),
    };
    if parts.is_empty() {
        return None;
    }
    Some(json!({ "role": "user", "parts": parts }))
}

fn text_parts<'a>(texts: impl Iterator<Item = &'a str>) -> Vec<Value> {
    texts
        .filter(|text| !text.trim().is_empty())
        .map(|text| json!({ "text": text }))
        .collect()
}

/// Translators carry the tool-name map and the tool_use ids a following user
/// turn has to answer, which is what makes deterministic `tool_result` order
/// possible.
#[derive(Default)]
struct Translation {
    tool_names: HashMap<String, String>,
    pending_tool_ids: Vec<String>,
}

impl Translation {
    fn contents(&mut self, messages: &[Value]) -> Result<Vec<Value>, GeminiIngressError> {
        let mut contents: Vec<Value> = Vec::new();
        for message in messages {
            let Some(original_role) = message.get("role").and_then(Value::as_str) else {
                continue;
            };
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            if matches!(original_role, "system" | "developer") {
                if let Some(reminder) = system_reminder(&content) {
                    contents.push(gemini_content("user", vec![json!({ "text": reminder })]));
                }
                continue;
            }
            let preceding = if original_role == "user" {
                std::mem::take(&mut self.pending_tool_ids)
            } else {
                Vec::new()
            };
            let content = if preceding.is_empty() {
                content
            } else {
                align_tool_results(&content, &preceding)
            };
            let mut parts = self.parts(original_role, &content)?;
            if original_role == "user" {
                parts = reorder_user_parts(parts);
            }
            if parts.is_empty() {
                continue;
            }
            let role = if original_role == "assistant" {
                "model"
            } else {
                "user"
            };
            contents.push(gemini_content(role, parts));
        }
        strip_unanswered_model_turn(&mut contents);
        Ok(merge_adjacent_user_turns(contents))
    }

    fn parts(
        &mut self,
        original_role: &str,
        content: &Value,
    ) -> Result<Vec<Value>, GeminiIngressError> {
        match content {
            Value::String(text) => Ok(text_parts(std::iter::once(text.as_str()))),
            Value::Array(blocks) => {
                let mut parts = Vec::new();
                for block in blocks {
                    self.push_block(original_role, block, &mut parts)?;
                }
                Ok(parts)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn push_block(
        &mut self,
        original_role: &str,
        block: &Value,
        parts: &mut Vec<Value>,
    ) -> Result<(), GeminiIngressError> {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
            }
            // Upstream replays these only with a signature it owns; see the
            // module doc.
            "thinking" => {}
            "tool_use" => self.push_tool_use(original_role, block, parts)?,
            "tool_result" => self.push_tool_result(block, parts)?,
            "image" | "document" | "container_upload" => match inline_data(block.get("source")) {
                Some(part) => parts.push(part),
                None => {
                    return Err(GeminiIngressError::UnsupportedPart(describe_part(
                        block, kind,
                    )))
                }
            },
            other => {
                return Err(GeminiIngressError::UnsupportedPart(describe_part(
                    block, other,
                )))
            }
        }
        Ok(())
    }

    fn push_tool_use(
        &mut self,
        original_role: &str,
        block: &Value,
        parts: &mut Vec<Value>,
    ) -> Result<(), GeminiIngressError> {
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
        let args = match block.get("input") {
            Some(Value::Object(map)) => Value::Object(map.clone()),
            _ => match block.get("input") {
                None | Some(Value::Null) => json!({}),
                Some(_) => {
                    return Err(GeminiIngressError::UnsupportedPart(
                        "a tool_use whose input is not a JSON object".to_string(),
                    ))
                }
            },
        };
        if !id.is_empty() && !name.is_empty() {
            self.tool_names.insert(id.to_string(), name.to_string());
        }
        let mut call = Map::new();
        call.insert("name".to_string(), json!(sanitize_tool_name(name)));
        call.insert("args".to_string(), args);
        if !id.is_empty() {
            call.insert("id".to_string(), json!(id));
        }
        parts.push(json!({ "functionCall": Value::Object(call) }));
        if original_role == "assistant" && !id.is_empty() {
            self.pending_tool_ids.push(id.to_string());
        }
        Ok(())
    }

    fn push_tool_result(
        &mut self,
        block: &Value,
        parts: &mut Vec<Value>,
    ) -> Result<(), GeminiIngressError> {
        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
            return Ok(());
        };
        let name = self
            .tool_names
            .get(id)
            .cloned()
            .or_else(|| tool_name_from_id(id))
            .unwrap_or_else(|| id.to_string());
        let normalized = normalize_tool_result(block.get("content"));
        let mut response = Map::new();
        response.insert("id".to_string(), json!(id));
        response.insert("name".to_string(), json!(sanitize_tool_name(&name)));
        response.insert(
            "response".to_string(),
            json!({ "result": normalized.result }),
        );
        parts.push(json!({ "functionResponse": Value::Object(response) }));
        for image in normalized.images {
            parts.push(inline_data_part(&image.mime_type, &image.data));
        }
        Ok(())
    }
}

fn gemini_content(role: &str, parts: Vec<Value>) -> Value {
    json!({ "role": role, "parts": parts })
}

/// A mid-conversation `system`/`developer` message becomes an ordinary user
/// turn wrapped in the reminder envelope, so a non-Claude upstream treats it as
/// a directive rather than as user speech.
fn system_reminder(content: &Value) -> Option<String> {
    let texts: Vec<&str> = match content {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    let joined = texts
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if joined.trim().is_empty() {
        return None;
    }
    Some(format!("<system-reminder>\n{joined}\n</system-reminder>"))
}

fn describes_base64_image(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("image")
        && block
            .get("source")
            .and_then(|source| source.get("type"))
            .and_then(Value::as_str)
            == Some("base64")
}

fn inline_data(source: Option<&Value>) -> Option<Value> {
    let source = source?;
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let mime_type = source.get("media_type").and_then(Value::as_str)?;
    let data = source.get("data").and_then(Value::as_str)?;
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(inline_data_part(mime_type, data))
}

fn inline_data_part(mime_type: &str, data: &str) -> Value {
    json!({ "inline_data": { "mime_type": mime_type, "data": data } })
}

fn describe_part(block: &Value, kind: &str) -> String {
    let kind = if kind.is_empty() { "an untyped" } else { kind };
    match block
        .get("source")
        .and_then(|source| source.get("type"))
        .and_then(Value::as_str)
    {
        Some(source) => format!("a {kind} content part whose source is `{source}`"),
        None => format!("a {kind} content part"),
    }
}

struct NormalizedToolResult {
    result: Value,
    images: Vec<Image>,
}

struct Image {
    mime_type: String,
    data: String,
}

fn image_from_block(block: &Value) -> Option<Image> {
    let source = block.get("source")?;
    Some(Image {
        mime_type: source
            .get("media_type")
            .and_then(Value::as_str)?
            .to_string(),
        data: source.get("data").and_then(Value::as_str)?.to_string(),
    })
}

/// Claude's `tool_result.content` may be a string, a block array, a single
/// object or absent. Gemini wants one `result` value plus any images as their
/// own parts (base64 blobs inside the textual result bloat the response).
fn normalize_tool_result(content: Option<&Value>) -> NormalizedToolResult {
    let empty = NormalizedToolResult {
        result: json!(""),
        images: Vec::new(),
    };
    match content {
        Some(Value::String(text)) => NormalizedToolResult {
            result: json!(text),
            images: Vec::new(),
        },
        Some(Value::Array(blocks)) => {
            let mut images = Vec::new();
            let mut kept: Vec<Value> = Vec::new();
            for block in blocks {
                if describes_base64_image(block) {
                    if let Some(image) = image_from_block(block) {
                        images.push(image);
                    }
                    continue;
                }
                kept.push(block.clone());
            }
            let result = match kept.len() {
                0 => json!(""),
                1 => kept.remove(0),
                _ => Value::Array(kept),
            };
            NormalizedToolResult { result, images }
        }
        Some(Value::Object(map)) => {
            if describes_base64_image(content.unwrap_or(&Value::Null)) {
                let images = image_from_block(content.unwrap_or(&Value::Null))
                    .map(|image| vec![image])
                    .unwrap_or_default();
                return NormalizedToolResult {
                    result: json!(""),
                    images,
                };
            }
            NormalizedToolResult {
                result: Value::Object(map.clone()),
                images: Vec::new(),
            }
        }
        Some(Value::Null) | None => empty,
        Some(other) => NormalizedToolResult {
            result: other.clone(),
            images: Vec::new(),
        },
    }
}

/// Tools arrive as `{name, description, input_schema}`; Gemini wants
/// `functionDeclarations` with `parametersJsonSchema`.
fn tool_declarations(tools: Option<&Value>) -> (Option<Vec<Value>>, bool) {
    let Some(tools) = tools.and_then(Value::as_array) else {
        return (None, false);
    };
    let mut declarations = Vec::new();
    let mut has_strict_tool = false;
    for tool in tools {
        if tool.get("strict").and_then(Value::as_bool) == Some(true) {
            has_strict_tool = true;
        }
        let Some(schema) = tool.get("input_schema").filter(|schema| schema.is_object()) else {
            continue;
        };
        let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
        let mut declaration = Map::new();
        declaration.insert("name".to_string(), json!(sanitize_tool_name(name)));
        if let Some(description) = tool.get("description").and_then(Value::as_str) {
            declaration.insert("description".to_string(), json!(description));
        }
        declaration.insert("parametersJsonSchema".to_string(), schema.clone());
        declarations.push(Value::Object(declaration));
    }
    if declarations.is_empty() {
        return (None, has_strict_tool);
    }
    (Some(declarations), has_strict_tool)
}

/// `auto | none | any | tool` → a Gemini function-calling mode. A strict tool
/// without an explicit choice asks Gemini to validate the call (`VALIDATED`),
/// which is upstream's behaviour.
fn tool_config(choice: Option<&Value>, has_strict_tool: bool) -> Option<Value> {
    let (kind, name) = match choice {
        Some(Value::Object(map)) => (
            map.get("type").and_then(Value::as_str).unwrap_or_default(),
            map.get("name").and_then(Value::as_str).unwrap_or_default(),
        ),
        Some(Value::String(kind)) => (kind.as_str(), ""),
        _ if has_strict_tool => ("strict", ""),
        _ => return None,
    };
    let mut calling = Map::new();
    match kind {
        "auto" => {
            calling.insert(
                "mode".to_string(),
                json!(if has_strict_tool { "VALIDATED" } else { "AUTO" }),
            );
        }
        "none" => {
            calling.insert("mode".to_string(), json!("NONE"));
        }
        "any" => {
            calling.insert("mode".to_string(), json!("ANY"));
        }
        "tool" => {
            calling.insert("mode".to_string(), json!("ANY"));
            if !name.is_empty() {
                calling.insert(
                    "allowedFunctionNames".to_string(),
                    json!([sanitize_tool_name(name)]),
                );
            }
        }
        "strict" => {
            calling.insert("mode".to_string(), json!("VALIDATED"));
        }
        _ => return None,
    }
    Some(json!({ "functionCallingConfig": Value::Object(calling) }))
}

fn generation_config(request: &Map<String, Value>) -> Value {
    let mut config = Map::new();
    copy_number(request, "max_tokens", "maxOutputTokens", &mut config);
    copy_number(request, "temperature", "temperature", &mut config);
    copy_number(request, "top_p", "topP", &mut config);
    copy_number(request, "top_k", "topK", &mut config);
    if let Some(thinking) = thinking_config(request) {
        config.insert("thinkingConfig".to_string(), thinking);
    }
    Value::Object(config)
}

fn copy_number(
    request: &Map<String, Value>,
    from: &str,
    to: &str,
    config: &mut Map<String, Value>,
) {
    if let Some(number) = request.get(from).and_then(Value::as_f64) {
        config.insert(to.to_string(), json!(number));
    }
}

fn thinking_config(request: &Map<String, Value>) -> Option<Value> {
    let thinking = request.get("thinking")?.as_object()?;
    match thinking.get("type").and_then(Value::as_str)? {
        "enabled" => {
            let budget = thinking.get("budget_tokens").and_then(Value::as_u64)?;
            Some(json!({ "thinkingBudget": budget }))
        }
        "adaptive" | "auto" => {
            let effort = request
                .get("output_config")
                .and_then(|config| config.get("effort"))
                .and_then(Value::as_str)
                .map(|effort| effort.trim().to_ascii_lowercase())
                .filter(|effort| !effort.is_empty());
            Some(match effort {
                Some(effort) => json!({ "thinkingLevel": effort }),
                None => json!({ "thinkingLevel": "high" }),
            })
        }
        _ => None,
    }
}

/// A trailing model turn whose function calls nothing answers makes Gemini
/// reject the request, so it is dropped (upstream does the same).
fn strip_unanswered_model_turn(contents: &mut Vec<Value>) {
    let Some(last) = contents.last() else {
        return;
    };
    if last.get("role").and_then(Value::as_str) != Some("model") {
        return;
    }
    let has_call = last
        .get("parts")
        .and_then(Value::as_array)
        .is_some_and(|parts| parts.iter().any(|part| part.get("functionCall").is_some()));
    if has_call {
        contents.pop();
    }
}

/// Consecutive user turns merge; model turns never do, because merging them
/// would shift part indexes and break reasoning replay.
fn merge_adjacent_user_turns(contents: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for content in contents {
        let role = content
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let parts = content
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if parts.is_empty() {
            continue;
        }
        if role == "user" {
            if let Some(last) = merged.last_mut() {
                if last.get("role").and_then(Value::as_str) == Some("user") {
                    let mut combined = last
                        .get("parts")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    combined.extend(parts);
                    last["parts"] = Value::Array(reorder_user_parts(combined));
                    continue;
                }
            }
        }
        merged.push(gemini_content(role, parts));
    }
    merged
}

/// Gemini expects prompt text before `functionResponse` parts inside one user
/// turn (Vertex rejects a turn that ends with text after tool results).
fn reorder_user_parts(parts: Vec<Value>) -> Vec<Value> {
    let has_response = parts
        .iter()
        .any(|part| part.get("functionResponse").is_some());
    if !has_response {
        return parts;
    }
    let mut text: Vec<Value> = Vec::new();
    let mut rest: Vec<Value> = Vec::new();
    for part in parts {
        if part.get("text").is_some() {
            text.push(part);
        } else {
            rest.push(part);
        }
    }
    text.extend(rest);
    text
}

/// Order `tool_result` blocks by the ids the preceding assistant turn called,
/// leaving non-result blocks at their existing slots. Any mismatch leaves the
/// original order alone.
fn align_tool_results(content: &Value, tool_use_ids: &[String]) -> Value {
    let Some(blocks) = content.as_array() else {
        return content.clone();
    };
    let result_indexes: Vec<usize> = blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|(index, _)| index)
        .collect();
    if result_indexes.len() != tool_use_ids.len() {
        return content.clone();
    }
    let mut ordered: Vec<Option<Value>> = result_indexes
        .iter()
        .map(|index| Some(blocks[*index].clone()))
        .collect();
    let mut aligned: Vec<Value> = Vec::with_capacity(tool_use_ids.len());
    for id in tool_use_ids {
        let position = ordered.iter().position(|candidate| {
            candidate
                .as_ref()
                .and_then(|block| block.get("tool_use_id"))
                .and_then(Value::as_str)
                == Some(id)
        });
        match position {
            Some(position) => aligned.push(ordered[position].take().unwrap_or(Value::Null)),
            None => return content.clone(),
        }
    }
    let mut blocks = blocks.clone();
    for (slot, block) in result_indexes.iter().zip(aligned) {
        blocks[*slot] = block;
    }
    Value::Array(blocks)
}

/// Claude tool ids look like `toolu_...` or `<name>-<counter>`; upstream derives
/// the function name from everything before the last dash.
fn tool_name_from_id(id: &str) -> Option<String> {
    let (name, _) = id.rsplit_once('-')?;
    (!name.is_empty() && name != id).then(|| name.to_string())
}

/// Tool names must match `[a-zA-Z0-9_-]{1,64}`; MCP servers routinely hand over
/// names with dots or spaces.
fn sanitize_tool_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    sanitized.chars().take(64).collect()
}

// ---------------------------------------------------------------------------
// The ingress half: `generateContent` (JSON or SSE) -> Anthropic Messages
// ---------------------------------------------------------------------------

/// Why a Gemini response cannot be expressed as a Messages response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiEgressError {
    /// The body was not a JSON object.
    NotJson,
    /// Gemini stopped with `MALFORMED_FUNCTION_CALL` without producing a call,
    /// so there is no assistant content to hand back.
    MalformedFunctionCall,
}

impl std::fmt::Display for GeminiEgressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson => write!(formatter, "the response body is not a JSON object"),
            Self::MalformedFunctionCall => write!(
                formatter,
                "Gemini response finished with MALFORMED_FUNCTION_CALL"
            ),
        }
    }
}

impl std::error::Error for GeminiEgressError {}

/// The Anthropic-shaped error upstream returns for a malformed function call.
fn malformed_function_call_error() -> Value {
    json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": "Gemini response finished with MALFORMED_FUNCTION_CALL",
        }
    })
}

fn is_malformed_function_call(finish_reason: &str, saw_tool_call: bool) -> bool {
    finish_reason == "MALFORMED_FUNCTION_CALL" && !saw_tool_call
}

/// Gemini `finishReason` -> Anthropic `stop_reason`, upstream's table. A tool
/// call wins over any finish reason because the caller still has to run it.
pub fn stop_reason(finish_reason: &str, saw_tool_call: bool) -> &'static str {
    if saw_tool_call {
        return "tool_use";
    }
    match finish_reason {
        "MAX_TOKENS" => "max_tokens",
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "SPII" | "BLOCKLIST" | "IMAGE_SAFETY"
        | "MALFORMED_RESPONSE" => "refusal",
        _ => "end_turn",
    }
}

/// Token accounting from one `usageMetadata` object. Gemini counts the cached
/// prefix inside `promptTokenCount`, while Anthropic reports it separately, so
/// the cached count is subtracted from `input_tokens` and surfaced as
/// `cache_read_input_tokens` (upstream's arithmetic).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct GeminiUsage {
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
}

impl GeminiUsage {
    fn parse(chunk: &Value) -> Option<Self> {
        let usage = chunk.get("usageMetadata")?;
        let number = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
        let cached_tokens = number("cachedContentTokenCount");
        let prompt_tokens = number("promptTokenCount");
        let mut output_tokens = number("candidatesTokenCount") + number("thoughtsTokenCount");
        if output_tokens == 0 {
            let total = number("totalTokenCount");
            if total > 0 {
                output_tokens = total.saturating_sub(prompt_tokens);
            }
        }
        Some(Self {
            input_tokens: prompt_tokens.saturating_sub(cached_tokens),
            output_tokens,
            cached_tokens,
        })
    }

    fn to_json(self) -> Value {
        let mut usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
        });
        if self.cached_tokens > 0 {
            usage["cache_read_input_tokens"] = json!(self.cached_tokens);
        }
        usage
    }
}

/// `candidates[0].content.parts`, the only place Gemini puts content.
fn response_parts(chunk: &Value) -> &[Value] {
    chunk
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// `candidates[0].finishReason`, if this chunk closes the response.
fn finish_reason(chunk: &Value) -> Option<&str> {
    chunk
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("finishReason"))
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
}

fn string_field<'a>(chunk: &'a Value, name: &str) -> Option<&'a str> {
    chunk
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// A `tool_use` id in the shape clud's own `tool_name_from_id` can read back.
fn tool_use_id(name: &str, counter: u64) -> String {
    sanitize_tool_name(&format!("{name}-{counter}"))
}

fn flush_text(blocks: &mut Vec<Value>, text: &mut String) {
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": std::mem::take(text) }));
    }
}

fn flush_thinking(blocks: &mut Vec<Value>, thinking: &mut String) {
    if !thinking.is_empty() {
        blocks.push(json!({ "type": "thinking", "thinking": std::mem::take(thinking) }));
    }
}

fn tool_use_block(call: &Value, counter: u64) -> Value {
    let name = sanitize_tool_name(call.get("name").and_then(Value::as_str).unwrap_or_default());
    let input = call
        .get("args")
        .filter(|args| args.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    json!({
        "type": "tool_use",
        "id": tool_use_id(&name, counter),
        "name": name,
        "input": input,
    })
}

/// Translate one buffered Gemini `generateContent` response into an Anthropic
/// Messages response.
///
/// `requested_model` stands in when the body carries no `modelVersion`, so the
/// reply echoes what the client asked for.
pub fn translate_generate_content_response(
    body: &[u8],
    requested_model: Option<&str>,
) -> Result<Value, GeminiEgressError> {
    let chunk: Value = serde_json::from_slice(body).map_err(|_| GeminiEgressError::NotJson)?;
    if !chunk.is_object() {
        return Err(GeminiEgressError::NotJson);
    }

    let id = string_field(&chunk, "responseId").unwrap_or("msg_gemini");
    let model = string_field(&chunk, "modelVersion")
        .or(requested_model.filter(|model| !model.is_empty()))
        .unwrap_or_default();

    let mut blocks: Vec<Value> = Vec::new();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_counter = 0_u64;
    let mut saw_tool_call = false;

    for part in response_parts(&chunk) {
        if part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            flush_text(&mut blocks, &mut text);
            thinking.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
            continue;
        }
        if let Some(call) = part.get("functionCall").filter(|call| call.is_object()) {
            flush_thinking(&mut blocks, &mut thinking);
            flush_text(&mut blocks, &mut text);
            saw_tool_call = true;
            tool_counter += 1;
            blocks.push(tool_use_block(call, tool_counter));
            continue;
        }
        let piece = part.get("text").and_then(Value::as_str).unwrap_or_default();
        if piece.is_empty() {
            // A signature-only part: dropped by design, see the module docs.
            continue;
        }
        flush_thinking(&mut blocks, &mut thinking);
        text.push_str(piece);
    }
    flush_thinking(&mut blocks, &mut thinking);
    flush_text(&mut blocks, &mut text);

    let finish = finish_reason(&chunk).unwrap_or_default();
    if is_malformed_function_call(finish, saw_tool_call) {
        return Err(GeminiEgressError::MalformedFunctionCall);
    }

    let mut response = json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": blocks,
        "stop_reason": stop_reason(finish, saw_tool_call),
        "stop_sequence": null,
    });
    if let Some(usage) = GeminiUsage::parse(&chunk) {
        response["usage"] = usage.to_json();
    }
    Ok(response)
}

/// Which block the stream currently has open. Gemini chunks do not tell us when
/// a block ends, so the translator tracks it the way upstream's state machine
/// does: a part of a different kind closes the previous block first.
const BLOCK_NONE: u8 = 0;
const BLOCK_TEXT: u8 = 1;
const BLOCK_THINKING: u8 = 2;
const BLOCK_TOOL: u8 = 3;

/// Stateful translator for the streaming half: feed it the upstream SSE lines,
/// get Anthropic `message_start` / `content_block_*` / `message_delta` /
/// `message_stop` events back.
///
/// Every chunk carries a whole `parts` array, so the translator emits one delta
/// per part and closes a block only when the next part (or the end of the
/// stream) needs a different one - the same event order upstream produces.
pub struct GeminiStreamTranslator {
    model: String,
    message_id: String,
    index: u64,
    block: u8,
    started: bool,
    finished: bool,
    saw_tool_call: bool,
    finish_reason: String,
    usage: GeminiUsage,
    tool_counter: u64,
}

impl GeminiStreamTranslator {
    /// `model` is echoed in `message_start` until a chunk names a `modelVersion`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            message_id: "msg_gemini".to_string(),
            index: 0,
            block: BLOCK_NONE,
            started: false,
            finished: false,
            saw_tool_call: false,
            finish_reason: String::new(),
            usage: GeminiUsage::default(),
            tool_counter: 0,
        }
    }

    /// Feed one SSE line. Returns the events it produced (frequently none).
    ///
    /// `event:`/`id:`/`retry:` framing, comments, blank lines and the `[DONE]`
    /// sentinel produce nothing except that `[DONE]` seals the message; a
    /// `data:` line that is not JSON is ignored rather than aborting the stream
    /// the client is already reading.
    pub fn push_line(&mut self, line: &str) -> Vec<Value> {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with(':')
            || line.starts_with("event:")
            || line.starts_with("id:")
            || line.starts_with("retry:")
        {
            return Vec::new();
        }
        let Some(payload) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            return self.finish();
        }
        if payload.is_empty() {
            return Vec::new();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        self.push_chunk(&chunk)
    }

    /// Feed a whole SSE body and collect every event it produces, sealing the
    /// message at the end even when the upstream never sent `[DONE]`.
    pub fn push_body(&mut self, body: &[u8]) -> Vec<Value> {
        let body = String::from_utf8_lossy(body);
        let mut events: Vec<Value> = body.lines().flat_map(|line| self.push_line(line)).collect();
        events.extend(self.finish());
        events
    }

    /// Emit the closing events once. Idempotent, so `[DONE]`, an explicit
    /// `finishReason` and the end of the body cannot double-close the message.
    pub fn finish(&mut self) -> Vec<Value> {
        if self.finished || !self.started {
            return Vec::new();
        }
        let mut output = Vec::new();
        if is_malformed_function_call(&self.finish_reason, self.saw_tool_call) {
            output.push(malformed_function_call_error());
            self.finished = true;
            return output;
        }
        self.close_block(&mut output);
        output.push(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason(&self.finish_reason, self.saw_tool_call),
                "stop_sequence": null,
            },
            "usage": self.usage.to_json(),
        }));
        output.push(json!({ "type": "message_stop" }));
        self.finished = true;
        output
    }

    fn push_chunk(&mut self, chunk: &Value) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        let mut output = Vec::new();
        if !self.started {
            self.start_message(chunk, &mut output);
        }
        if let Some(usage) = GeminiUsage::parse(chunk) {
            self.usage = usage;
        }
        for part in response_parts(chunk) {
            if part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                self.push_thinking(
                    part.get("text").and_then(Value::as_str).unwrap_or_default(),
                    &mut output,
                );
                continue;
            }
            if let Some(call) = part.get("functionCall").filter(|call| call.is_object()) {
                self.push_tool_call(call, &mut output);
                continue;
            }
            let piece = part.get("text").and_then(Value::as_str).unwrap_or_default();
            if !piece.is_empty() {
                self.push_text(piece, &mut output);
            }
        }
        if let Some(reason) = finish_reason(chunk) {
            self.finish_reason = reason.to_string();
            output.extend(self.finish());
        }
        output
    }

    fn start_message(&mut self, chunk: &Value, output: &mut Vec<Value>) {
        if let Some(id) = string_field(chunk, "responseId") {
            self.message_id = id.to_string();
        }
        if let Some(model) = string_field(chunk, "modelVersion") {
            self.model = model.to_string();
        }
        output.push(json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            }
        }));
        self.started = true;
    }

    fn start_block(&mut self, kind: u8, block: Value, output: &mut Vec<Value>) {
        self.close_block(output);
        output.push(json!({
            "type": "content_block_start",
            "index": self.index,
            "content_block": block,
        }));
        self.block = kind;
    }

    fn close_block(&mut self, output: &mut Vec<Value>) {
        if self.block != BLOCK_NONE {
            output.push(json!({ "type": "content_block_stop", "index": self.index }));
            self.index += 1;
            self.block = BLOCK_NONE;
        }
    }

    fn push_text(&mut self, piece: &str, output: &mut Vec<Value>) {
        if self.block != BLOCK_TEXT {
            self.start_block(BLOCK_TEXT, json!({ "type": "text", "text": "" }), output);
        }
        output.push(json!({
            "type": "content_block_delta",
            "index": self.index,
            "delta": { "type": "text_delta", "text": piece },
        }));
    }

    fn push_thinking(&mut self, piece: &str, output: &mut Vec<Value>) {
        if self.block != BLOCK_THINKING {
            self.start_block(
                BLOCK_THINKING,
                json!({ "type": "thinking", "thinking": "" }),
                output,
            );
        }
        if !piece.is_empty() {
            output.push(json!({
                "type": "content_block_delta",
                "index": self.index,
                "delta": { "type": "thinking_delta", "thinking": piece },
            }));
        }
    }

    fn push_tool_call(&mut self, call: &Value, output: &mut Vec<Value>) {
        self.saw_tool_call = true;
        self.tool_counter += 1;
        let block = tool_use_block(call, self.tool_counter);
        let arguments = block
            .get("input")
            .filter(|input| input.as_object().is_some_and(|object| !object.is_empty()))
            .cloned();
        self.start_block(BLOCK_TOOL, block, output);
        if let Some(arguments) = arguments {
            output.push(json!({
                "type": "content_block_delta",
                "index": self.index,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": serde_json::to_string(&arguments).unwrap_or_default(),
                },
            }));
        }
    }
}

/// The bytes a served `POST /v1/messages` writes for a buffered Gemini response,
/// or the Anthropic error envelope for a malformed function call.
pub fn render_messages_body(model: &str, body: &[u8]) -> Vec<u8> {
    match translate_generate_content_response(body, Some(model)) {
        Ok(translated) => serde_json::to_vec(&translated).unwrap_or_default(),
        Err(error) => serde_json::to_vec(&malformed_error_for(&error)).unwrap_or_default(),
    }
}

fn malformed_error_for(error: &GeminiEgressError) -> Value {
    match error {
        GeminiEgressError::MalformedFunctionCall => malformed_function_call_error(),
        GeminiEgressError::NotJson => json!({
            "type": "error",
            "error": { "type": "api_error", "message": error.to_string() },
        }),
    }
}

/// The SSE bytes for a streamed Gemini response: one Anthropic event per frame.
pub fn render_messages_stream(model: &str, body: &[u8]) -> Vec<u8> {
    let mut translator = GeminiStreamTranslator::new(model);
    let mut rendered = Vec::new();
    for event in translator.push_body(body) {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        rendered.extend_from_slice(format!("event: {kind}\ndata: ").as_bytes());
        rendered.extend_from_slice(&serde_json::to_vec(&event).unwrap_or_default());
        rendered.extend_from_slice(b"\n\n");
    }
    rendered
}

/// `clud __claude-gemini-translate --response [--stream] [--http]` — the process
/// seam for the response half. Reads the upstream body on stdin; writes one
/// Messages JSON object per line (a single message, or one event per line for a
/// stream), or with `--http` the exact HTTP body the route writes. No session,
/// backend, credential or daemon action.
pub fn run_translate_response_stdin(
    stream: bool,
    requested_model: Option<&str>,
    http: bool,
) -> i32 {
    use std::io::{Read, Write};

    let mut body = Vec::new();
    if std::io::stdin().read_to_end(&mut body).is_err() {
        eprintln!("could not read the response body from stdin");
        return 2;
    }
    let model = requested_model.unwrap_or_default();
    let mut rendered = Vec::new();
    if http {
        if stream {
            rendered = render_messages_stream(model, &body);
        } else {
            rendered = render_messages_body(model, &body);
        }
    } else if stream {
        let mut translator = GeminiStreamTranslator::new(model);
        for event in translator.push_body(&body) {
            match serde_json::to_vec(&event) {
                Ok(mut encoded) => {
                    encoded.push(b'\n');
                    rendered.extend_from_slice(&encoded);
                }
                Err(error) => {
                    eprintln!("could not encode a translated event: {error}");
                    return 2;
                }
            }
        }
    } else {
        match translate_generate_content_response(&body, requested_model) {
            Ok(translated) => match serde_json::to_vec(&translated) {
                Ok(mut encoded) => {
                    encoded.push(b'\n');
                    rendered.extend_from_slice(&encoded);
                }
                Err(error) => {
                    eprintln!("could not encode the translated response: {error}");
                    return 2;
                }
            },
            Err(error) => {
                eprintln!("{error}");
                return 2;
            }
        }
    }
    match std::io::stdout().write_all(&rendered) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("could not write the translated response: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(body: Value) -> Value {
        translate_messages_request("gemini-2.5-pro", body.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn system_prompt_and_text_turns_map_to_contents() {
        let out = translate(json!({
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "be terse"}],
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
                {"role": "user", "content": [{"type": "text", "text": "again"}]}
            ]
        }));
        assert_eq!(out["model"], "gemini-2.5-pro");
        assert_eq!(out["systemInstruction"]["role"], "user");
        assert_eq!(out["systemInstruction"]["parts"][0]["text"], "be terse");
        assert_eq!(out["contents"][0]["role"], "user");
        assert_eq!(out["contents"][0]["parts"][0]["text"], "hello");
        assert_eq!(out["contents"][1]["role"], "model");
        assert_eq!(out["contents"][2]["parts"][0]["text"], "again");
        assert_eq!(out["generationConfig"]["maxOutputTokens"], 1024.0);
        assert_eq!(out["safetySettings"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn system_string_is_accepted_and_empty_system_is_omitted() {
        let out = translate(json!({
            "model": "m",
            "system": "policy",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "q"}]}]
        }));
        assert_eq!(out["systemInstruction"]["parts"][0]["text"], "policy");
        let absent = translate(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "q"}]}]
        }));
        assert!(absent.get("systemInstruction").is_none());
    }

    #[test]
    fn base64_images_become_inline_data_and_urls_are_refused() {
        let out = translate(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}}
            ]}]
        }));
        let parts = out["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
        assert_eq!(parts[1]["inline_data"]["data"], "AAA");

        let error = translate_messages_request(
            "gemini-2.5-pro",
            json!({
                "model": "m",
                "messages": [{"role": "user", "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.test/a.png"}}
                ]}]
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap_err();
        assert!(
            matches!(error, GeminiIngressError::UnsupportedPart(ref part) if part.contains("url"))
        );
    }

    #[test]
    fn tool_use_and_result_round_trip_with_the_call_name() {
        let out = translate(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "read a file"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read.File", "input": {"path": "a.rs"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "text", "text": "contents"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "BBB"}}
                    ]}
                ]}
            ]
        }));
        let contents = out["contents"].as_array().unwrap();
        let call = &contents[1]["parts"][0]["functionCall"];
        assert_eq!(call["name"], "Read_File");
        assert_eq!(call["id"], "toolu_1");
        assert_eq!(call["args"]["path"], "a.rs");
        let response = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(response["name"], "Read_File");
        assert_eq!(response["id"], "toolu_1");
        assert_eq!(response["response"]["result"]["text"], "contents");
        assert_eq!(contents[2]["parts"][1]["inline_data"]["data"], "BBB");
    }

    #[test]
    fn tools_and_choice_map_to_function_declarations() {
        let out = translate(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "q"}],
            "tools": [{
                "name": "Read.File",
                "description": "read",
                "strict": true,
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            }],
            "tool_choice": {"type": "tool", "name": "Read.File"}
        }));
        let declaration = &out["tools"][0]["functionDeclarations"][0];
        assert_eq!(declaration["name"], "Read_File");
        assert_eq!(declaration["description"], "read");
        assert_eq!(declaration["parametersJsonSchema"]["type"], "object");
        assert!(declaration.get("strict").is_none());
        assert_eq!(out["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(
            out["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "Read_File"
        );
    }

    #[test]
    fn strict_tools_without_a_choice_ask_for_validation() {
        let out = translate(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "q"}],
            "tools": [{"name": "t", "strict": true, "input_schema": {"type": "object"}}]
        }));
        assert_eq!(
            out["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
    }

    #[test]
    fn thinking_maps_to_a_budget_or_a_level() {
        let enabled = translate(json!({
            "model": "m",
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "messages": [{"role": "user", "content": "q"}]
        }));
        assert_eq!(
            enabled["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4096
        );
        let adaptive = translate(json!({
            "model": "m",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "LOW"},
            "messages": [{"role": "user", "content": "q"}]
        }));
        assert_eq!(
            adaptive["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
        let default_adaptive = translate(json!({
            "model": "m",
            "thinking": {"type": "adaptive"},
            "messages": [{"role": "user", "content": "q"}]
        }));
        assert_eq!(
            default_adaptive["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    #[test]
    fn an_unanswered_trailing_model_turn_is_stripped() {
        let out = translate(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_9", "name": "t", "input": {}}
                ]}
            ]
        }));
        assert_eq!(out["contents"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn adjacent_user_turns_merge_and_blank_text_is_dropped() {
        let out = translate(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "one"}]},
                {"role": "user", "content": [{"type": "text", "text": "two"}]},
                {"role": "user", "content": [{"type": "text", "text": "   "}]}
            ]
        }));
        let contents = out["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_mid_conversation_system_message_becomes_a_reminder_turn() {
        let out = translate(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "system", "content": [{"type": "text", "text": "stay in scope"}]}
            ]
        }));
        // The reminder turn is a user turn, so it merges into the preceding one.
        let parts = out["contents"][0]["parts"].as_array().unwrap();
        let text = parts[1]["text"].as_str().unwrap();
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.contains("stay in scope"));
    }

    #[test]
    fn malformed_requests_are_refused_by_name() {
        let not_json = translate_messages_request("x", b"nope").unwrap_err();
        assert_eq!(not_json, GeminiIngressError::NotJson);
        let not_object = translate_messages_request("x", b"[]").unwrap_err();
        assert_eq!(not_object, GeminiIngressError::NotJson);
        let no_model =
            translate_messages_request("", br#"{"messages":[{"role":"user","content":"q"}]}"#)
                .unwrap_err();
        assert_eq!(no_model, GeminiIngressError::MissingModel);
        let no_messages =
            translate_messages_request("m", br#"{"model":"x","messages":[]}"#).unwrap_err();
        assert_eq!(no_messages, GeminiIngressError::EmptyMessages);
    }

    #[test]
    fn an_empty_model_argument_keeps_the_requested_model() {
        let out = translate_messages_request(
            "",
            br#"{"model":"claude-sonnet-4","messages":[{"role":"user","content":"q"}]}"#,
        )
        .unwrap();
        assert_eq!(out["model"], "claude-sonnet-4");
    }

    fn gemini_response(body: &str) -> Value {
        translate_generate_content_response(body.as_bytes(), None).unwrap()
    }

    #[test]
    fn a_gemini_response_becomes_a_messages_response() {
        let out = gemini_response(
            r#"{"responseId":"resp_1","modelVersion":"gemini-3-pro","candidates":[{"content":{"parts":[{"text":"he"},{"text":"llo"},{"functionCall":{"name":"Read.File","args":{"path":"a.rs"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":10,"thoughtsTokenCount":5}}"#,
        );
        assert_eq!(out["id"], "resp_1");
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "gemini-3-pro");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "hello");
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["content"][1]["name"], "Read_File");
        assert_eq!(out["content"][1]["input"]["path"], "a.rs");
        assert_eq!(out["content"][1]["id"], "Read_File-1");
        assert_eq!(out["stop_reason"], "tool_use");
        // Gemini folds the cached prefix into promptTokenCount; Anthropic does not.
        assert_eq!(out["usage"]["input_tokens"], 60);
        assert_eq!(out["usage"]["output_tokens"], 15);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 40);
    }

    #[test]
    fn thought_parts_become_thinking_blocks() {
        let out = gemini_response(
            r#"{"candidates":[{"content":{"parts":[{"text":"weigh it","thought":true},{"text":"answer"}]},"finishReason":"STOP"}]}"#,
        );
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "weigh it");
        assert_eq!(out["content"][1]["type"], "text");
        assert_eq!(out["content"][1]["text"], "answer");
    }

    #[test]
    fn a_signature_only_part_is_dropped() {
        let out = gemini_response(
            r#"{"candidates":[{"content":{"parts":[{"text":"a"},{"thoughtSignature":"c2ln"},{"text":"b"}]},"finishReason":"STOP"}]}"#,
        );
        assert_eq!(out["content"][0]["text"], "ab");
        assert_eq!(out["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn finish_reasons_map_to_the_anthropic_table() {
        assert_eq!(stop_reason("STOP", false), "end_turn");
        assert_eq!(stop_reason("", false), "end_turn");
        assert_eq!(stop_reason("MAX_TOKENS", false), "max_tokens");
        assert_eq!(stop_reason("SAFETY", false), "refusal");
        assert_eq!(stop_reason("MALFORMED_RESPONSE", false), "refusal");
        assert_eq!(stop_reason("STOP", true), "tool_use");
        assert_eq!(stop_reason("MAX_TOKENS", true), "tool_use");
    }

    #[test]
    fn a_response_without_a_model_echoes_the_requested_one() {
        let out = translate_generate_content_response(
            br#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"STOP"}]}"#,
            Some("clud-claude-gemini-3-pro"),
        )
        .unwrap();
        assert_eq!(out["model"], "clud-claude-gemini-3-pro");
        assert_eq!(out["id"], "msg_gemini");
        assert_eq!(out["usage"], Value::Null);
    }

    #[test]
    fn a_malformed_function_call_is_a_typed_error() {
        let error = translate_generate_content_response(
            br#"{"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#,
            Some("m"),
        )
        .unwrap_err();
        assert_eq!(error, GeminiEgressError::MalformedFunctionCall);
        let rendered = render_messages_body(
            "m",
            br#"{"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#,
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&rendered).unwrap()["error"]["type"],
            "api_error"
        );
        let not_json = translate_generate_content_response(b"nope", None).unwrap_err();
        assert_eq!(not_json, GeminiEgressError::NotJson);
    }

    fn stream_kinds(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    const STREAM_BODY: &str = concat!(
        "data: {\"responseId\":\"resp_2\",\"modelVersion\":\"gemini-3-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"Bash\",\"args\":{\"command\":\"ls\"}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":4}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn a_streamed_response_emits_anthropic_events_in_order() {
        let mut translator = GeminiStreamTranslator::new("gemini-3-pro");
        let events = translator.push_body(STREAM_BODY.as_bytes());
        let kinds = stream_kinds(&events);
        assert_eq!(kinds[0], "message_start");
        assert_eq!(events[0]["message"]["id"], "resp_2");
        assert_eq!(events[0]["message"]["model"], "gemini-3-pro");
        assert_eq!(kinds.last().map(String::as_str), Some("message_stop"));
        // The trailing [DONE] must not seal the message a second time.
        assert_eq!(
            kinds.iter().filter(|kind| *kind == "message_delta").count(),
            1
        );
        let text: String = events
            .iter()
            .filter(|event| event["delta"]["type"] == "text_delta")
            .map(|event| event["delta"]["text"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(text, "hi there");
        let tool_start = events
            .iter()
            .find(|event| event["content_block"]["type"] == "tool_use")
            .expect("a tool_use block");
        assert_eq!(tool_start["content_block"]["name"], "Bash");
        let partial: Vec<&Value> = events
            .iter()
            .filter(|event| event["delta"]["type"] == "input_json_delta")
            .collect();
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0]["delta"]["partial_json"], "{\"command\":\"ls\"}");
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("a message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
        assert_eq!(delta["usage"]["input_tokens"], 10);
        assert_eq!(delta["usage"]["output_tokens"], 4);
        // Every opened block is closed exactly once.
        let starts = kinds
            .iter()
            .filter(|kind| *kind == "content_block_start")
            .count();
        let stops = kinds
            .iter()
            .filter(|kind| *kind == "content_block_stop")
            .count();
        assert_eq!(starts, stops);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| *kind == "content_block_start")
                .count(),
            2
        );
    }

    #[test]
    fn a_stream_without_done_is_sealed_at_the_end_of_the_body() {
        let mut translator = GeminiStreamTranslator::new("m");
        let events =
            translator.push_body(br#"data: {"candidates":[{"content":{"parts":[{"text":"x"}]}}]}"#);
        let kinds = stream_kinds(&events);
        assert_eq!(kinds.last().map(String::as_str), Some("message_stop"));
        assert_eq!(events.len(), 6);
        // A second finish (a late [DONE]) is a no-op.
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn a_malformed_stream_ends_with_an_error_event() {
        let mut translator = GeminiStreamTranslator::new("m");
        let events = translator.push_body(
            br#"data: {"candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#,
        );
        let kinds = stream_kinds(&events);
        assert_eq!(kinds.last().map(String::as_str), Some("error"));
        assert!(!kinds.iter().any(|kind| kind == "message_delta"));
        assert_eq!(events.last().unwrap()["error"]["type"], "api_error");
    }

    #[test]
    fn rendered_sse_frames_name_their_event() {
        let rendered = String::from_utf8(render_messages_stream(
            "gemini-3-pro",
            STREAM_BODY.as_bytes(),
        ))
        .expect("utf-8");
        assert!(rendered.starts_with("event: message_start\ndata: {"));
        assert!(rendered.contains("event: content_block_delta\ndata: {\"delta\""));
        assert!(rendered.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }
}

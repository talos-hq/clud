//! OpenAI Chat Completions → Anthropic Messages request translation.
//!
//! This is the ingress half of the OpenAI-compatible gateway surface: an
//! OpenAI-shaped client (`POST /v1/chat/completions`) is rewritten into the
//! Messages request the rest of clud already speaks, so none of the routing,
//! failover, catalog or credential machinery has to learn a second protocol.
//!
//! Ported from CLIProxyAPI
//! `internal/translator/claude/openai/chat-completions/claude_openai_request.go`
//! (`ConvertOpenAIRequestToClaude`). Deliberate differences, all additive:
//!
//! * `metadata.user_id` derivation is not ported — clud forwards what the
//!   caller sent and does not fingerprint the request.
//! * `response_format` is not turned into a synthesized system instruction:
//!   silently injecting prompt text is the kind of hidden payload rewrite
//!   clud avoids.
//! * Unknown/unsupported content parts (audio, uploaded files) are a typed
//!   error naming the part instead of being dropped, so a client learns its
//!   input did not arrive.
//!
//! The response half (`Anthropic Messages JSON/SSE → OpenAI`) lives further down
//! in [`translate_chat_response`] and [`ChatStreamTranslator`], ported from
//! CLIProxyAPI `.../claude_openai_response.go`. [`render_chat_body`],
//! [`render_chat_stream`] and [`render_chat_error`] are the bytes the
//! `POST /v1/chat/completions` route puts on the wire. Nothing here changes the
//! launch-scoped gateway contract: the route is additive beside it.

use serde_json::{json, Map, Value};

/// Upstream's default when the client omits both `max_tokens` and
/// `max_completion_tokens`. Messages requests must always carry the field.
pub const DEFAULT_MAX_TOKENS: u64 = 32_000;

/// Longest tool name Anthropic accepts.
const MAX_TOOL_NAME: usize = 64;

/// `clud __openai-chat-translate` — the process seam behind the Python
/// subprocess test. Reads one OpenAI-shaped body from stdin and writes the
/// translated Messages body (compact JSON + newline) to stdout.
///
/// It is hidden and takes no session, backend, credential or daemon action, so
/// exercising the translator never starts a harness.
pub fn run_translate_stdin() -> i32 {
    use std::io::{Read, Write};

    let mut body = Vec::new();
    if std::io::stdin().read_to_end(&mut body).is_err() {
        eprintln!("could not read the request body from stdin");
        return 2;
    }
    match translate_chat_request(&body) {
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

/// Why an OpenAI-shaped request cannot be served as a Messages request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressError {
    /// The body was not a JSON object.
    NotJson,
    /// No `model` string, so no route can be chosen.
    MissingModel,
    /// No usable `messages` (and no system prompt to stand in for one).
    EmptyMessages,
    /// A content part clud has no Anthropic spelling for. Names the part.
    UnsupportedPart(String),
}

impl std::fmt::Display for IngressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson => formatter.write_str("request body is not a JSON object"),
            Self::MissingModel => formatter.write_str("request is missing a model"),
            Self::EmptyMessages => formatter.write_str("request contains no messages"),
            Self::UnsupportedPart(part) => write!(
                formatter,
                "content part `{part}` has no Anthropic equivalent on this gateway"
            ),
        }
    }
}

impl std::error::Error for IngressError {}

/// Translate one OpenAI Chat Completions request body into an Anthropic
/// Messages request body.
///
/// `model` is passed through verbatim: resolution against clud's catalog,
/// allowlists and effort suffixes stays with the gateway that already owns it.
pub fn translate_chat_request(body: &[u8]) -> Result<Value, IngressError> {
    let request: Value = serde_json::from_slice(body).map_err(|_| IngressError::NotJson)?;
    let request = request.as_object().ok_or(IngressError::NotJson)?;

    let model = request
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or(IngressError::MissingModel)?;

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model.to_string()));
    out.insert(
        "max_tokens".into(),
        json!(max_tokens(request).unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    if let Some(top_p) = request.get("top_p").and_then(Value::as_f64) {
        out.insert("top_p".into(), json!(top_p));
    }
    if let Some(temperature) = request.get("temperature").and_then(Value::as_f64) {
        out.insert("temperature".into(), json!(temperature));
    }
    if let Some(stop) = stop_sequences(request.get("stop")) {
        out.insert("stop_sequences".into(), stop);
    }
    out.insert(
        "stream".into(),
        json!(request.get("stream").and_then(Value::as_bool) == Some(true)),
    );
    if let Some(effort) = reasoning_effort(request.get("reasoning_effort")) {
        out.insert("output_config".into(), json!({ "effort": effort }));
    }

    let messages = match request.get("messages").and_then(Value::as_array) {
        Some(messages) => messages.as_slice(),
        None => return Err(IngressError::EmptyMessages),
    };

    let mut system: Vec<Value> = Vec::new();
    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    for message in messages {
        let message = message.as_object().ok_or(IngressError::NotJson)?;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or(IngressError::NotJson)?;
        match role {
            "system" | "developer" => {
                // Developer messages rank with system messages in OpenAI's
                // instruction hierarchy; dropping them loses operator intent.
                for block in text_blocks(message.get("content"))? {
                    system.push(block);
                }
            }
            "user" | "assistant" => {
                let mut blocks = content_blocks(message.get("content"))?;
                if role == "assistant" {
                    blocks.extend(tool_use_blocks(message.get("tool_calls"))?);
                }
                if !blocks.is_empty() {
                    push_turn(&mut turns, role, blocks);
                }
            }
            "tool" => {
                let block = tool_result_block(message)?;
                // Anthropic requires alternating roles, so consecutive tool
                // results coalesce into a single user turn.
                push_turn(&mut turns, "user", vec![block]);
            }
            other => return Err(IngressError::UnsupportedPart(format!("role:{other}"))),
        }
    }

    // A system-only request still needs one conversational turn: an empty
    // `messages` array is not a Messages request.
    if turns.is_empty() {
        if system.is_empty() {
            return Err(IngressError::EmptyMessages);
        }
        turns.push(("user".into(), vec![json!({ "type": "text", "text": "" })]));
    }

    if !system.is_empty() {
        out.insert("system".into(), Value::Array(system));
    }
    out.insert(
        "messages".into(),
        Value::Array(
            turns
                .into_iter()
                .map(|(role, content)| json!({ "role": role, "content": content }))
                .collect(),
        ),
    );

    let tools = tool_definitions(request.get("tools"))?;
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = tool_choice(request.get("tool_choice")) {
            out.insert("tool_choice".into(), choice);
        }
        if request.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false) {
            let choice = out
                .entry("tool_choice")
                .or_insert_with(|| json!({ "type": "auto" }));
            if choice.get("type").and_then(Value::as_str) != Some("none") {
                choice["disable_parallel_tool_use"] = json!(true);
            }
        }
    }

    Ok(Value::Object(out))
}

/// Merge a new turn into the accumulated alternation.
///
/// Two consecutive messages with the same role are one Anthropic turn; keeping
/// them separate is rejected by the Messages API, and a tool result always
/// follows its assistant turn.
fn push_turn(turns: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    match turns.last_mut() {
        Some((previous, content)) if previous == role => content.extend(blocks),
        _ => turns.push((role.to_string(), blocks)),
    }
}

fn max_tokens(request: &Map<String, Value>) -> Option<u64> {
    // OpenAI deprecated `max_tokens` in favour of `max_completion_tokens`, so
    // both spellings are requests for the same Anthropic field.
    request
        .get("max_completion_tokens")
        .or_else(|| request.get("max_tokens"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
}

fn stop_sequences(stop: Option<&Value>) -> Option<Value> {
    match stop? {
        Value::String(single) => Some(json!([single])),
        Value::Array(items) => {
            let sequences: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            (!sequences.is_empty()).then(|| json!(sequences))
        }
        _ => None,
    }
}

/// OpenAI `reasoning_effort` is clud's session-wide effort spelling. `none`
/// disables thinking by omitting the block, matching the catalog's `none`.
fn reasoning_effort(effort: Option<&Value>) -> Option<String> {
    let effort = effort?.as_str()?.trim().to_lowercase();
    match effort.as_str() {
        "" | "none" => None,
        _ => Some(effort),
    }
}

fn text_blocks(content: Option<&Value>) -> Result<Vec<Value>, IngressError> {
    let Some(content) = content else {
        return Ok(Vec::new());
    };
    match content {
        Value::String(text) => Ok(text_block(text)),
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                let part = part.as_object().ok_or(IngressError::NotJson)?;
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        let block = text_block(text);
                        blocks.extend(block);
                    }
                    Some("image_url") => {
                        return Err(IngressError::UnsupportedPart("image_url".into()));
                    }
                    Some(other) => return Err(IngressError::UnsupportedPart(other.into())),
                    None => return Err(IngressError::NotJson),
                }
            }
            Ok(blocks)
        }
        _ => Err(IngressError::NotJson),
    }
}

fn text_block(text: &str) -> Vec<Value> {
    // Blank text is not a content block: it would create an empty turn the
    // Messages API rejects.
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": text })]
    }
}

fn content_blocks(content: Option<&Value>) -> Result<Vec<Value>, IngressError> {
    let Some(content) = content else {
        return Ok(Vec::new());
    };
    match content {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(text_block(text)),
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                let part = part.as_object().ok_or(IngressError::NotJson)?;
                let kind = part
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or(IngressError::NotJson)?;
                match kind {
                    "text" => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        blocks.extend(text_block(text));
                    }
                    "image_url" => blocks.push(image_block(part)?),
                    other => return Err(IngressError::UnsupportedPart(other.into())),
                }
            }
            Ok(blocks)
        }
        _ => Err(IngressError::NotJson),
    }
}

/// `image_url.url` is either a `data:` URL (base64 inline) or an https URL,
/// which is exactly Anthropic's two image sources.
fn image_block(part: &Map<String, Value>) -> Result<Value, IngressError> {
    let url = part
        .get("image_url")
        .and_then(|image| image.get("url"))
        .and_then(Value::as_str)
        .ok_or_else(|| IngressError::UnsupportedPart("image_url".into()))?;
    let block = if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest
            .split_once(',')
            .ok_or_else(|| IngressError::UnsupportedPart("image_url".into()))?;
        if !meta.ends_with(";base64") {
            return Err(IngressError::UnsupportedPart("image_url".into()));
        }
        json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": meta.trim_end_matches(";base64"),
                "data": data,
            }
        })
    } else if url.starts_with("http://") || url.starts_with("https://") {
        json!({ "type": "image", "source": { "type": "url", "url": url } })
    } else {
        return Err(IngressError::UnsupportedPart("image_url".into()));
    };
    Ok(block)
}

fn tool_use_blocks(tool_calls: Option<&Value>) -> Result<Vec<Value>, IngressError> {
    let Some(tool_calls) = tool_calls else {
        return Ok(Vec::new());
    };
    let tool_calls = tool_calls.as_array().ok_or(IngressError::NotJson)?;
    let mut blocks = Vec::new();
    for call in tool_calls {
        let call = call.as_object().ok_or(IngressError::NotJson)?;
        if call.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        let function = call
            .get("function")
            .and_then(Value::as_object)
            .ok_or(IngressError::NotJson)?;
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| IngressError::UnsupportedPart("tool_calls.id".into()))?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or(IngressError::NotJson)?;
        blocks.push(json!({
            "type": "tool_use",
            "id": id,
            "name": sanitize_tool_name(name),
            "input": tool_arguments(function.get("arguments")),
        }));
    }
    Ok(blocks)
}

/// OpenAI carries tool arguments as a JSON *string*; Anthropic wants the
/// object. Malformed or non-object arguments become `{}`, which is what the
/// harness would have sent for a call with no arguments.
fn tool_arguments(arguments: Option<&Value>) -> Value {
    match arguments {
        Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(object)) => Value::Object(object),
            _ => json!({}),
        },
        Some(value @ Value::Object(_)) => value.clone(),
        _ => json!({}),
    }
}

fn tool_result_block(message: &Map<String, Value>) -> Result<Value, IngressError> {
    let id = message
        .get("tool_call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| IngressError::UnsupportedPart("tool_call_id".into()))?;
    let content = match message.get("content") {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => Value::Array({
            let mut blocks = Vec::new();
            for part in parts {
                let part = part.as_object().ok_or(IngressError::NotJson)?;
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        blocks.extend(text_block(text));
                    }
                    Some(other) => return Err(IngressError::UnsupportedPart(other.into())),
                    None => return Err(IngressError::NotJson),
                }
            }
            blocks
        }),
        Some(Value::Null) | None => Value::String(String::new()),
        _ => return Err(IngressError::NotJson),
    };
    Ok(json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": content,
    }))
}

fn tool_definitions(tools: Option<&Value>) -> Result<Vec<Value>, IngressError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    let tools = tools.as_array().ok_or(IngressError::NotJson)?;
    let mut definitions = Vec::new();
    for tool in tools {
        let tool = tool.as_object().ok_or(IngressError::NotJson)?;
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        let function = tool
            .get("function")
            .and_then(Value::as_object)
            .ok_or(IngressError::NotJson)?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or(IngressError::NotJson)?;
        let mut definition = Map::new();
        definition.insert("name".into(), json!(sanitize_tool_name(name)));
        definition.insert(
            "description".into(),
            json!(function
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()),
        );
        definition.insert(
            "input_schema".into(),
            function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        );
        if let Some(strict) = function.get("strict").or_else(|| tool.get("strict")) {
            if let Some(strict) = strict.as_bool() {
                definition.insert("strict".into(), json!(strict));
            }
        }
        definitions.push(Value::Object(definition));
    }
    Ok(definitions)
}

fn tool_choice(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    match choice {
        Value::String(name) => match name.as_str() {
            "none" => Some(json!({ "type": "none" })),
            "auto" => Some(json!({ "type": "auto" })),
            "required" => Some(json!({ "type": "any" })),
            _ => None,
        },
        Value::Object(choice) => {
            let kind = choice.get("type").and_then(Value::as_str)?;
            match kind {
                "none" => Some(json!({ "type": "none" })),
                "auto" => Some(json!({ "type": "auto" })),
                "required" | "any" => Some(json!({ "type": "any" })),
                "function" => {
                    let name = choice
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .or_else(|| choice.get("name"))
                        .and_then(Value::as_str)?;
                    Some(json!({ "type": "tool", "name": sanitize_tool_name(name) }))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Anthropic tool names are `[a-zA-Z0-9_-]{1,64}`. Clients such as MCP
/// servers routinely hand over names with dots or spaces.
pub(crate) fn sanitize_tool_name(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    sanitized.truncate(MAX_TOOL_NAME);
    sanitized
}

// ---------------------------------------------------------------------------
// Response half: Anthropic Messages → OpenAI Chat Completions
// ---------------------------------------------------------------------------

/// Anthropic usage counters, merged across every event that reports them.
///
/// Anthropic reports the cache counters *outside* `input_tokens`, while OpenAI
/// folds them into `prompt_tokens` — [`Self::openai`] does that arithmetic in
/// one place so the streaming and buffered paths cannot drift.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MessagesUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    thinking_tokens: Option<u64>,
    reported: bool,
}

impl MessagesUsage {
    /// Merge one `usage` object. Later events win: `message_delta` carries the
    /// final output count, `message_start` only the input side.
    pub fn merge(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage.and_then(Value::as_object) else {
            return;
        };
        self.reported = true;
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = value;
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = value;
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.cache_creation_input_tokens = value;
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read_input_tokens = value;
        }
        if let Some(value) = usage
            .get("output_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("thinking_tokens"))
            .and_then(Value::as_u64)
        {
            self.thinking_tokens = Some(value);
        }
    }

    /// Whether upstream reported usage at all.
    pub fn reported(&self) -> bool {
        self.reported
    }

    /// The OpenAI-shaped `usage` object.
    ///
    /// `completion_tokens_details.reasoning_tokens` is only emitted when
    /// upstream reported it, so a client never sees a fabricated zero.
    pub fn openai(&self) -> Value {
        let prompt_tokens =
            self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens;
        let completion_tokens = self.output_tokens;
        let mut usage = json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
            "prompt_tokens_details": {
                "cached_tokens": self.cache_read_input_tokens,
                "cached_creation_tokens": self.cache_creation_input_tokens,
                "cache_write_tokens": self.cache_creation_input_tokens,
            },
        });
        if let Some(thinking_tokens) = self.thinking_tokens {
            usage["completion_tokens_details"] = json!({ "reasoning_tokens": thinking_tokens });
        }
        usage
    }
}

/// Anthropic `stop_reason` → OpenAI `finish_reason`, upstream's table.
pub fn finish_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "end_turn" | "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" | "model_context_window_exceeded" => "length",
        "refusal" | "sensitive" => "content_filter",
        _ => "stop",
    }
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Translate one buffered Anthropic Messages response body into an OpenAI
/// `chat.completion` body.
///
/// `requested_model` stands in when the response does not carry a model; the
/// caller passes whatever the client asked for so the reply echoes it.
pub fn translate_chat_response(
    body: &[u8],
    requested_model: Option<&str>,
) -> Result<Value, IngressError> {
    let response: Value = serde_json::from_slice(body).map_err(|_| IngressError::NotJson)?;
    let response = response.as_object().ok_or(IngressError::NotJson)?;

    let id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .or(requested_model)
        .unwrap_or_default()
        .to_string();
    let stop_reason = response
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut usage = MessagesUsage::default();
    usage.merge(response.get("usage"));

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in response
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(block) = block.as_object() else {
            continue;
        };
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                if let Some(chunk) = block.get("text").and_then(Value::as_str) {
                    text.push_str(chunk);
                }
            }
            "thinking" => {
                if let Some(chunk) = block.get("thinking").and_then(Value::as_str) {
                    reasoning.push_str(chunk);
                }
            }
            "tool_use" => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "arguments": serde_json::to_string(&input)
                            .unwrap_or_else(|_| "{}".to_string()),
                    },
                }));
            }
            _ => {}
        }
    }

    let mut message = json!({ "role": "assistant", "content": text });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    let mapped = finish_reason(stop_reason);
    let finish = if !tool_calls.is_empty() && mapped != "length" {
        "tool_calls"
    } else {
        mapped
    };
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    Ok(json!({
        "id": id,
        "object": "chat.completion",
        "created": now_seconds(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish,
        }],
        "usage": usage.openai(),
    }))
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    openai_index: u64,
    arguments: String,
}

/// Stateful translator for the streaming half: feed it the upstream SSE lines,
/// get OpenAI `chat.completion.chunk` values back.
///
/// Tool-call arguments are buffered and emitted whole at `content_block_stop`,
/// exactly as upstream does: OpenAI clients reassemble `tool_calls` deltas by
/// `index`, so a single complete delta is a legal stream and avoids forwarding
/// half-parsed JSON to a client that might act on it.
pub struct ChatStreamTranslator {
    model: String,
    id: String,
    created: u64,
    stop_reason: String,
    usage: MessagesUsage,
    pending: std::collections::BTreeMap<u64, PendingToolCall>,
    next_tool_call_index: u64,
    trailing_usage_sent: bool,
}

impl ChatStreamTranslator {
    /// `model` is echoed on every chunk until a `message_start` names one.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            id: String::new(),
            created: 0,
            stop_reason: String::new(),
            usage: MessagesUsage::default(),
            pending: std::collections::BTreeMap::new(),
            next_tool_call_index: 0,
            trailing_usage_sent: false,
        }
    }

    /// Feed one SSE line. Returns the chunks it produced (frequently none).
    ///
    /// Non-`data:` framing (`event:`, `id:`, comments), blank lines and the
    /// `[DONE]` sentinel produce nothing; a `data:` line that is not JSON is
    /// ignored rather than aborting the stream the client is already reading.
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
        if payload.is_empty() || payload == "[DONE]" {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        self.push_event(&event)
    }

    /// Feed a whole SSE body and collect every chunk it produces.
    pub fn push_body(&mut self, body: &[u8]) -> Vec<Value> {
        let body = String::from_utf8_lossy(body);
        body.lines().flat_map(|line| self.push_line(line)).collect()
    }

    fn push_event(&mut self, event: &Value) -> Vec<Value> {
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => self.on_message_start(event),
            "content_block_start" => self.on_content_block_start(event),
            "content_block_delta" => self.on_content_block_delta(event),
            "content_block_stop" => self.on_content_block_stop(event),
            "message_delta" => self.on_message_delta(event),
            "message_stop" => self.on_message_stop(),
            "error" => Self::on_error(event),
            _ => Vec::new(),
        }
    }

    fn on_message_start(&mut self, event: &Value) -> Vec<Value> {
        if let Some(message) = event.get("message") {
            if let Some(id) = message.get("id").and_then(Value::as_str) {
                self.id = id.to_string();
            }
            if let Some(model) = message
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
            {
                self.model = model.to_string();
            }
            self.created = now_seconds();
            self.usage.merge(message.get("usage"));
        }
        vec![self.chunk(json!({ "role": "assistant" }), None)]
    }

    fn on_content_block_start(&mut self, event: &Value) -> Vec<Value> {
        let block = event.get("content_block");
        if block
            .and_then(|block| block.get("type"))
            .and_then(Value::as_str)
            != Some("tool_use")
        {
            return Vec::new();
        }
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        let openai_index = self.next_tool_call_index;
        self.next_tool_call_index += 1;
        let pending = PendingToolCall {
            id: block
                .and_then(|block| block.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: block
                .and_then(|block| block.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            openai_index,
            arguments: String::new(),
        };
        self.pending.insert(index, pending);
        Vec::new()
    }

    fn on_content_block_delta(&mut self, event: &Value) -> Vec<Value> {
        let Some(delta) = event.get("delta") else {
            return Vec::new();
        };
        match delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text_delta" => delta
                .get("text")
                .and_then(Value::as_str)
                .map(|text| vec![self.chunk(json!({ "content": text }), None)])
                .unwrap_or_default(),
            "thinking_delta" => delta
                .get("thinking")
                .and_then(Value::as_str)
                .map(|thinking| vec![self.chunk(json!({ "reasoning_content": thinking }), None)])
                .unwrap_or_default(),
            "input_json_delta" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let (Some(partial), Some(pending)) = (
                    delta.get("partial_json").and_then(Value::as_str),
                    self.pending.get_mut(&index),
                ) {
                    pending.arguments.push_str(partial);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_content_block_stop(&mut self, event: &Value) -> Vec<Value> {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        let Some(pending) = self.pending.remove(&index) else {
            return Vec::new();
        };
        let arguments = if pending.arguments.is_empty() {
            "{}".to_string()
        } else {
            pending.arguments
        };
        vec![self.chunk(
            json!({
                "tool_calls": [{
                    "index": pending.openai_index,
                    "id": pending.id,
                    "type": "function",
                    "function": {
                        "name": pending.name,
                        "arguments": arguments,
                    },
                }],
            }),
            None,
        )]
    }

    fn on_message_delta(&mut self, event: &Value) -> Vec<Value> {
        if let Some(reason) = event
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
        {
            self.stop_reason = reason.to_string();
        }
        self.usage.merge(event.get("usage"));
        let mapped = if self.stop_reason.is_empty() {
            "stop"
        } else {
            finish_reason(&self.stop_reason)
        };
        let mut chunk = self.chunk(json!({}), Some(mapped));
        if event.get("usage").is_some() {
            chunk["usage"] = self.usage.openai();
        }
        vec![chunk]
    }

    fn on_message_stop(&mut self) -> Vec<Value> {
        if !self.usage.reported() || self.trailing_usage_sent {
            return Vec::new();
        }
        self.trailing_usage_sent = true;
        vec![json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": self.usage.openai(),
        })]
    }

    fn on_error(event: &Value) -> Vec<Value> {
        let Some(error) = event.get("error") else {
            return Vec::new();
        };
        vec![json!({
            "error": {
                "message": error.get("message").and_then(Value::as_str).unwrap_or_default(),
                "type": error.get("type").and_then(Value::as_str).unwrap_or_default(),
            },
        })]
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }],
        })
    }
}

/// Translate a whole buffered SSE body in one call.
pub fn translate_chat_sse(model: &str, body: &[u8]) -> Vec<Value> {
    ChatStreamTranslator::new(model).push_body(body)
}

/// Render one buffered Anthropic Messages body as the OpenAI
/// `chat.completion` body an HTTP client receives.
pub fn render_chat_body(model: &str, body: &[u8]) -> Result<Vec<u8>, IngressError> {
    let translated = translate_chat_response(body, Some(model))?;
    serde_json::to_vec(&translated).map_err(|_| IngressError::NotJson)
}

/// Render one buffered Anthropic Messages SSE body as the OpenAI stream an
/// HTTP client receives: one `data: <chunk>` frame per translated chunk, then
/// the `data: [DONE]` sentinel OpenAI clients wait for.
pub fn render_chat_stream(model: &str, body: &[u8]) -> Vec<u8> {
    let mut rendered = String::new();
    for chunk in translate_chat_sse(model, body) {
        if let Ok(encoded) = serde_json::to_string(&chunk) {
            rendered.push_str("data: ");
            rendered.push_str(&encoded);
            rendered.push_str("\n\n");
        }
    }
    rendered.push_str("data: [DONE]\n\n");
    rendered.into_bytes()
}

/// Render an OpenAI-shaped error envelope.
///
/// A client that speaks Chat Completions only understands this shape, so every
/// refusal this gateway makes on that route — a malformed request, an
/// unsupported model, an upstream failure — arrives as one. `status` picks the
/// OpenAI error `type` the client's own retry logic keys on.
pub fn render_chat_error(status: u16, message: &str) -> Vec<u8> {
    let kind = match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        501 | 503 => "service_unavailable_error",
        _ => "api_error",
    };
    let envelope = json!({
        "error": {
            "message": message,
            "type": kind,
            "param": Value::Null,
            "code": Value::Null,
        }
    });
    serde_json::to_vec(&envelope).unwrap_or_default()
}

/// `clud __openai-chat-translate --response [--stream] [--http]` — the process
/// seam for the response half. Reads the upstream body on stdin; writes one
/// OpenAI JSON object per line (a single `chat.completion`, or one chunk per
/// line for a stream), or with `--http` the exact HTTP body the route writes.
/// No session, backend, credential or daemon action.
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
    if http {
        // The exact bytes the route puts on the wire, so the Python suite can
        // assert the client-visible framing rather than one JSON per line.
        let model = requested_model.unwrap_or_default();
        let rendered = if stream {
            render_chat_stream(model, &body)
        } else {
            match render_chat_body(model, &body) {
                Ok(rendered) => rendered,
                Err(error) => {
                    eprintln!("{error}");
                    return 2;
                }
            }
        };
        return match std::io::stdout().write_all(&rendered) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("could not write the translated response: {error}");
                2
            }
        };
    }
    let mut rendered = Vec::new();
    if stream {
        let mut translator = ChatStreamTranslator::new(requested_model.unwrap_or_default());
        for chunk in translator.push_body(&body) {
            match serde_json::to_vec(&chunk) {
                Ok(mut encoded) => {
                    encoded.push(b'\n');
                    rendered.extend_from_slice(&encoded);
                }
                Err(error) => {
                    eprintln!("could not encode a translated chunk: {error}");
                    return 2;
                }
            }
        }
    } else {
        match translate_chat_response(&body, requested_model) {
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

    #[test]
    fn rendered_stream_frames_are_data_lines_terminated_by_done() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_r\",\"model\":\"claude-x\",\"usage\":{\"input_tokens\":2}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let rendered = String::from_utf8(render_chat_stream("alias-m", sse.as_bytes())).unwrap();
        assert!(rendered.ends_with("data: [DONE]\n\n"), "{rendered:?}");
        let frames: Vec<&str> = rendered.split("\n\n").filter(|f| !f.is_empty()).collect();
        assert!(
            frames.iter().all(|frame| frame.starts_with("data: ")),
            "{frames:?}"
        );
        let first: Value = serde_json::from_str(&frames[0]["data: ".len()..]).unwrap();
        assert_eq!(first["object"], "chat.completion.chunk");
        // The body's own model wins over the fallback alias.
        assert_eq!(first["model"], "claude-x");
    }

    #[test]
    fn rendered_error_is_an_openai_envelope_typed_by_status() {
        let body: Value = serde_json::from_slice(&render_chat_error(429, "slow down")).unwrap();
        assert_eq!(body["error"]["message"], "slow down");
        assert_eq!(body["error"]["type"], "rate_limit_error");
        let body: Value = serde_json::from_slice(&render_chat_error(400, "bad")).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    fn translate(request: Value) -> Value {
        translate_chat_request(&serde_json::to_vec(&request).unwrap()).unwrap()
    }

    #[test]
    fn plain_user_message_becomes_one_text_turn() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [{"role": "user", "content": "hello"}],
        }));
        assert_eq!(out["model"], "claude-sonnet");
        assert_eq!(out["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(out["stream"], false);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["type"], "text");
        assert_eq!(out["messages"][0]["content"][0]["text"], "hello");
        assert!(out.get("system").is_none());
    }

    #[test]
    fn system_and_developer_messages_both_become_system_blocks() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "developer", "content": [{"type": "text", "text": "use tools"}]},
                {"role": "user", "content": "hi"},
            ],
        }));
        assert_eq!(out["system"][0]["text"], "be terse");
        assert_eq!(out["system"][1]["text"], "use tools");
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn assistant_tool_calls_become_tool_use_and_results_coalesce() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [
                {"role": "user", "content": "read a.rs"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"},
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "fn main() {}"},
                {"role": "tool", "tool_call_id": "call_2", "content": "second"},
            ],
        }));
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["input"]["path"], "a.rs");
        // Both tool results land in one user turn: Anthropic alternates roles.
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(messages[2]["content"][1]["tool_use_id"], "call_2");
    }

    #[test]
    fn malformed_tool_arguments_become_an_empty_object() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [
                {"role": "user", "content": "x"},
                {"role": "assistant", "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "noop", "arguments": "not json"},
                }]},
            ],
        }));
        assert_eq!(out["messages"][1]["content"][0]["input"], json!({}));
    }

    #[test]
    fn function_tools_and_required_choice_map_to_anthropic() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [{"role": "user", "content": "x"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                    "strict": true,
                },
            }],
            "tool_choice": "required",
            "parallel_tool_calls": false,
        }));
        assert_eq!(out["tools"][0]["name"], "read_file");
        assert_eq!(
            out["tools"][0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(out["tools"][0]["strict"], true);
        assert_eq!(out["tool_choice"]["type"], "any");
        assert_eq!(out["tool_choice"]["disable_parallel_tool_use"], true);
    }

    #[test]
    fn function_tool_choice_names_the_tool() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [{"role": "user", "content": "x"}],
            "tools": [{"type": "function", "function": {"name": "read.file"}}],
            "tool_choice": {"type": "function", "function": {"name": "read.file"}},
        }));
        assert_eq!(
            out["tool_choice"],
            json!({"type": "tool", "name": "read_file"})
        );
        assert_eq!(out["tools"][0]["name"], "read_file");
        assert_eq!(
            out["tools"][0]["input_schema"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn tool_choice_without_tools_is_not_forwarded() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [{"role": "user", "content": "x"}],
            "tool_choice": "required",
        }));
        assert!(out.get("tool_choice").is_none());
        assert!(out.get("tools").is_none());
    }

    #[test]
    fn inline_images_keep_their_data_url_and_remote_images_stay_urls() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "max_completion_tokens": 512,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
            ]}],
        }));
        assert_eq!(out["max_tokens"], 512);
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
        assert_eq!(content[2]["source"]["type"], "url");
        assert_eq!(content[2]["source"]["url"], "https://example.com/a.png");
    }

    #[test]
    fn sampling_parameters_and_stop_strings_are_carried() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [{"role": "user", "content": "x"}],
            "temperature": 0.2,
            "top_p": 0.9,
            "stop": "DONE",
            "reasoning_effort": "HIGH",
        }));
        assert_eq!(out["temperature"], 0.2);
        assert_eq!(out["top_p"], 0.9);
        assert_eq!(out["stop_sequences"], json!(["DONE"]));
        assert_eq!(out["output_config"]["effort"], "high");
    }

    #[test]
    fn effort_none_is_omitted_rather_than_forwarded() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [{"role": "user", "content": "x"}],
            "reasoning_effort": "none",
        }));
        assert!(out.get("output_config").is_none());
    }

    #[test]
    fn consecutive_same_role_messages_merge_into_one_turn() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [
                {"role": "user", "content": "a"},
                {"role": "user", "content": "b"},
            ],
        }));
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn system_only_request_gets_the_blank_user_turn_anthropic_requires() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [{"role": "system", "content": "instructions"}],
        }));
        assert_eq!(out["system"][0]["text"], "instructions");
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "");
    }

    #[test]
    fn blank_content_does_not_create_an_empty_turn() {
        let out = translate(json!({
            "model": "claude-sonnet",
            "messages": [
                {"role": "user", "content": "  "},
                {"role": "assistant", "content": ""},
                {"role": "user", "content": "real"},
            ],
        }));
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"][0]["text"], "real");
    }

    #[test]
    fn errors_are_typed_and_name_the_offending_input() {
        assert_eq!(
            translate_chat_request(b"not json").unwrap_err(),
            IngressError::NotJson
        );
        assert_eq!(
            translate_chat_request(br#"{"messages":[]}"#).unwrap_err(),
            IngressError::MissingModel
        );
        assert_eq!(
            translate_chat_request(br#"{"model":"m"}"#).unwrap_err(),
            IngressError::EmptyMessages
        );
        assert_eq!(
            translate_chat_request(br#"{"model":"m","messages":[{"role":"user","content":[]}]}"#)
                .unwrap_err(),
            IngressError::EmptyMessages
        );
        let unsupported = translate_chat_request(
            br#"{"model":"m","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AAAA"}}]}]}"#,
        )
        .unwrap_err();
        assert_eq!(
            unsupported,
            IngressError::UnsupportedPart("input_audio".into())
        );
        assert!(unsupported.to_string().contains("input_audio"));
    }

    fn translate_response(response: Value) -> Value {
        translate_chat_response(&serde_json::to_vec(&response).unwrap(), None).unwrap()
    }

    #[test]
    fn buffered_response_becomes_a_chat_completion_with_folded_cache_tokens() {
        let out = translate_response(json!({
            "id": "msg_1",
            "type": "message",
            "model": "claude-sonnet",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 7,
                "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 5,
            },
        }));
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["id"], "msg_1");
        assert_eq!(out["model"], "claude-sonnet");
        assert_eq!(out["choices"][0]["message"]["content"], "hello world");
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 18);
        assert_eq!(out["usage"]["completion_tokens"], 7);
        assert_eq!(out["usage"]["total_tokens"], 25);
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 5);
        assert_eq!(
            out["usage"]["prompt_tokens_details"]["cache_write_tokens"],
            3
        );
        assert!(out["usage"].get("completion_tokens_details").is_none());
        assert!(out["created"].as_u64().unwrap() > 0);
    }

    #[test]
    fn buffered_thinking_and_tool_use_become_reasoning_and_tool_calls() {
        let out = translate_response(json!({
            "id": "msg_2",
            "model": "claude-sonnet",
            "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "consider the file"},
                {"type": "text", "text": "reading"},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.rs"}},
            ],
            "usage": {"input_tokens": 1, "output_tokens": 2,
                      "output_tokens_details": {"thinking_tokens": 1}},
        }));
        let message = &out["choices"][0]["message"];
        assert_eq!(message["reasoning_content"], "consider the file");
        assert_eq!(message["content"], "reading");
        assert_eq!(message["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(message["tool_calls"][0]["type"], "function");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a.rs"}"#
        );
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            out["usage"]["completion_tokens_details"]["reasoning_tokens"],
            1
        );
    }

    #[test]
    fn requested_model_stands_in_when_the_response_has_none() {
        let out = translate_chat_response(
            br#"{"content":[{"type":"text","text":"hi"}],"stop_reason":"max_tokens"}"#,
            Some("my-alias"),
        )
        .unwrap();
        assert_eq!(out["model"], "my-alias");
        assert_eq!(out["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn stop_reason_table_matches_upstream() {
        assert_eq!(finish_reason("end_turn"), "stop");
        assert_eq!(finish_reason("stop_sequence"), "stop");
        assert_eq!(finish_reason("tool_use"), "tool_calls");
        assert_eq!(finish_reason("max_tokens"), "length");
        assert_eq!(finish_reason("model_context_window_exceeded"), "length");
        assert_eq!(finish_reason("refusal"), "content_filter");
        assert_eq!(finish_reason("sensitive"), "content_filter");
        assert_eq!(finish_reason("something_new"), "stop");
    }

    fn stream(events: &[Value]) -> Vec<Value> {
        let mut translator = ChatStreamTranslator::new("claude-sonnet");
        let mut chunks = Vec::new();
        for event in events {
            let line = format!("data: {}", serde_json::to_string(event).unwrap());
            chunks.extend(translator.push_line(&line));
        }
        chunks
    }

    #[test]
    fn streaming_text_stream_ends_with_a_trailing_usage_chunk() {
        let chunks = stream(&[
            json!({"type": "message_start", "message": {"id": "msg_s", "model": "claude-sonnet",
                   "usage": {"input_tokens": 4}}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "hel"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(chunks.len(), 5);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[0]["id"], "msg_s");
        assert_eq!(chunks[0]["object"], "chat.completion.chunk");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "hel");
        assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "lo");
        assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
        assert_eq!(chunks[3]["usage"]["completion_tokens"], 2);
        assert_eq!(chunks[4]["choices"].as_array().unwrap().len(), 0);
        assert_eq!(chunks[4]["usage"]["prompt_tokens"], 4);
        assert_eq!(chunks[4]["usage"]["total_tokens"], 6);
    }

    #[test]
    fn streaming_thinking_goes_out_as_reasoning_content() {
        let chunks = stream(&[
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "signature_delta", "signature": "x"}}),
            json!({"type": "ping"}),
        ]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0]["choices"][0]["delta"]["reasoning_content"], "hmm");
    }

    #[test]
    fn streaming_tool_arguments_are_buffered_and_emitted_whole() {
        let chunks = stream(&[
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "tool_use", "id": "toolu_9", "name": "read_file"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "input_json_delta", "partial_json": "th\":\"a.rs\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ]);
        // start and both argument deltas produce nothing; stop emits the call.
        assert_eq!(chunks.len(), 2);
        let call = &chunks[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["id"], "toolu_9");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "read_file");
        assert_eq!(call["function"]["arguments"], r#"{"path":"a.rs"}"#);
        assert_eq!(chunks[1]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn streaming_tool_call_without_arguments_sends_an_empty_object() {
        let chunks = stream(&[
            json!({"type": "content_block_start", "index": 3,
                   "content_block": {"type": "tool_use", "id": "toolu_2", "name": "list"}}),
            json!({"type": "content_block_stop", "index": 3}),
        ]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }

    #[test]
    fn streaming_ignores_framing_and_unparsable_frames() {
        let mut translator = ChatStreamTranslator::new("claude-sonnet");
        assert!(translator.push_line("").is_empty());
        assert!(translator.push_line(": keep-alive").is_empty());
        assert!(translator.push_line("event: message_start").is_empty());
        assert!(translator.push_line("data: [DONE]").is_empty());
        assert!(translator.push_line("data: {not json").is_empty());
        assert!(translator.push_line("nonsense").is_empty());
    }

    #[test]
    fn streaming_error_event_is_reported_to_the_client() {
        let chunks = stream(&[json!({"type": "error",
            "error": {"type": "overloaded_error", "message": "try later"}})]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0]["error"]["type"], "overloaded_error");
        assert_eq!(chunks[0]["error"]["message"], "try later");
    }

    #[test]
    fn response_bodies_that_are_not_json_are_refused() {
        assert_eq!(
            translate_chat_response(b"nope", None).unwrap_err(),
            IngressError::NotJson
        );
        assert_eq!(
            translate_chat_response(b"[]", None).unwrap_err(),
            IngressError::NotJson
        );
    }
}

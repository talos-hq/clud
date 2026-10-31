//! Gemini `generateContent` request → Anthropic Messages request translation.
//!
//! This is the *ingress* half of the Gemini client dialect: a caller that
//! speaks Google's `generateContent` wire format (the `gemini` CLI, a Gemini
//! SDK, or a remote `clud serve` instance in Gemini mode) is rewritten into the
//! Messages request the rest of clud already serves. It needs no provider row,
//! credential or route, so it is additive: nothing enables it until a route
//! selects it, and an unused module changes no behaviour.
//!
//! Ported from CLIProxyAPI
//! `internal/translator/claude/gemini/claude_gemini_request.go`
//! (`ConvertGeminiRequestToClaude`), plus the shared helpers it calls
//! (`translatorcommon.NewClaudeMessageAccumulator`, `util.SanitizeClaudeFunctionName`,
//! `normalizeClaudeToolSchema`, `lowercaseClaudeToolSchemaTypes`,
//! `setClaudeToolChoiceFromGeminiToolConfig`).
//!
//! Deliberate differences, all documented rather than silent:
//!
//! * **The system instruction is a top-level `system` array**, not a synthetic
//!   first `user` turn. Upstream has to fold it into `messages` because the
//!   Claude Code API shape it targets rejects a top-level field; clud speaks the
//!   Messages API, which owns `system`.
//! * **A `functionCall` in a non-assistant turn is a typed error**, not dropped:
//!   a model call can only be replayed from an assistant turn, and silently
//!   losing it would answer a truncated conversation.
//! * **Media Claude cannot read never becomes invisible.** In an assistant turn
//!   the block becomes the same placeholder text upstream emits, and in a user
//!   turn it is a typed error naming the part (upstream records a turn drop and
//!   continues), so the caller learns its attachment did not arrive.
//! * **`candidateCount > 1` is a typed error**: clud serves one response per
//!   request and will not silently answer a one-of-N question with one.
//! * **No `metadata.user_id` derivation and no thinking-summary rewriting.**
//!   clud does not fingerprint callers (see the same note in `openai_ingress`).
//! * **`service_tier` is not forwarded**; clud's catalog does not carry a tier
//!   per route and forwarding an unknown tier is a validation error upstream.

use crate::provider_catalog::EffortLevel;
use serde_json::{json, Value};

/// Why a `generateContent` request cannot be served as a Messages request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiClientIngressError {
    /// The body was not a JSON object.
    NotJson,
    /// No model in the request and no override, so the target is unknown.
    MissingModel,
    /// The request carried no `contents` to send.
    EmptyContents,
    /// A content part clud has no Messages spelling for. Names the part.
    UnsupportedPart(String),
    /// A generation option clud cannot honour. Names the option.
    UnsupportedOption(String),
    /// A response block clud has no Gemini part for. Names the block.
    UnsupportedResponseBlock(String),
}

impl std::fmt::Display for GeminiClientIngressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson => write!(formatter, "the request body is not a JSON object"),
            Self::MissingModel => write!(formatter, "the request names no model"),
            Self::EmptyContents => write!(formatter, "the request carries no contents"),
            Self::UnsupportedPart(part) => {
                write!(
                    formatter,
                    "cannot translate {part} to a Messages content block"
                )
            }
            Self::UnsupportedOption(option) => {
                write!(formatter, "cannot honour {option} on a Messages route")
            }
            Self::UnsupportedResponseBlock(block) => {
                write!(formatter, "cannot translate {block} to a Gemini part")
            }
        }
    }
}

impl std::error::Error for GeminiClientIngressError {}

/// Translate one Gemini `generateContent` request into an Anthropic Messages
/// request body.
///
/// `model` is the resolved Claude-side model id. When it is empty the request's
/// own `model` field is used, so a caller that already names a routable model
/// needs no override. `stream` is the caller's framing choice; a Gemini body
/// does not carry it (`streamGenerateContent` is a different URL).
pub fn translate_generate_content_request(
    model: &str,
    stream: bool,
    body: &[u8],
) -> Result<Value, GeminiClientIngressError> {
    let parsed: Value =
        serde_json::from_slice(body).map_err(|_| GeminiClientIngressError::NotJson)?;
    let request = parsed
        .as_object()
        .ok_or(GeminiClientIngressError::NotJson)?;

    let target = if model.trim().is_empty() {
        request
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or(GeminiClientIngressError::MissingModel)?
    } else {
        model.trim()
    };

    let mut out = json!({
        "model": target,
        "max_tokens": crate::openai_ingress::DEFAULT_MAX_TOKENS,
        "messages": [],
    });

    apply_generation_config(request, &mut out)?;
    apply_system_instruction(request, &mut out);

    let mut messages = build_messages(request)?;
    if messages.is_empty() {
        messages.push(json!({ "role": "user", "content": [text_block("")] }));
    } else if messages[0].get("role").and_then(Value::as_str) == Some("assistant") {
        // The Messages API needs a user turn first; a conversation that opens
        // with a replayed model turn gets an empty user turn, exactly as the
        // OpenAI ingress does for a system-only request.
        messages.insert(0, json!({ "role": "user", "content": [text_block("")] }));
    }
    out["messages"] = Value::Array(messages);

    apply_tools(request, &mut out)?;
    apply_tool_config(request, &mut out);
    out["stream"] = Value::Bool(stream);
    Ok(out)
}

/// `clud __gemini-client-translate` — the process seam the Python suite drives.
/// Reads one `generateContent` request from stdin and writes the translated
/// Messages body (compact JSON + newline) to stdout. No session, backend,
/// credential or daemon action.
pub fn run_translate_stdin(model: Option<&str>, stream: bool) -> i32 {
    use std::io::{Read, Write};

    let mut body = Vec::new();
    if std::io::stdin().read_to_end(&mut body).is_err() {
        eprintln!("could not read the request body from stdin");
        return 2;
    }
    match translate_generate_content_request(model.unwrap_or_default(), stream, &body) {
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

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// `generationConfig` → sampling, stop sequences and thinking.
fn apply_generation_config(
    request: &serde_json::Map<String, Value>,
    out: &mut Value,
) -> Result<(), GeminiClientIngressError> {
    let Some(config) = request.get("generationConfig").and_then(Value::as_object) else {
        return Ok(());
    };

    if let Some(max) = config.get("maxOutputTokens").and_then(Value::as_u64) {
        if max > 0 {
            out["max_tokens"] = json!(max);
        }
    }
    if let Some(temperature) = config.get("temperature").and_then(Value::as_f64) {
        out["temperature"] = json!(temperature);
    }
    if let Some(top_p) = config.get("topP").and_then(Value::as_f64) {
        out["top_p"] = json!(top_p);
    }
    if let Some(top_k) = config.get("topK").and_then(Value::as_i64) {
        if top_k > 0 {
            out["top_k"] = json!(top_k);
        }
    }
    if let Some(count) = config.get("candidateCount").and_then(Value::as_i64) {
        if count > 1 {
            return Err(GeminiClientIngressError::UnsupportedOption(
                "generationConfig.candidateCount > 1".to_string(),
            ));
        }
    }
    if let Some(stops) = config.get("stopSequences").and_then(Value::as_array) {
        let sequences: Vec<Value> = stops
            .iter()
            .filter_map(Value::as_str)
            .map(|stop| json!(stop))
            .collect();
        if !sequences.is_empty() {
            out["stop_sequences"] = Value::Array(sequences);
        }
    }
    if let Some(thinking) = config.get("thinkingConfig").and_then(Value::as_object) {
        apply_thinking_config(thinking, out)?;
    }
    Ok(())
}

fn apply_thinking_config(
    thinking: &serde_json::Map<String, Value>,
    out: &mut Value,
) -> Result<(), GeminiClientIngressError> {
    let level = thinking
        .get("thinkingLevel")
        .or_else(|| thinking.get("thinking_level"))
        .and_then(Value::as_str);
    if let Some(level) = level.map(str::trim).filter(|level| !level.is_empty()) {
        let level = if level.eq_ignore_ascii_case("minimal") {
            "low"
        } else {
            level
        };
        if level.eq_ignore_ascii_case("none") {
            out["thinking"] = json!({ "type": "disabled" });
        } else if level.eq_ignore_ascii_case("auto") {
            out["thinking"] = json!({ "type": "enabled" });
        } else {
            let parsed = EffortLevel::parse(level).ok_or_else(|| {
                GeminiClientIngressError::UnsupportedOption(format!(
                    "generationConfig.thinkingConfig.thinkingLevel {level}"
                ))
            })?;
            out["thinking"] = json!({ "type": "enabled" });
            out["output_config"] = json!({ "effort": parsed.as_str() });
        }
        return Ok(());
    }

    let budget = thinking
        .get("thinkingBudget")
        .or_else(|| thinking.get("thinking_budget"))
        .and_then(Value::as_i64);
    match budget {
        None => Ok(()),
        Some(0) => {
            out["thinking"] = json!({ "type": "disabled" });
            Ok(())
        }
        // -1 asks the provider to size the budget; the Messages API says that
        // with `enabled` and no `budget_tokens`.
        Some(budget) if budget < 0 => {
            out["thinking"] = json!({ "type": "enabled" });
            Ok(())
        }
        Some(budget) => {
            out["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            Ok(())
        }
    }
}

/// `systemInstruction` (or `system_instruction`) text parts → `system` blocks.
fn apply_system_instruction(request: &serde_json::Map<String, Value>, out: &mut Value) {
    let Some(instruction) = request
        .get("systemInstruction")
        .or_else(|| request.get("system_instruction"))
    else {
        return;
    };
    let Some(parts) = instruction.get("parts").and_then(Value::as_array) else {
        return;
    };
    let mut joined: Vec<&str> = Vec::new();
    for part in parts {
        if is_thought_part(part) {
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            joined.push(text);
        }
    }
    if joined.is_empty() {
        return;
    }
    out["system"] = json!([text_block(&joined.join("\n"))]);
}

/// `contents` → `messages`, merging adjacent turns of the same role.
fn build_messages(
    request: &serde_json::Map<String, Value>,
) -> Result<Vec<Value>, GeminiClientIngressError> {
    let Some(contents) = request.get("contents").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut messages: Vec<Value> = Vec::new();
    let mut pending_tool_ids: Vec<String> = Vec::new();
    let mut tool_call_counter: u64 = 0;

    for content in contents {
        let role = match content.get("role").and_then(Value::as_str) {
            Some("model") => "assistant",
            Some("function") | Some("tool") => "user",
            Some("user") | None | Some("") => "user",
            Some(other) => {
                return Err(GeminiClientIngressError::UnsupportedPart(format!(
                    "content role {other}"
                )))
            }
        };

        let mut blocks: Vec<Value> = Vec::new();
        if let Some(parts) = content.get("parts").and_then(Value::as_array) {
            for part in parts {
                if is_thought_part(part) {
                    continue;
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    blocks.push(text_block(text));
                    continue;
                }
                if let Some(call) = part.get("functionCall") {
                    let block =
                        tool_use_block(call, role, &mut pending_tool_ids, &mut tool_call_counter)?;
                    blocks.push(block);
                    continue;
                }
                if let Some(response) = part.get("functionResponse") {
                    blocks.push(tool_result_block(
                        response,
                        &mut pending_tool_ids,
                        &mut tool_call_counter,
                    ));
                    continue;
                }
                if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                    blocks.push(inline_data_block(inline, role)?);
                    continue;
                }
                if let Some(file) = part.get("fileData").or_else(|| part.get("file_data")) {
                    blocks.push(file_data_block(file));
                    continue;
                }
                if part.as_object().is_some_and(|object| object.is_empty()) {
                    continue;
                }
                let named = part
                    .as_object()
                    .and_then(|object| object.keys().next())
                    .cloned()
                    .unwrap_or_else(|| "an empty part".to_string());
                return Err(GeminiClientIngressError::UnsupportedPart(format!(
                    "content part {named}"
                )));
            }
        }

        if blocks.is_empty() {
            continue;
        }

        let merged = match messages.last_mut() {
            Some(last) if last.get("role").and_then(Value::as_str) == Some(role) => last,
            _ => {
                messages.push(json!({ "role": role, "content": [] }));
                messages.last_mut().expect("a message was just pushed")
            }
        };
        if let Some(existing) = merged.get_mut("content").and_then(Value::as_array_mut) {
            existing.extend(blocks);
        }
    }

    if !contents.is_empty() && messages.is_empty() {
        return Err(GeminiClientIngressError::EmptyContents);
    }
    Ok(messages)
}

/// Gemini marks replayed reasoning with `thought: true`; the Messages ingress
/// does not replay thinking, so those parts are skipped (never errors).
fn is_thought_part(part: &Value) -> bool {
    part.get("thought")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn tool_id_from(value: &Value) -> Option<String> {
    ["id", "call_id"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|id| !id.is_empty())
        .map(str::to_string)
}

fn next_generated_tool_id(counter: &mut u64) -> String {
    *counter += 1;
    format!("toolu_gemini_{:016}", *counter)
}

fn tool_use_block(
    call: &Value,
    role: &str,
    pending_tool_ids: &mut Vec<String>,
    counter: &mut u64,
) -> Result<Value, GeminiClientIngressError> {
    if role != "assistant" {
        return Err(GeminiClientIngressError::UnsupportedPart(
            "functionCall in a user turn".to_string(),
        ));
    }
    let tool_id = tool_id_from(call).unwrap_or_else(|| next_generated_tool_id(counter));
    pending_tool_ids.push(tool_id.clone());

    let name = call
        .get("name")
        .and_then(Value::as_str)
        .map(crate::openai_ingress::sanitize_tool_name)
        .unwrap_or_default();
    let input = call
        .get("args")
        .filter(|args| args.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(json!({
        "type": "tool_use",
        "id": tool_id,
        "name": name,
        "input": input,
    }))
}

fn tool_result_block(
    response: &Value,
    pending_tool_ids: &mut Vec<String>,
    counter: &mut u64,
) -> Value {
    let tool_use_id = match tool_id_from(response) {
        Some(custom) => {
            pending_tool_ids.retain(|pending| pending != &custom);
            custom
        }
        None if !pending_tool_ids.is_empty() => pending_tool_ids.remove(0),
        None => next_generated_tool_id(counter),
    };
    let content = match response.get("response") {
        Some(response) => match response.get("result") {
            Some(result) => result
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| result.to_string()),
            None => response.to_string(),
        },
        None => String::new(),
    };
    json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content,
    })
}

fn mime_type(value: &Value) -> Option<&str> {
    ["mimeType", "mime_type"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|mime| !mime.is_empty())
}

fn inline_data_block(inline: &Value, role: &str) -> Result<Value, GeminiClientIngressError> {
    let mime = mime_type(inline).unwrap_or_default();
    let data = inline
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if mime.is_empty() || data.is_empty() {
        return Err(GeminiClientIngressError::UnsupportedPart(
            "an inlineData part with no mimeType or data".to_string(),
        ));
    }
    let lower = mime.to_ascii_lowercase();
    if lower.starts_with("image/") {
        return Ok(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": mime, "data": data },
        }));
    }
    if lower.starts_with("application/") || lower.starts_with("text/") {
        return Ok(json!({
            "type": "document",
            "source": { "type": "base64", "media_type": mime, "data": data },
        }));
    }
    if role == "assistant" {
        // A model turn cannot be silently shortened; the placeholder keeps the
        // transcript shape without claiming Claude can read the media.
        return Ok(text_block(&format!(
            "Media content: inline data (Type: {mime})"
        )));
    }
    Err(GeminiClientIngressError::UnsupportedPart(format!(
        "inlineData with mimeType {mime}"
    )))
}

fn file_data_block(file: &Value) -> Value {
    let uri = ["fileUri", "file_uri"]
        .iter()
        .filter_map(|key| file.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|uri| !uri.is_empty())
        .unwrap_or_default();
    let mime = mime_type(file).unwrap_or_default();
    let lower = mime.to_ascii_lowercase();
    if lower.starts_with("image/") {
        return json!({ "type": "image", "source": { "type": "url", "url": uri } });
    }
    if lower.starts_with("application/") || lower.starts_with("text/") {
        let mut source = json!({ "type": "url", "url": uri });
        if !mime.is_empty() {
            source["media_type"] = json!(mime);
        }
        return json!({ "type": "document", "source": source });
    }
    let mut info = format!("File: {uri}");
    if !mime.is_empty() {
        info.push_str(&format!(" (Type: {mime})"));
    }
    text_block(&info)
}

/// `tools[].functionDeclarations[]` → Messages `tools`.
fn apply_tools(
    request: &serde_json::Map<String, Value>,
    out: &mut Value,
) -> Result<(), GeminiClientIngressError> {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else {
        return Ok(());
    };
    let mut declared: Vec<Value> = Vec::new();
    for tool in tools {
        let Some(functions) = tool.get("functionDeclarations").and_then(Value::as_array) else {
            continue;
        };
        for function in functions {
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .map(crate::openai_ingress::sanitize_tool_name)
                .unwrap_or_default();
            if name.is_empty() {
                return Err(GeminiClientIngressError::UnsupportedPart(
                    "a functionDeclaration with no name".to_string(),
                ));
            }
            let schema = function
                .get("parametersJsonSchema")
                .or_else(|| function.get("parameters"))
                .map(normalize_tool_schema)
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            let mut declared_tool = json!({ "name": name, "input_schema": schema });
            if let Some(description) = function.get("description").and_then(Value::as_str) {
                declared_tool["description"] = json!(description);
            }
            declared.push(declared_tool);
        }
    }
    if !declared.is_empty() {
        out["tools"] = Value::Array(declared);
    }
    Ok(())
}

/// Claude validates draft-07 schemas and expects lowercase `type` values; Gemini
/// declarations carry neither guarantee.
fn normalize_tool_schema(parameters: &Value) -> Value {
    if !parameters.is_object() {
        return json!({ "type": "object", "properties": {} });
    }
    let mut schema = parameters.clone();
    if schema.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        schema["additionalProperties"] = json!(false);
    }
    schema["$schema"] = json!("http://json-schema.org/draft-07/schema#");
    lowercase_schema_types(&mut schema);
    schema
}

fn lowercase_schema_types(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(kind)) = object.get("type") {
                object.insert("type".to_string(), json!(kind.to_ascii_lowercase()));
            }
            for child in object.values_mut() {
                lowercase_schema_types(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                lowercase_schema_types(item);
            }
        }
        _ => {}
    }
}

/// `toolConfig.functionCallingConfig` → `tool_choice`.
fn apply_tool_config(request: &serde_json::Map<String, Value>, out: &mut Value) {
    let Some(config) = request
        .get("toolConfig")
        .or_else(|| request.get("tool_config"))
    else {
        return;
    };
    let Some(calling) = config
        .get("functionCallingConfig")
        .or_else(|| config.get("function_calling_config"))
    else {
        return;
    };
    let Some(mode) = calling.get("mode").and_then(Value::as_str) else {
        return;
    };
    match mode.to_ascii_uppercase().as_str() {
        "AUTO" => out["tool_choice"] = json!({ "type": "auto" }),
        "NONE" => out["tool_choice"] = json!({ "type": "none" }),
        "ANY" => {
            let allowed = calling
                .get("allowedFunctionNames")
                .or_else(|| calling.get("allowed_function_names"))
                .and_then(Value::as_array);
            match allowed {
                Some(names) if names.len() == 1 => {
                    let name = names[0]
                        .as_str()
                        .map(crate::openai_ingress::sanitize_tool_name)
                        .unwrap_or_default();
                    out["tool_choice"] = json!({ "type": "tool", "name": name });
                }
                _ => out["tool_choice"] = json!({ "type": "any" }),
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(body: Value) -> Value {
        translate_generate_content_request("claude-sonnet-4", false, body.to_string().as_bytes())
            .expect("the request translates")
    }

    #[test]
    fn system_instruction_and_text_turns_map_to_messages() {
        let out = translate(json!({
            "systemInstruction": { "parts": [{ "text": "be brief" }, { "text": "no lists" }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "hi" }] },
                { "role": "model", "parts": [{ "text": "hello" }] },
            ],
        }));
        assert_eq!(out["model"], "claude-sonnet-4");
        assert_eq!(out["max_tokens"], crate::openai_ingress::DEFAULT_MAX_TOKENS);
        assert_eq!(out["system"][0]["text"], "be brief\nno lists");
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(out["messages"][1]["role"], "assistant");
        assert_eq!(out["messages"][1]["content"][0]["text"], "hello");
        assert_eq!(out["stream"], false);
    }

    #[test]
    fn adjacent_same_role_turns_merge_into_one_message() {
        let out = translate(json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "one" }] },
                { "role": "user", "parts": [{ "text": "two" }] },
            ],
        }));
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[0]["content"][1]["text"], "two");
    }

    #[test]
    fn function_calls_pair_with_results_by_fifo_and_keep_given_ids() {
        let out = translate(json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "run it" }] },
                { "role": "model", "parts": [
                    { "functionCall": { "name": "Read.File", "args": { "path": "a" } } },
                    { "functionCall": { "name": "list", "id": "call_7", "args": {} } },
                ] },
                { "role": "user", "parts": [
                    { "functionResponse": { "name": "Read.File", "response": { "result": "ok" } } },
                    { "functionResponse": { "name": "list", "response": { "items": [1] } } },
                ] },
            ],
        }));
        let assistant = &out["messages"][1];
        let first = &assistant["content"][0];
        let second = &assistant["content"][1];
        assert_eq!(first["type"], "tool_use");
        assert_eq!(first["name"], "Read_File");
        assert_eq!(first["id"], "toolu_gemini_0000000000000001");
        assert_eq!(first["input"]["path"], "a");
        assert_eq!(second["id"], "call_7");

        let results = &out["messages"][2]["content"];
        assert_eq!(results[0]["tool_use_id"], "toolu_gemini_0000000000000001");
        assert_eq!(results[0]["content"], "ok");
        assert_eq!(results[1]["tool_use_id"], "call_7");
        assert_eq!(results[1]["content"], "{\"items\":[1]}");
    }

    #[test]
    fn inline_image_becomes_a_base64_image_block_and_documents_become_documents() {
        let out = translate(json!({
            "contents": [
                { "role": "user", "parts": [
                    { "inlineData": { "mimeType": "image/png", "data": "AAA" } },
                    { "inline_data": { "mime_type": "application/pdf", "data": "BBB" } },
                ] },
            ],
        }));
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "AAA");
        assert_eq!(blocks[1]["type"], "document");
        assert_eq!(blocks[1]["source"]["media_type"], "application/pdf");
    }

    #[test]
    fn unreadable_user_media_is_a_named_error_but_an_assistant_turn_gets_a_placeholder() {
        let err = translate_generate_content_request(
            "claude-sonnet-4",
            false,
            json!({
                "contents": [{ "role": "user", "parts": [
                    { "inlineData": { "mimeType": "video/mp4", "data": "AAA" } },
                ] }],
            })
            .to_string()
            .as_bytes(),
        )
        .expect_err("video has no Messages spelling");
        assert_eq!(
            err,
            GeminiClientIngressError::UnsupportedPart("inlineData with mimeType video/mp4".into())
        );

        let out = translate(json!({
            "contents": [{ "role": "model", "parts": [
                { "inlineData": { "mimeType": "video/mp4", "data": "AAA" } },
            ] }],
        }));
        assert_eq!(out["messages"][1]["content"][0]["type"], "text");
        assert_eq!(
            out["messages"][1]["content"][0]["text"],
            "Media content: inline data (Type: video/mp4)"
        );
    }

    #[test]
    fn file_data_becomes_a_url_source() {
        let out = translate(json!({
            "contents": [{ "role": "user", "parts": [
                { "fileData": { "mimeType": "image/jpeg", "fileUri": "https://x/y.jpg" } },
            ] }],
        }));
        let block = &out["messages"][0]["content"][0];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "url");
        assert_eq!(block["source"]["url"], "https://x/y.jpg");
    }

    #[test]
    fn function_declarations_become_tools_with_a_lowercase_closed_schema() {
        let out = translate(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "tools": [{ "functionDeclarations": [
                {
                    "name": "Read.File",
                    "description": "read",
                    "parameters": {
                        "type": "OBJECT",
                        "properties": { "path": { "type": "STRING" } },
                        "required": ["path"],
                    },
                },
                { "name": "noargs" },
            ] }],
        }));
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "Read_File");
        assert_eq!(tools[0]["description"], "read");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        assert_eq!(
            tools[0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(tools[0]["input_schema"]["additionalProperties"], false);
        assert_eq!(
            tools[0]["input_schema"]["$schema"],
            "http://json-schema.org/draft-07/schema#"
        );
        assert_eq!(tools[1]["input_schema"]["type"], "object");
    }

    #[test]
    fn tool_config_modes_map_to_tool_choice() {
        let any_one = translate(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "toolConfig": { "functionCallingConfig": {
                "mode": "ANY",
                "allowedFunctionNames": ["Read.File"],
            } },
        }));
        assert_eq!(any_one["tool_choice"]["type"], "tool");
        assert_eq!(any_one["tool_choice"]["name"], "Read_File");

        let auto = translate(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "tool_config": { "function_calling_config": { "mode": "AUTO" } },
        }));
        assert_eq!(auto["tool_choice"]["type"], "auto");
    }

    #[test]
    fn thinking_budget_and_level_both_map_to_anthropic_thinking() {
        let budget = translate(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": { "thinkingConfig": { "thinkingBudget": 4096 } },
        }));
        assert_eq!(budget["thinking"]["type"], "enabled");
        assert_eq!(budget["thinking"]["budget_tokens"], 4096);

        let level = translate(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": { "thinkingConfig": { "thinkingLevel": "high" } },
        }));
        assert_eq!(level["thinking"]["type"], "enabled");
        assert_eq!(level["output_config"]["effort"], "high");

        let none = translate(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": { "thinkingConfig": { "thinkingLevel": "none" } },
        }));
        assert_eq!(none["thinking"]["type"], "disabled");
        assert!(none.get("output_config").is_none());
    }

    #[test]
    fn sampling_and_stop_sequences_are_forwarded() {
        let out = translate(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": {
                "maxOutputTokens": 512,
                "temperature": 0.25,
                "topP": 0.9,
                "topK": 40,
                "stopSequences": ["STOP"],
            },
        }));
        assert_eq!(out["max_tokens"], 512);
        assert_eq!(out["temperature"], 0.25);
        assert_eq!(out["top_p"], 0.9);
        assert_eq!(out["top_k"], 40);
        assert_eq!(out["stop_sequences"][0], "STOP");
    }

    #[test]
    fn a_multi_candidate_request_is_refused_by_name() {
        let err = translate_generate_content_request(
            "claude-sonnet-4",
            false,
            json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                "generationConfig": { "candidateCount": 3 },
            })
            .to_string()
            .as_bytes(),
        )
        .expect_err("one response per request");
        assert_eq!(
            err,
            GeminiClientIngressError::UnsupportedOption(
                "generationConfig.candidateCount > 1".into()
            )
        );
    }

    #[test]
    fn thought_parts_are_skipped_and_a_model_first_turn_gets_a_blank_user_turn() {
        let out = translate(json!({
            "contents": [
                { "role": "model", "parts": [
                    { "text": "prior" },
                    { "text": "reasoning", "thought": true },
                ] },
                { "role": "user", "parts": [{ "text": "next" }] },
            ],
        }));
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "");
        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(messages[1]["content"][0]["text"], "prior");
    }

    #[test]
    fn a_model_override_wins_and_a_missing_model_is_an_error() {
        let overridden = translate_generate_content_request(
            "claude-opus-4",
            true,
            json!({ "model": "gemini-3-pro", "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] })
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(overridden["model"], "claude-opus-4");
        assert_eq!(overridden["stream"], true);

        let err = translate_generate_content_request(
            "",
            false,
            json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] })
                .to_string()
                .as_bytes(),
        )
        .expect_err("no model");
        assert_eq!(err, GeminiClientIngressError::MissingModel);
    }

    #[test]
    fn contents_without_a_sendable_part_are_refused_rather_than_sent_empty() {
        let err = translate_generate_content_request(
            "claude-sonnet-4",
            false,
            json!({ "contents": [{ "role": "user", "parts": [{ "text": "x", "thought": true }] }] })
                .to_string()
                .as_bytes(),
        )
        .expect_err("nothing to send");
        assert_eq!(err, GeminiClientIngressError::EmptyContents);
    }
}

// ---------------------------------------------------------------------------
// Response half: Anthropic Messages → Gemini `generateContent`.
//
// Ported from CLIProxyAPI
// `internal/translator/claude/gemini/claude_gemini_response.go`
// (`ConvertClaudeResponseToGemini` / `ConvertClaudeResponseToGeminiNonStream`).
//
// Deliberate differences, all documented rather than silent:
//
// * **No `thoughtSignature` is ever emitted.** Upstream mints one with its
//   signature-compat shim because it owns a Gemini signature store; clud does
//   not, and a fabricated signature makes Gemini reject the replay. Thinking is
//   still surfaced as a `thought` part, just without the signature.
// * **Streaming emits only the new part per frame.** Upstream rebuilds and
//   re-sends every accumulated part on each SSE event, so a client that
//   concatenates frames sees the same text repeatedly. clud emits the delta
//   (one `candidates[0].content.parts[0]` per frame), which is what a Gemini
//   SDK expects.
// * **`usageMetadata` is Gemini-shaped, not upstream's.** Upstream sets
//   `promptTokenCount` to `input_tokens` alone and then puts cache tokens in
//   `cachedContentTokenCount`, so the cached count can exceed the prompt.
//   Claude's `input_tokens` excludes cached tokens, so clud reports
//   `promptTokenCount = input + cache_creation + cache_read` with the cached
//   tokens as the (subset) `cachedContentTokenCount`.
// * **`trafficType` is not fabricated.** Upstream always claims
//   `PROVISIONED_THROUGHPUT`; clud has no such guarantee to report.
// * **An unknown response block is a typed error naming it** instead of being
//   dropped. A `redacted_thinking` block is the one deliberate skip: it is
//   opaque ciphertext with no Gemini spelling.
//

/// Claude's `stop_reason` → Gemini's `finishReason`.
///
/// Gemini has no "refusal" reason, so a Claude refusal maps to `SAFETY` (the
/// closest meaning: the model declined). An absent or unknown reason is `STOP`.
pub fn finish_reason_for(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("max_tokens") => "MAX_TOKENS",
        Some("refusal") => "SAFETY",
        Some("end_turn" | "stop_sequence" | "tool_use" | "pause_turn") | None => "STOP",
        Some(_) => "STOP",
    }
}

/// Anthropic `usage` → Gemini `usageMetadata`.
fn gemini_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage?;
    let count = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let input = count("input_tokens");
    let output = count("output_tokens");
    let cache_write = count("cache_creation_input_tokens");
    let cache_read = count("cache_read_input_tokens");
    let prompt = input + cache_write + cache_read;
    let mut metadata = json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": output,
        "totalTokenCount": prompt + output,
    });
    if cache_write + cache_read > 0 {
        metadata["cachedContentTokenCount"] = json!(cache_write + cache_read);
    }
    Some(metadata)
}

/// One Anthropic content block → one Gemini part.
fn response_parts(content: &Value) -> Result<Vec<Value>, GeminiClientIngressError> {
    let Some(blocks) = content.as_array() else {
        return Ok(Vec::new());
    };
    let mut parts = Vec::new();
    for block in blocks {
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    parts.push(json!({ "text": text }));
                }
            }
            "thinking" => {
                let text = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    parts.push(json!({ "thought": true, "text": text }));
                }
            }
            // Opaque ciphertext: no Gemini spelling, and clud mints no
            // thought signatures, so there is nothing honest to emit.
            "redacted_thinking" => {}
            "tool_use" => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                if name.is_empty() {
                    return Err(GeminiClientIngressError::UnsupportedResponseBlock(
                        "a tool_use block with no name".to_string(),
                    ));
                }
                let arguments = block
                    .get("input")
                    .filter(|input| input.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let mut call = json!({ "name": name, "args": arguments });
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    call["id"] = json!(id);
                }
                parts.push(json!({ "functionCall": call }));
            }
            other => {
                return Err(GeminiClientIngressError::UnsupportedResponseBlock(format!(
                    "a `{other}` response block"
                )));
            }
        }
    }
    Ok(parts)
}

/// Translate one Anthropic Messages response body into a Gemini
/// `GenerateContentResponse`.
///
/// `model` is reported as `modelVersion`; the message's own `model` wins when
/// it is present, so a route can pass its resolved target as a fallback.
pub fn translate_messages_response(
    model: &str,
    body: &[u8],
) -> Result<Value, GeminiClientIngressError> {
    let parsed: Value =
        serde_json::from_slice(body).map_err(|_| GeminiClientIngressError::NotJson)?;
    let message = parsed
        .as_object()
        .ok_or(GeminiClientIngressError::NotJson)?;
    let parts = response_parts(message.get("content").unwrap_or(&Value::Null))?;
    let mut response = json!({
        "candidates": [{
            "content": { "role": "model", "parts": parts },
            "finishReason": finish_reason_for(
                message.get("stop_reason").and_then(Value::as_str)
            ),
        }],
    });
    let reported = message
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(model);
    if !reported.is_empty() {
        response["modelVersion"] = json!(reported);
    }
    if let Some(id) = message.get("id").and_then(Value::as_str) {
        response["responseId"] = json!(id);
    }
    if let Some(usage) = gemini_usage(message.get("usage")) {
        response["usageMetadata"] = usage;
    }
    Ok(response)
}

/// A tool call the stream is still assembling across `input_json_delta`s.
#[derive(Debug, Default)]
struct PendingToolCall {
    name: String,
    id: Option<String>,
    arguments: String,
}

/// Stateful translator for the streaming half: feed it the Claude SSE lines,
/// get Gemini `GenerateContentResponse` objects back, one per frame that
/// carries something new.
pub struct GeminiClientStreamTranslator {
    model: String,
    response_id: String,
    saw_event: bool,
    finished: bool,
    stop_reason: Option<String>,
    usage: Option<Value>,
    tools: std::collections::BTreeMap<u64, PendingToolCall>,
}

impl GeminiClientStreamTranslator {
    /// `model` is reported as `modelVersion` until a frame names one.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            response_id: String::new(),
            saw_event: false,
            finished: false,
            stop_reason: None,
            usage: None,
            tools: std::collections::BTreeMap::new(),
        }
    }

    /// Feed one SSE line. `event:`/`id:`/`retry:` framing, comments, blank
    /// lines and non-JSON `data:` payloads produce nothing; `[DONE]` seals the
    /// stream the way `message_stop` does.
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
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        self.push_event(&event)
    }

    /// Feed a whole SSE body, sealing the stream at the end even when the
    /// upstream never sent `message_stop`.
    pub fn push_body(&mut self, body: &[u8]) -> Vec<Value> {
        let body = String::from_utf8_lossy(body);
        let mut chunks: Vec<Value> = body.lines().flat_map(|line| self.push_line(line)).collect();
        chunks.extend(self.finish());
        chunks
    }

    /// Emit the closing `finishReason` + usage frame once. Idempotent.
    pub fn finish(&mut self) -> Vec<Value> {
        if self.finished || !self.saw_event {
            return Vec::new();
        }
        self.finished = true;
        let mut chunk = json!({
            "candidates": [{
                "content": { "role": "model", "parts": [] },
                "finishReason": finish_reason_for(self.stop_reason.as_deref()),
            }],
        });
        if !self.model.is_empty() {
            chunk["modelVersion"] = json!(self.model);
        }
        if !self.response_id.is_empty() {
            chunk["responseId"] = json!(self.response_id);
        }
        if let Some(usage) = &self.usage {
            chunk["usageMetadata"] = usage.clone();
        }
        vec![chunk]
    }

    fn push_event(&mut self, event: &Value) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        self.saw_event = true;
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => {
                self.start_message(event);
                Vec::new()
            }
            "content_block_start" => {
                self.start_tool(event);
                Vec::new()
            }
            "content_block_delta" => self.push_delta(event),
            "content_block_stop" => self.finish_tool(event),
            "message_delta" => {
                if let Some(reason) = event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(usage) = gemini_usage(event.get("usage")) {
                    self.usage = Some(usage);
                }
                self.finish()
            }
            "message_stop" => self.finish(),
            "error" => {
                self.finished = true;
                let message = event
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("the upstream reported an error");
                vec![gemini_error_response(message)]
            }
            _ => Vec::new(),
        }
    }

    /// `message_start` only names the response; its `content` is always empty.
    fn start_message(&mut self, event: &Value) {
        let Some(message) = event.get("message") else {
            return;
        };
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            self.response_id = id.to_string();
        }
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            if !model.trim().is_empty() {
                self.model = model.to_string();
            }
        }
    }

    /// Record a `tool_use` block so its streamed arguments can be assembled.
    fn start_tool(&mut self, event: &Value) {
        let Some(block) = event.get("content_block") else {
            return;
        };
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return;
        }
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = block.get("id").and_then(Value::as_str).map(str::to_string);
        self.tools.insert(
            self.index_of(event),
            PendingToolCall {
                name,
                id,
                arguments: String::new(),
            },
        );
    }

    fn push_delta(&mut self, event: &Value) -> Vec<Value> {
        let Some(delta) = event.get("delta") else {
            return Vec::new();
        };
        match delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text_delta" => {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![self.chunk(vec![json!({ "text": text })])]
                }
            }
            "thinking_delta" => {
                let text = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![self.chunk(vec![json!({ "thought": true, "text": text })])]
                }
            }
            "input_json_delta" => {
                let index = self.index_of(event);
                if let Some(pending) = self.tools.get_mut(&index) {
                    if let Some(piece) = delta.get("partial_json").and_then(Value::as_str) {
                        pending.arguments.push_str(piece);
                    }
                }
                Vec::new()
            }
            // clud mints no Gemini thought signatures; a signature delta has
            // nothing honest to become.
            _ => Vec::new(),
        }
    }

    /// `content_block_stop` finalises a streamed tool call as one
    /// `functionCall` part.
    fn finish_tool(&mut self, event: &Value) -> Vec<Value> {
        let Some(pending) = self.tools.remove(&self.index_of(event)) else {
            return Vec::new();
        };
        if pending.name.trim().is_empty() {
            return Vec::new();
        }
        let arguments = serde_json::from_str::<Value>(pending.arguments.trim())
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let mut call = json!({ "name": pending.name, "args": arguments });
        if let Some(id) = pending.id {
            call["id"] = json!(id);
        }
        vec![self.chunk(vec![json!({ "functionCall": call })])]
    }

    fn index_of(&self, event: &Value) -> u64 {
        event.get("index").and_then(Value::as_u64).unwrap_or(0)
    }

    fn chunk(&self, parts: Vec<Value>) -> Value {
        let mut chunk = json!({
            "candidates": [{ "content": { "role": "model", "parts": parts } }],
        });
        if !self.model.is_empty() {
            chunk["modelVersion"] = json!(self.model);
        }
        if !self.response_id.is_empty() {
            chunk["responseId"] = json!(self.response_id);
        }
        chunk
    }
}

/// The bytes a route writes to refuse a `generateContent` call.
///
/// Gemini clients read `error.code` + `error.status`; the HTTP status is
/// preserved from the caller's contract (a 429 stays a 429) and only the name
/// follows it, so a client's own retry logic keeps working.
pub fn render_generate_content_error(status: u16, message: &str) -> Vec<u8> {
    let name = match status {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        501 => "UNIMPLEMENTED",
        503 => "UNAVAILABLE",
        504 => "DEADLINE_EXCEEDED",
        _ => "INTERNAL",
    };
    let error = json!({
        "error": {
            "code": status,
            "message": message,
            "status": name,
        }
    });
    serde_json::to_vec(&error).unwrap_or_default()
}

/// The Gemini error envelope upstream emits for a failed translation.
fn gemini_error_response(message: &str) -> Value {
    json!({
        "error": {
            "code": 400,
            "message": message,
            "status": "INVALID_ARGUMENT",
        }
    })
}

fn gemini_error_for(error: &GeminiClientIngressError) -> Value {
    gemini_error_response(&error.to_string())
}

/// The bytes a served `generateContent` call writes for a buffered Messages
/// response, or the Gemini error envelope for a refusal.
pub fn render_generate_content_body(model: &str, body: &[u8]) -> Vec<u8> {
    match translate_messages_response(model, body) {
        Ok(translated) => serde_json::to_vec(&translated).unwrap_or_default(),
        Err(error) => serde_json::to_vec(&gemini_error_for(&error)).unwrap_or_default(),
    }
}

/// The SSE bytes for a streamed Messages response: `data: <chunk>` per frame.
pub fn render_generate_content_stream(model: &str, body: &[u8]) -> Vec<u8> {
    let mut translator = GeminiClientStreamTranslator::new(model);
    let mut rendered = Vec::new();
    for chunk in translator.push_body(body) {
        rendered.extend_from_slice(b"data: ");
        rendered.extend_from_slice(&serde_json::to_vec(&chunk).unwrap_or_default());
        rendered.extend_from_slice(b"\n\n");
    }
    rendered
}

/// `clud __gemini-client-translate --response [--stream] [--http]` — the
/// process seam for the response half. Reads the upstream body on stdin; writes
/// one Gemini response object per line (a single response, or one chunk per
/// line for a stream), or with `--http` the exact body a route would write.
/// No session, backend, credential or daemon action.
pub fn run_translate_response_stdin(stream: bool, model: Option<&str>, http: bool) -> i32 {
    use std::io::{Read, Write};

    let mut body = Vec::new();
    if std::io::stdin().read_to_end(&mut body).is_err() {
        eprintln!("could not read the response body from stdin");
        return 2;
    }
    let model = model.unwrap_or_default();
    if http {
        let rendered = if stream {
            render_generate_content_stream(model, &body)
        } else {
            render_generate_content_body(model, &body)
        };
        return match std::io::stdout().write_all(&rendered) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("could not write the translated response: {error}");
                2
            }
        };
    }
    let chunks = if stream {
        let mut translator = GeminiClientStreamTranslator::new(model);
        translator.push_body(&body)
    } else {
        match translate_messages_response(model, &body) {
            Ok(translated) => vec![translated],
            Err(error) => {
                eprintln!("{error}");
                return 2;
            }
        }
    };
    let mut rendered = Vec::new();
    for chunk in chunks {
        rendered.extend_from_slice(&serde_json::to_vec(&chunk).unwrap_or_default());
        rendered.push(b'\n');
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
mod response_tests {
    use super::*;

    fn body(value: &Value) -> Vec<u8> {
        value.to_string().into_bytes()
    }

    #[test]
    fn a_messages_response_becomes_a_generate_content_response() {
        let response = translate_messages_response(
            "claude-sonnet-4",
            &body(&json!({
                "id": "msg_01",
                "model": "claude-sonnet-4-5",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "tool_use", "id": "toolu_9", "name": "read_file", "input": { "path": "a" } }
                ],
                "stop_reason": "tool_use",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 4,
                    "cache_creation_input_tokens": 5,
                    "cache_read_input_tokens": 2
                }
            })),
        )
        .unwrap();

        assert_eq!(response["candidates"][0]["content"]["role"], "model");
        assert_eq!(response["candidates"][0]["finishReason"], "STOP");
        assert_eq!(
            response["candidates"][0]["content"]["parts"][0]["text"],
            "hello"
        );
        let call = &response["candidates"][0]["content"]["parts"][1]["functionCall"];
        assert_eq!(call["name"], "read_file");
        assert_eq!(call["args"]["path"], "a");
        assert_eq!(call["id"], "toolu_9");
        assert_eq!(response["responseId"], "msg_01");
        assert_eq!(response["modelVersion"], "claude-sonnet-4-5");
        // input_tokens excludes cached tokens, so the prompt count folds them in.
        assert_eq!(response["usageMetadata"]["promptTokenCount"], 17);
        assert_eq!(response["usageMetadata"]["cachedContentTokenCount"], 7);
        assert_eq!(response["usageMetadata"]["candidatesTokenCount"], 4);
        assert_eq!(response["usageMetadata"]["totalTokenCount"], 21);
    }

    #[test]
    fn thinking_becomes_a_thought_part_and_redacted_thinking_is_skipped() {
        let response = translate_messages_response(
            "claude-opus-4",
            &body(&json!({
                "content": [
                    { "type": "thinking", "thinking": "weigh it", "signature": "sig" },
                    { "type": "redacted_thinking", "data": "opaque" },
                    { "type": "text", "text": "answer" }
                ],
                "stop_reason": "max_tokens"
            })),
        )
        .unwrap();
        let parts = response["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["thought"], true);
        assert_eq!(parts[0]["text"], "weigh it");
        assert!(
            parts[0].get("thoughtSignature").is_none(),
            "clud mints no Gemini signatures"
        );
        assert_eq!(response["candidates"][0]["finishReason"], "MAX_TOKENS");
    }

    #[test]
    fn an_unknown_response_block_is_named_not_dropped() {
        let error = translate_messages_response(
            "claude-sonnet-4",
            &body(&json!({ "content": [{ "type": "server_tool_use", "name": "x" }] })),
        )
        .expect_err("unknown block");
        assert_eq!(
            error,
            GeminiClientIngressError::UnsupportedResponseBlock(
                "a `server_tool_use` response block".to_string()
            )
        );

        let error = translate_messages_response("m", b"not json").expect_err("not json");
        assert_eq!(error, GeminiClientIngressError::NotJson);
    }

    fn sse(events: &[Value]) -> Vec<u8> {
        let mut body = String::new();
        for event in events {
            body.push_str(&format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap_or("message"),
                event
            ));
        }
        body.into_bytes()
    }

    #[test]
    fn the_stream_emits_one_chunk_per_delta_and_seals_once() {
        let events = [
            json!({ "type": "message_start", "message": { "id": "msg_7", "model": "claude-opus-4" } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "he" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "llo" } }),
            json!({ "type": "ping" }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "max_tokens" }, "usage": { "input_tokens": 3, "output_tokens": 2 } }),
            json!({ "type": "message_stop" }),
        ];
        let mut translator = GeminiClientStreamTranslator::new("fallback");
        let chunks = translator.push_body(&sse(&events));
        assert_eq!(chunks.len(), 3, "two text deltas + one sealed frame");
        assert_eq!(
            chunks[0]["candidates"][0]["content"]["parts"][0]["text"],
            "he"
        );
        assert_eq!(
            chunks[1]["candidates"][0]["content"]["parts"][0]["text"],
            "llo"
        );
        assert_eq!(chunks[1]["responseId"], "msg_7");
        assert_eq!(chunks[2]["candidates"][0]["finishReason"], "MAX_TOKENS");
        assert_eq!(chunks[2]["usageMetadata"]["promptTokenCount"], 3);
        assert_eq!(chunks[2]["modelVersion"], "claude-opus-4");
        // Idempotent: a second seal emits nothing.
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn streamed_tool_arguments_are_assembled_at_block_stop() {
        let events = [
            json!({ "type": "message_start", "message": { "id": "msg_8" } }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "glob" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"pattern\":" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "\"*.rs\"}" } }),
            json!({ "type": "content_block_stop", "index": 1 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } }),
            json!({ "type": "message_stop" }),
        ];
        let chunks = GeminiClientStreamTranslator::new("m").push_body(&sse(&events));
        assert_eq!(chunks.len(), 2);
        let call = &chunks[0]["candidates"][0]["content"]["parts"][0]["functionCall"];
        assert_eq!(call["name"], "glob");
        assert_eq!(call["args"]["pattern"], "*.rs");
        assert_eq!(call["id"], "toolu_1");
        assert_eq!(chunks[1]["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn a_stream_error_frame_is_a_gemini_error_envelope() {
        let events = [
            json!({ "type": "message_start", "message": { "id": "msg_9" } }),
            json!({ "type": "error", "error": { "type": "overloaded_error", "message": "busy" } }),
        ];
        let chunks = GeminiClientStreamTranslator::new("m").push_body(&sse(&events));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0]["error"]["message"], "busy");
        assert_eq!(chunks[0]["error"]["status"], "INVALID_ARGUMENT");
        assert!(chunks[0].get("candidates").is_none());
    }

    #[test]
    fn the_http_renderers_write_a_body_and_data_frames() {
        let buffered = render_generate_content_body(
            "m",
            &body(
                &json!({ "content": [{ "type": "text", "text": "ok" }], "stop_reason": "end_turn" }),
            ),
        );
        let parsed: Value = serde_json::from_slice(&buffered).unwrap();
        assert_eq!(parsed["candidates"][0]["finishReason"], "STOP");

        let refused = render_generate_content_body("m", b"[]");
        let error: Value = serde_json::from_slice(&refused).unwrap();
        assert_eq!(error["error"]["code"], 400);

        let streamed = render_generate_content_stream(
            "m",
            &sse(&[
                json!({ "type": "message_start", "message": { "id": "msg_10" } }),
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "hi" } }),
            ]),
        );
        let text = String::from_utf8(streamed).unwrap();
        assert!(text.starts_with("data: {"));
        assert_eq!(text.matches("data: ").count(), 2, "{text}");
    }

    #[test]
    fn every_stop_reason_has_a_gemini_finish_reason() {
        assert_eq!(finish_reason_for(Some("end_turn")), "STOP");
        assert_eq!(finish_reason_for(Some("tool_use")), "STOP");
        assert_eq!(finish_reason_for(Some("stop_sequence")), "STOP");
        assert_eq!(finish_reason_for(Some("max_tokens")), "MAX_TOKENS");
        assert_eq!(finish_reason_for(Some("refusal")), "SAFETY");
        assert_eq!(finish_reason_for(None), "STOP");
    }
}

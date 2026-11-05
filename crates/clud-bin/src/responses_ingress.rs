//! OpenAI Responses client ingress: one `POST /v1/responses` body turned into an
//! Anthropic Messages request.
//!
//! Ports CLIProxyAPI
//! `internal/translator/claude/openai/responses/claude_openai-responses_request.go`
//! (`ConvertOpenAIResponsesRequestToClaude`): the Responses API is item-based, so
//! `instructions` plus every role=system/developer input item become top-level
//! system blocks in source order, `message` items become user/assistant turns,
//! `function_call`/`custom_tool_call` items become `tool_use`, their `*_output`
//! partners become `tool_result`, and `reasoning` items become `thinking` blocks
//! when they carry an encrypted signature.
//!
//! Deliberate differences from upstream, all recorded in
//! `docs/architecture/standalone-gateway.md`:
//!
//! * a builtin server tool (`web_search`, `file_search`, `code_interpreter`,
//!   `image_generation`, `computer_use*`, `local_shell`, `mcp`, `tool_search`) is
//!   refused by name instead of being dropped, because clud has no counterpart and
//!   silently removing a capability makes the model answer worse for no stated
//!   reason;
//! * `text.format` / `response_format` structured-output synthesis, tool-search
//!   discovery replay and the namespace/custom-tool name map are not ported yet;
//! * a reasoning item without `encrypted_content` keeps its text as an assistant
//!   text block, because a Claude `thinking` block without a signature is invalid;
//! * `cache_control` markers are not replayed (clud does not pass them through on
//!   any ingress today).

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::openai_ingress::{sanitize_tool_name, IngressError, DEFAULT_MAX_TOKENS};

/// Translate one OpenAI Responses request body into an Anthropic Messages body.
///
/// `model` is passed through verbatim: catalog resolution, allowlists and effort
/// suffixes stay with the gateway that already owns them.
pub fn translate_responses_request(body: &[u8]) -> Result<Value, IngressError> {
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
        json!(request
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    out.insert(
        "stream".into(),
        json!(request.get("stream").and_then(Value::as_bool) == Some(true)),
    );
    if let Some(temperature) = request.get("temperature").and_then(Value::as_f64) {
        out.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.get("top_p").and_then(Value::as_f64) {
        out.insert("top_p".into(), json!(top_p));
    }
    if let Some(effort) = effort(request.get("reasoning")) {
        out.insert("output_config".into(), json!({ "effort": effort }));
    }

    let mut system: Vec<Value> = Vec::new();
    if let Some(instructions) = request
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        system.push(json!({ "type": "text", "text": instructions }));
    }

    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    match request.get("input") {
        Some(Value::String(text)) => {
            if !text.trim().is_empty() {
                push_turn(&mut turns, "user", vec![text_block(text)]);
            }
        }
        Some(Value::Array(items)) => translate_items(items, &mut system, &mut turns)?,
        Some(Value::Null) | None => {}
        Some(_) => return Err(IngressError::NotJson),
    }

    repair_dangling_tool_uses(&mut turns);

    if turns.is_empty() {
        if system.is_empty() {
            return Err(IngressError::EmptyMessages);
        }
        // An empty `messages` array is not a Messages request, and a system-only
        // history still has to leave the client one place to answer.
        turns.push(("user".into(), vec![text_block("")]));
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
    let tool_names: HashSet<String> = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = tool_choice(request.get("tool_choice"), &tool_names) {
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

/// Hidden seam: read one Responses request body on stdin, write the Messages
/// request body the route would forward to stdout.
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
    match translate_responses_request(&body) {
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

/// One item at a time, in source order.
fn translate_items(
    items: &[Value],
    system: &mut Vec<Value>,
    turns: &mut Vec<(String, Vec<Value>)>,
) -> Result<(), IngressError> {
    // Tool results may only follow a call that was actually emitted, so the ids
    // seen so far are tracked in order (upstream's `emittedRawToolUses`).
    let mut emitted_calls: HashSet<String> = HashSet::new();
    for item in items {
        let item = item.as_object().ok_or(IngressError::NotJson)?;
        let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
        if is_system_role(role) {
            // System-level items already became top-level system blocks. Each
            // source block stays separate: Claude keeps operator authority there.
            for block in system_content_blocks(item.get("content"))? {
                system.push(block);
            }
            continue;
        }
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .unwrap_or("message");
        match kind {
            "message" => translate_message(item, turns)?,
            "function_call" | "custom_tool_call" => {
                let call = translate_call(item, kind == "custom_tool_call")?;
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    emitted_calls.insert(id.to_string());
                }
                push_turn(turns, "assistant", vec![call]);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let raw_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default();
                if raw_id.is_empty() || !emitted_calls.contains(&call_id(raw_id)) {
                    // Claude rejects an orphan tool_result, and the client sent
                    // output it never paired with a call (a delegation seed, for
                    // example). Keep the content as plain user text.
                    push_turn(turns, "user", standalone_output_blocks(item.get("output"))?);
                    continue;
                }
                push_turn(turns, "user", vec![tool_result_block(item, raw_id)?]);
            }
            "reasoning" => {
                if let Some(block) = reasoning_block(item) {
                    push_turn(turns, "assistant", vec![block]);
                }
            }
            "additional_tools" => {}
            other => {
                // Reachability guard: a new item type means the client gained a
                // capability whose Claude counterpart still has to be decided.
                // Say so on stderr instead of dropping turn content in silence.
                eprintln!("resolves->claude: dropped input item of unmapped type `{other}`");
            }
        }
    }
    Ok(())
}

fn translate_message(
    item: &Map<String, Value>,
    turns: &mut Vec<(String, Vec<Value>)>,
) -> Result<(), IngressError> {
    let content = item.get("content");
    let mut blocks: Vec<Value> = Vec::new();
    let mut role: Option<&str> = None;
    match content {
        Some(Value::String(text)) => {
            if !text.trim().is_empty() {
                blocks.push(text_block(text));
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                let part = part.as_object().ok_or(IngressError::NotJson)?;
                let (block, part_role) = message_part(part)?;
                if let Some(block) = block {
                    blocks.push(block);
                    role.get_or_insert(part_role);
                }
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(IngressError::NotJson),
    }
    // Fall back to the stated role when the content types were not decisive.
    let role = role.unwrap_or(match item.get("role").and_then(Value::as_str) {
        Some("assistant") => "assistant",
        _ => "user",
    });
    if !blocks.is_empty() {
        push_turn(turns, role, blocks);
    }
    Ok(())
}

/// One content part of a `message` item, and the role its type implies.
fn message_part(part: &Map<String, Value>) -> Result<(Option<Value>, &'static str), IngressError> {
    let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "input_text" => Ok((part_text(part, "text").map(text_block), "user")),
        "output_text" | "text" => Ok((part_text(part, "text").map(text_block), "assistant")),
        "refusal" => Ok((part_text(part, "refusal").map(text_block), "assistant")),
        "input_image" => Ok((image_block(part)?, "user")),
        "input_file" => Ok((document_block(part)?, "user")),
        other => Err(IngressError::UnsupportedPart(format!("item-part:{other}"))),
    }
}

fn part_text<'a>(part: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    part.get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn image_block(part: &Map<String, Value>) -> Result<Option<Value>, IngressError> {
    let url = part
        .get("image_url")
        .and_then(Value::as_str)
        .or_else(|| part.get("url").and_then(Value::as_str))
        .unwrap_or_default();
    if url.is_empty() {
        return Ok(None);
    }
    if let Some((media_type, data)) = split_data_url(url) {
        return Ok(Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        })));
    }
    Ok(Some(json!({
        "type": "image",
        "source": { "type": "url", "url": url },
    })))
}

fn document_block(part: &Map<String, Value>) -> Result<Option<Value>, IngressError> {
    let file_data = part
        .get("file_data")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if file_data.is_empty() {
        return Ok(None);
    }
    let (media_type, data) = split_data_url(file_data).unwrap_or_else(|| {
        (
            "application/octet-stream".to_string(),
            file_data.to_string(),
        )
    });
    Ok(Some(json!({
        "type": "document",
        "source": { "type": "base64", "media_type": media_type, "data": data },
    })))
}

/// `data:<media-type>;base64,<payload>` -> its two halves.
fn split_data_url(url: &str) -> Option<(String, String)> {
    let trimmed = url.strip_prefix("data:")?;
    let mut parts = trimmed.splitn(2, ";base64,");
    let media_type = parts.next().unwrap_or_default();
    let data = parts.next()?;
    let media_type = if media_type.is_empty() {
        "application/octet-stream"
    } else {
        media_type
    };
    (!data.is_empty()).then(|| (media_type.to_string(), data.to_string()))
}

fn translate_call(item: &Map<String, Value>, custom: bool) -> Result<Value, IngressError> {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or(IngressError::UnsupportedPart("call-without-name".into()))?;
    let raw_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = if custom {
        // Claude tool_use input must be an object; freeform text gets a field.
        json!({ "input": item.get("input").and_then(Value::as_str).unwrap_or_default() })
    } else {
        tool_arguments(item.get("arguments"))
    };
    Ok(json!({
        "type": "tool_use",
        "id": call_id(raw_id),
        "name": sanitize_tool_name(name),
        "input": input,
    }))
}

/// Responses carries function arguments as a JSON *string*.
fn tool_arguments(arguments: Option<&Value>) -> Value {
    let raw = match arguments {
        Some(Value::String(text)) => text.clone(),
        Some(other) if other.is_object() => return other.clone(),
        _ => return json!({}),
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn tool_result_block(item: &Map<String, Value>, raw_id: &str) -> Result<Value, IngressError> {
    let mut block = Map::new();
    block.insert("type".into(), json!("tool_result"));
    block.insert("tool_use_id".into(), json!(call_id(raw_id)));
    // Responses reports an unfinished output as status "incomplete"; Claude's
    // only in-band equivalent is is_error, so carry the signal over.
    if item
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status.trim().eq_ignore_ascii_case("incomplete"))
    {
        block.insert("is_error".into(), json!(true));
    }
    block.insert("content".into(), tool_output_content(item.get("output"))?);
    Ok(Value::Object(block))
}

fn tool_output_content(output: Option<&Value>) -> Result<Value, IngressError> {
    match output {
        Some(Value::String(text)) => Ok(json!(text)),
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for part in parts {
                let part = part.as_object().ok_or(IngressError::NotJson)?;
                match part.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "input_text" | "output_text" | "text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            blocks.push(text_block(text));
                        }
                    }
                    "input_image" => {
                        if let Some(block) = image_block(part)? {
                            blocks.push(block);
                        }
                    }
                    other => {
                        return Err(IngressError::UnsupportedPart(format!(
                            "tool-output:{other}"
                        )));
                    }
                }
            }
            Ok(Value::Array(blocks))
        }
        Some(Value::Null) | None => Ok(json!("")),
        Some(other) if other.is_object() => Ok(json!(other.to_string())),
        Some(_) => Ok(json!("")),
    }
}

fn standalone_output_blocks(output: Option<&Value>) -> Result<Vec<Value>, IngressError> {
    match tool_output_content(output)? {
        Value::String(text) => Ok(if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![text_block(&text)]
        }),
        Value::Array(blocks) => Ok(blocks),
        other => Ok(vec![text_block(&other.to_string())]),
    }
}

/// `reasoning` -> `thinking` when a signature is present, else assistant text.
fn reasoning_block(item: &Map<String, Value>) -> Option<Value> {
    let text = reasoning_text(item);
    let signature = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|signature| !signature.is_empty());
    match (text, signature) {
        (Some(text), Some(signature)) => Some(json!({
            "type": "thinking",
            "thinking": text,
            "signature": signature,
        })),
        (Some(text), None) => (!text.trim().is_empty()).then(|| text_block(&text)),
        (None, _) => None,
    }
}

fn reasoning_text(item: &Map<String, Value>) -> Option<String> {
    for key in ["summary", "content"] {
        let Some(parts) = item.get(key).and_then(Value::as_array) else {
            continue;
        };
        let joined: String = parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect();
        if !joined.trim().is_empty() {
            return Some(joined);
        }
    }
    None
}

fn system_content_blocks(content: Option<&Value>) -> Result<Vec<Value>, IngressError> {
    match content {
        Some(Value::String(text)) => Ok(if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![text_block(text)]
        }),
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for part in parts {
                let part = part.as_object().ok_or(IngressError::NotJson)?;
                match part.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "input_text" | "output_text" | "text" => {
                        if let Some(text) = part_text(part, "text") {
                            blocks.push(text_block(text));
                        }
                    }
                    other => {
                        return Err(IngressError::UnsupportedPart(format!(
                            "system-part:{other}"
                        )));
                    }
                }
            }
            Ok(blocks)
        }
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(_) => Err(IngressError::NotJson),
    }
}

fn tool_definitions(tools: Option<&Value>) -> Result<Vec<Value>, IngressError> {
    let Some(tools) = tools.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut definitions = Vec::new();
    for tool in tools {
        let tool = tool.as_object().ok_or(IngressError::NotJson)?;
        let kind = tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function");
        let descriptor = match kind {
            "function" => tool
                .get("function")
                .and_then(Value::as_object)
                .unwrap_or(tool),
            "custom" => {
                let name = tool.get("name").and_then(Value::as_str).ok_or(
                    IngressError::UnsupportedPart("custom-tool-without-name".into()),
                )?;
                definitions.push(json!({
                    "name": sanitize_tool_name(name),
                    "description": tool.get("description").and_then(Value::as_str).unwrap_or_default(),
                    "input_schema": {
                        "type": "object",
                        "properties": { "input": { "type": "string" } },
                        "required": ["input"],
                    },
                }));
                continue;
            }
            other => {
                return Err(IngressError::UnsupportedPart(format!("tool:{other}")));
            }
        };
        let Some(name) = descriptor.get("name").and_then(Value::as_str) else {
            continue;
        };
        let schema = descriptor
            .get("parameters")
            .filter(|schema| schema.is_object())
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        definitions.push(json!({
            "name": sanitize_tool_name(name),
            "description": descriptor.get("description").and_then(Value::as_str).unwrap_or_default(),
            "input_schema": schema,
        }));
    }
    Ok(definitions)
}

fn tool_choice(choice: Option<&Value>, tool_names: &HashSet<String>) -> Option<Value> {
    match choice? {
        Value::String(spec) => match spec.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            // "none" is left unset: it means no tools may run, and the absence of
            // tool_choice is the Anthropic way to say nothing is forced.
            "none" => None,
            "required" => (!tool_names.is_empty()).then(|| json!({ "type": "any" })),
            _ => None,
        },
        Value::Object(spec) => {
            if spec.get("type").and_then(Value::as_str) == Some("none") {
                return None;
            }
            let name = spec
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| {
                    spec.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                })
                .or_else(|| {
                    spec.get("custom")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                })?;
            let name = sanitize_tool_name(name);
            tool_names
                .contains(&name)
                .then(|| json!({ "type": "tool", "name": name }))
        }
        _ => None,
    }
}

/// Merge a new turn into the accumulated alternation.
///
/// Two consecutive items with the same role are one Anthropic turn; keeping them
/// separate is rejected by the Messages API.
fn push_turn(turns: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    match turns.last_mut() {
        Some((previous, content)) if previous == role => content.extend(blocks),
        _ => turns.push((role.to_string(), blocks)),
    }
}

/// Answer every tool_use that never got a result.
///
/// An interrupted Responses turn replays as an assistant tool_use with no
/// partner; the Messages API rejects that history instead of letting the model
/// continue from it, so the gap is closed with an explicit error result.
fn repair_dangling_tool_uses(turns: &mut Vec<(String, Vec<Value>)>) {
    let answered: HashSet<String> = turns
        .iter()
        .flat_map(|(_, blocks)| blocks.iter())
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let dangling: Vec<String> = turns
        .iter()
        .flat_map(|(_, blocks)| blocks.iter())
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .filter(|id| !answered.contains(id))
        .collect();
    if dangling.is_empty() {
        return;
    }
    let blocks = dangling
        .into_iter()
        .map(|id| {
            json!({
                "type": "tool_result",
                "tool_use_id": id,
                "is_error": true,
                "content": "the tool call was interrupted before a result was recorded",
            })
        })
        .collect();
    turns.push(("user".into(), blocks));
}

fn is_system_role(role: &str) -> bool {
    matches!(role, "system" | "developer")
}

/// Claude tool ids are `[a-zA-Z0-9_-]+`; OpenAI's carry the same alphabet in
/// practice, and anything else is folded so a use and its result stay paired.
fn call_id(raw: &str) -> String {
    if raw.is_empty() {
        return "call_unknown".into();
    }
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// `reasoning.effort` -> `output_config.effort`, the level names both wires share.
fn effort(reasoning: Option<&Value>) -> Option<String> {
    let effort = reasoning?
        .get("effort")
        .and_then(Value::as_str)?
        .trim()
        .to_ascii_lowercase();
    (!effort.is_empty() && effort != "none").then_some(effort)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(request: Value) -> Value {
        translate_responses_request(request.to_string().as_bytes()).expect("translates")
    }

    #[test]
    fn plain_input_string_becomes_one_user_turn() {
        let out = translate(json!({ "model": "claude-sonnet", "input": "hello" }));
        assert_eq!(out["model"], "claude-sonnet");
        assert_eq!(out["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(out["stream"], false);
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn instructions_and_system_items_become_system_blocks_in_order() {
        let out = translate(json!({
            "model": "m",
            "instructions": "be terse",
            "input": [
                { "role": "system", "content": [{ "type": "input_text", "text": "operator rule" }] },
                { "role": "user", "content": [{ "type": "input_text", "text": "hi" }] },
                { "role": "developer", "content": "later rule" },
            ],
        }));
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["text"], "be terse");
        assert_eq!(system[1]["text"], "operator rule");
        assert_eq!(system[2]["text"], "later rule");
        // System items never leak into the conversation turns.
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn part_types_decide_the_turn_role() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "question" } ] },
                { "type": "message", "role": "assistant", "content": [
                    { "type": "output_text", "text": "answer" } ] },
            ],
        }));
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][1]["role"], "assistant");
        assert_eq!(out["messages"][1]["content"][0]["text"], "answer");
    }

    #[test]
    fn function_call_and_its_output_become_tool_use_and_result() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "user", "content": "read it" },
                { "type": "function_call", "call_id": "call_a", "name": "read_file",
                  "arguments": "{\"path\":\"a.rs\"}" },
                { "type": "function_call_output", "call_id": "call_a", "output": "fn main() {}" },
            ],
        }));
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["input"]["path"], "a.rs");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_a");
        assert_eq!(messages[2]["content"][0]["content"], "fn main() {}");
    }

    #[test]
    fn malformed_arguments_become_an_empty_object() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "function_call", "call_id": "c1", "name": "f", "arguments": "not json" },
                { "type": "function_call_output", "call_id": "c1", "output": "ok" },
            ],
        }));
        assert_eq!(out["messages"][0]["content"][0]["input"], json!({}));
    }

    #[test]
    fn custom_tool_call_wraps_freeform_input() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "custom_tool_call", "call_id": "c1", "name": "apply_patch",
                  "input": "*** Begin Patch" },
                { "type": "custom_tool_call_output", "call_id": "c1", "output": "done" },
            ],
        }));
        assert_eq!(
            out["messages"][0]["content"][0]["input"],
            json!({ "input": "*** Begin Patch" })
        );
    }

    #[test]
    fn an_unpaired_output_keeps_its_text_as_a_user_turn() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "user", "content": "go" },
                { "type": "function_call_output", "call_id": "never_called", "output": "seed" },
            ],
        }));
        // The two user-side items are one Anthropic turn, so the seed text lands
        // beside the question instead of in a second user message.
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][1]["type"], "text");
        assert_eq!(messages[0]["content"][1]["text"], "seed");
    }

    #[test]
    fn reasoning_with_a_signature_becomes_a_thinking_block() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "pondering" }],
                  "encrypted_content": "sig-1" },
                { "type": "message", "role": "user", "content": "hi" },
            ],
        }));
        let block = &out["messages"][0]["content"][0];
        assert_eq!(block["type"], "thinking");
        assert_eq!(block["thinking"], "pondering");
        assert_eq!(block["signature"], "sig-1");
    }

    #[test]
    fn reasoning_without_a_signature_keeps_its_text() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "reasoning", "content": [{ "type": "reasoning_text", "text": "why" }] },
                { "type": "message", "role": "user", "content": "hi" },
            ],
        }));
        let block = &out["messages"][0]["content"][0];
        assert_eq!(block["type"], "text");
        assert_eq!(block["text"], "why");
    }

    #[test]
    fn inline_images_keep_their_data_url_and_remote_images_stay_urls() {
        let out = translate(json!({
            "model": "m",
            "input": [{ "type": "message", "role": "user", "content": [
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
                { "type": "input_image", "image_url": "https://example.test/a.png" },
            ] }],
        }));
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "AAAA");
        assert_eq!(content[1]["source"]["type"], "url");
        assert_eq!(content[1]["source"]["url"], "https://example.test/a.png");
    }

    #[test]
    fn function_tools_and_a_required_choice_map_to_anthropic() {
        let out = translate(json!({
            "model": "m",
            "input": "hi",
            "tools": [{
                "type": "function",
                "name": "read_file",
                "description": "read a file",
                "parameters": { "type": "object", "properties": { "path": { "type": "string" } } },
            }],
            "tool_choice": "required",
        }));
        assert_eq!(out["tools"][0]["name"], "read_file");
        assert_eq!(
            out["tools"][0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(out["tool_choice"], json!({ "type": "any" }));
    }

    #[test]
    fn a_forced_function_choice_names_the_tool() {
        let out = translate(json!({
            "model": "m",
            "input": "hi",
            "tools": [{ "type": "function", "name": "read_file", "parameters": {} }],
            "tool_choice": { "type": "function", "name": "read_file" },
        }));
        assert_eq!(
            out["tool_choice"],
            json!({ "type": "tool", "name": "read_file" })
        );
    }

    #[test]
    fn parallel_tool_calls_false_disables_parallel_use() {
        let out = translate(json!({
            "model": "m",
            "input": "hi",
            "tools": [{ "type": "function", "name": "f", "parameters": {} }],
            "parallel_tool_calls": false,
        }));
        assert_eq!(out["tool_choice"]["type"], "auto");
        assert_eq!(out["tool_choice"]["disable_parallel_tool_use"], true);
    }

    #[test]
    fn a_builtin_server_tool_is_refused_by_name() {
        let error = translate_responses_request(
            json!({ "model": "m", "input": "hi", "tools": [{ "type": "web_search" }] })
                .to_string()
                .as_bytes(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            IngressError::UnsupportedPart("tool:web_search".into())
        );
    }

    #[test]
    fn a_dangling_tool_use_is_answered_with_an_error_result() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "user", "content": "go" },
                { "type": "function_call", "call_id": "call_x", "name": "f", "arguments": "{}" },
            ],
        }));
        let last = out["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "call_x");
        assert_eq!(last["content"][0]["is_error"], true);
    }

    #[test]
    fn a_system_only_request_gets_the_blank_user_turn() {
        let out = translate(json!({ "model": "m", "instructions": "be terse" }));
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["content"][0]["text"], "");
    }

    #[test]
    fn missing_model_and_empty_input_are_typed_errors() {
        assert_eq!(
            translate_responses_request(b"{\"input\":\"hi\"}").unwrap_err(),
            IngressError::MissingModel
        );
        assert_eq!(
            translate_responses_request(b"{\"model\":\"m\"}").unwrap_err(),
            IngressError::EmptyMessages
        );
        assert_eq!(
            translate_responses_request(b"not json").unwrap_err(),
            IngressError::NotJson
        );
    }

    #[test]
    fn max_output_tokens_sampling_and_effort_are_carried() {
        let out = translate(json!({
            "model": "m",
            "input": "hi",
            "max_output_tokens": 512,
            "temperature": 0.2,
            "top_p": 0.9,
            "reasoning": { "effort": "high" },
        }));
        assert_eq!(out["max_tokens"], 512);
        assert_eq!(out["temperature"], 0.2);
        assert_eq!(out["top_p"], 0.9);
        assert_eq!(out["output_config"]["effort"], "high");
    }

    #[test]
    fn effort_none_is_omitted_rather_than_forwarded() {
        let out = translate(json!({
            "model": "m",
            "input": "hi",
            "reasoning": { "effort": "none" },
        }));
        assert!(out.get("output_config").is_none());
    }

    #[test]
    fn an_unknown_item_type_is_skipped_and_a_bad_part_is_refused() {
        let out = translate(json!({
            "model": "m",
            "input": [
                { "type": "web_search_call", "id": "ws_1" },
                { "type": "message", "role": "user", "content": "hi" },
            ],
        }));
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            translate_responses_request(
                json!({ "model": "m", "input": [{ "type": "message", "role": "user",
                    "content": [{ "type": "input_audio" }] }] })
                .to_string()
                .as_bytes(),
            )
            .unwrap_err(),
            IngressError::UnsupportedPart("item-part:input_audio".into())
        );
    }
}

// ---------------------------------------------------------------------------
// Response half: Anthropic Messages JSON / SSE -> a Responses `response` object
// or an `event: response.*` stream.
//
// Ports CLIProxyAPI
// `internal/translator/claude/openai/responses/claude_openai-responses_response.go`.
//
// Deliberate differences, also recorded in `docs/architecture/standalone-gateway.md`:
//
// * server-side web search (`server_tool_use` + `web_search_tool_result`) folds
//   into upstream's `web_search_call` item only because CLIProxyAPI injects that
//   tool; clud refuses those tools at the door, so the pair is skipped here
//   instead of synthesised;
// * `pause_turn` ends the response as `completed` with no `incomplete_details`,
//   as upstream does: the client continues it by replaying the assistant turn.
// ---------------------------------------------------------------------------

/// Usage folded into the Responses `usage` object.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}

impl ResponsesUsage {
    fn merge(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage.and_then(Value::as_object) else {
            return;
        };
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
    }

    fn input_total(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }

    fn render(&self) -> Value {
        let input = self.input_total();
        let output = self.output_tokens;
        json!({
            "input_tokens": input,
            "input_tokens_details": { "cached_tokens": self.cache_read_input_tokens },
            "output_tokens": output,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": input + output,
        })
    }
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// A Responses `response` still being assembled.
#[derive(Debug, Clone)]
struct ResponseEnvelope {
    id: String,
    created_at: u64,
    model: String,
    output: Vec<Value>,
}

impl ResponseEnvelope {
    fn new(model: &str) -> Self {
        let created_at = now_seconds();
        Self {
            id: format!("resp_{created_at}"),
            created_at,
            model: model.to_string(),
            output: Vec::new(),
        }
    }

    fn status_json(&self, status: &str, details: Option<Value>, usage: Option<Value>) -> Value {
        json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "background": false,
            "error": Value::Null,
            "incomplete_details": details,
            "model": self.model,
            "output": self.output,
            "parallel_tool_calls": true,
            "store": false,
            "truncation": "disabled",
            "usage": usage,
        })
    }
}

/// Upstream's mapping from an Anthropic stop reason to Responses'
/// `incomplete_details`, plus whether the response is incomplete at all.
fn incomplete_details(stop_reason: &str) -> Option<Value> {
    match stop_reason.trim().to_ascii_lowercase().as_str() {
        "max_tokens" | "model_context_window_exceeded" => {
            Some(json!({ "reason": "max_output_tokens" }))
        }
        // A safety refusal stops the turn with a normal 200; Responses clients
        // treat `content_filter` as a retryable refusal, not a finished answer.
        "refusal" => Some(json!({ "reason": "content_filter" })),
        _ => None,
    }
}

fn item_id(prefix: &str, block: &Map<String, Value>) -> String {
    block
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{prefix}_{}", now_seconds()))
}

fn output_text_part(text: &str) -> Value {
    json!({ "type": "output_text", "annotations": [], "logprobs": [], "text": text })
}

fn reasoning_output_item(id: &str, text: &str, signature: &str) -> Value {
    json!({
        "id": id,
        "type": "reasoning",
        "status": "completed",
        "encrypted_content": signature,
        "summary": [{ "type": "summary_text", "text": text }],
    })
}

fn message_output_item(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [output_text_part(text)],
    })
}

fn function_call_output_item(id: &str, call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "id": id,
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

/// Translate one buffered Anthropic Messages body into a Responses `response`.
///
/// `requested_model` is echoed when the upstream body carries no model, so the
/// client sees the name it asked for rather than a provider's wire name.
pub fn translate_responses_response(
    body: &[u8],
    requested_model: Option<&str>,
) -> Result<Value, IngressError> {
    let response: Value = serde_json::from_slice(body).map_err(|_| IngressError::NotJson)?;
    let response = response.as_object().ok_or(IngressError::NotJson)?;
    let stop_reason = response
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let upstream_model = response
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty());
    let model = upstream_model.or(requested_model).unwrap_or_default();
    let mut envelope = ResponseEnvelope::new(model);
    // The upstream id keeps the conversation traceable on both sides.
    if let Some(id) = response.get("id").and_then(Value::as_str) {
        envelope.id = id.to_string();
    }
    let mut usage = ResponsesUsage::default();
    usage.merge(response.get("usage"));
    for block in response
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if let Some(item) = buffered_output_item(block) {
            envelope.output.push(item);
        }
    }
    let details = incomplete_details(&stop_reason);
    let status = if details.is_some() {
        "incomplete"
    } else {
        "completed"
    };
    Ok(envelope.status_json(status, details, Some(usage.render())))
}

/// One Anthropic content block as a Responses output item, or `None` when it
/// has no Responses counterpart (an unknown/extended block type).
fn buffered_output_item(block: &Value) -> Option<Value> {
    let block = block.as_object()?;
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
            Some(message_output_item(&item_id("msg", block), text))
        }
        "thinking" => {
            let text = block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(reasoning_output_item(
                &item_id("rs", block),
                text,
                signature,
            ))
        }
        "redacted_thinking" => {
            // Responses has no redacted item; the payload rides in
            // `encrypted_content` behind upstream's marker so a replay through
            // the request half restores it verbatim.
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(reasoning_output_item(
                &item_id("rs", block),
                "",
                &format!("{REDACTED_THINKING_PREFIX}{data}"),
            ))
        }
        "tool_use" => {
            let arguments = block
                .get("input")
                .map(|input| serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()))
                .unwrap_or_else(|| "{}".to_string());
            Some(function_call_output_item(
                &item_id("fc", block),
                block.get("id").and_then(Value::as_str).unwrap_or_default(),
                block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                &arguments,
            ))
        }
        _ => None,
    }
}

/// Marker upstream puts in `encrypted_content` for a replayed
/// `redacted_thinking` block (`ClaudeResponsesRedactedThinkingPrefix`).
pub const REDACTED_THINKING_PREFIX: &str = "claude-redacted-thinking:";

/// The item a freshly-opened Anthropic block becomes, plus the
/// `response.output_item.added` payloads that announce it (empty for a block
/// whose payload only exists once it completes).
fn open_block(block: &Map<String, Value>) -> Option<(OpenItem, Vec<Value>)> {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "text" => {
            let id = item_id("msg", block);
            let added = json!({
                "id": id.clone(),
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            });
            Some((
                OpenItem::Text {
                    id,
                    text: String::new(),
                },
                vec![added],
            ))
        }
        "thinking" => {
            let id = item_id("rs", block);
            let added = json!({
                "id": id.clone(),
                "type": "reasoning",
                "status": "in_progress",
                "summary": [],
            });
            Some((
                OpenItem::Reasoning {
                    id,
                    text: String::new(),
                    signature: String::new(),
                },
                vec![added],
            ))
        }
        "redacted_thinking" => {
            let id = item_id("rs", block);
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some((
                OpenItem::Reasoning {
                    id,
                    text: String::new(),
                    signature: format!("{REDACTED_THINKING_PREFIX}{data}"),
                },
                Vec::new(),
            ))
        }
        "tool_use" => {
            let id = item_id("fc", block);
            let call_id = block.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let added = json!({
                "id": id.clone(),
                "type": "function_call",
                "status": "in_progress",
                "call_id": call_id,
                "name": name,
                "arguments": "",
            });
            Some((
                OpenItem::Tool {
                    id,
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    arguments: String::new(),
                },
                vec![added],
            ))
        }
        _ => None,
    }
}

fn open_item_id(item: &OpenItem) -> String {
    match item {
        OpenItem::Text { id, .. } | OpenItem::Reasoning { id, .. } | OpenItem::Tool { id, .. } => {
            id.clone()
        }
    }
}

/// A content block mid-stream.
#[derive(Debug)]
enum OpenItem {
    Text {
        id: String,
        text: String,
    },
    Reasoning {
        id: String,
        text: String,
        signature: String,
    },
    Tool {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
}

/// Stateful translator for the streaming half: feed it the upstream Messages SSE
/// lines, get Responses `response.*` events back.
///
/// Anthropic streams a block's `thinking` text *before* its `signature_delta`,
/// so the `response.output_item.done` reasoning item is emitted at
/// `content_block_stop` with the signature collected so far — exactly the point
/// at which upstream seals the item.
pub struct ResponsesStreamTranslator {
    envelope: ResponseEnvelope,
    sequence: u64,
    output_index: u32,
    open: std::collections::BTreeMap<u64, OpenItem>,
    indexes: std::collections::BTreeMap<String, u32>,
    stop_reason: String,
    usage: ResponsesUsage,
    terminal: bool,
}

impl ResponsesStreamTranslator {
    pub fn new(model: impl Into<String>) -> Self {
        let envelope = ResponseEnvelope::new(&model.into());
        Self {
            envelope,
            sequence: 0,
            output_index: 0,
            open: std::collections::BTreeMap::new(),
            indexes: std::collections::BTreeMap::new(),
            stop_reason: String::new(),
            usage: ResponsesUsage::default(),
            terminal: false,
        }
    }

    fn event(&mut self, kind: &str, mut fields: Map<String, Value>) -> Value {
        fields.insert("type".into(), Value::String(kind.to_string()));
        fields.insert("sequence_number".into(), json!(self.sequence));
        self.sequence += 1;
        Value::Object(fields)
    }

    /// Feed one SSE line. Returns the events it produced (usually one, never
    /// more than a handful).
    pub fn push_line(&mut self, line: &str) -> Vec<Value> {
        let line = line.trim();
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

    /// Feed a whole SSE body.
    pub fn push_body(&mut self, body: &[u8]) -> Vec<Value> {
        let text = String::from_utf8_lossy(body);
        let mut out = Vec::new();
        for line in text.lines() {
            out.extend(self.push_line(line));
        }
        out
    }

    /// Close a stream whose upstream body ended without `message_stop`.
    ///
    /// That is a truncated answer, not a finished one: sealing it as
    /// `response.completed` would tell the client the model stopped on purpose,
    /// so the response is failed with a `server_error` the client can retry.
    /// Idempotent, and a no-op once a `message_stop` has sealed the stream.
    pub fn finish(&mut self) -> Vec<Value> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        let mut response = self
            .envelope
            .status_json("failed", None, Some(self.usage.render()));
        response["error"] = json!({
            "code": "server_error",
            "message": "the upstream stream ended before the response was complete",
        });
        let mut fields = Map::new();
        fields.insert("response".into(), response);
        vec![self.event("response.failed", fields)]
    }

    fn push_event(&mut self, event: &Value) -> Vec<Value> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "message_start" => self.message_start(event),
            "content_block_start" => self.block_start(event),
            "content_block_delta" => self.block_delta(event),
            "content_block_stop" => self.block_stop(event),
            "message_delta" => {
                self.message_delta(event);
                Vec::new()
            }
            "message_stop" => self.terminal_event(),
            "error" => vec![self.failed_event(event)],
            _ => Vec::new(),
        }
    }

    fn message_start(&mut self, event: &Value) -> Vec<Value> {
        let message = event.get("message");
        if let Some(id) = message
            .and_then(|message| message.get("id"))
            .and_then(Value::as_str)
        {
            self.envelope.id = id.to_string();
        }
        if let Some(model) = message
            .and_then(|message| message.get("model"))
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
        {
            self.envelope.model = model.to_string();
        }
        self.usage
            .merge(message.and_then(|message| message.get("usage")));
        let created = self.envelope.status_json("in_progress", None, None);
        let mut created_fields = Map::new();
        created_fields.insert("response".into(), created.clone());
        let made = self.event("response.created", created_fields);
        let in_progress = self.envelope.status_json("in_progress", None, None);
        let mut progress_fields = Map::new();
        progress_fields.insert("response".into(), in_progress);
        let started = self.event("response.in_progress", progress_fields);
        vec![made, started]
    }

    fn block_start(&mut self, event: &Value) -> Vec<Value> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let block = event
            .get("content_block")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let output_index = self.output_index;
        let Some((item, added)) = open_block(&block) else {
            return Vec::new();
        };
        let id = open_item_id(&item);
        self.indexes.insert(id.clone(), output_index);
        let opens_text = matches!(item, OpenItem::Text { .. });
        self.open.insert(index, item);
        self.output_index += 1;
        let mut out = Vec::new();
        for item in added {
            let mut fields = Map::new();
            fields.insert("output_index".into(), json!(output_index));
            fields.insert("item".into(), item);
            out.push(self.event("response.output_item.added", fields));
        }
        if opens_text {
            let mut fields = Map::new();
            fields.insert("item_id".into(), Value::String(id));
            fields.insert("output_index".into(), json!(output_index));
            fields.insert("content_index".into(), json!(0));
            fields.insert("part".into(), output_text_part(""));
            out.push(self.event("response.content_part.added", fields));
        }
        out
    }

    fn block_delta(&mut self, event: &Value) -> Vec<Value> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let delta = event
            .get("delta")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let kind = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Each Anthropic delta kind spells its payload differently: text and
        // thinking are `text`/`thinking`, a tool call is `partial_json`, and a
        // signature carries no client-visible text at all.
        let payload = match kind.as_str() {
            "thinking_delta" => delta
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            "input_json_delta" => delta
                .get("partial_json")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => delta
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        let Some(item) = self.open.get_mut(&index) else {
            return Vec::new();
        };
        let id = match item {
            OpenItem::Text { id, text } if kind == "text_delta" => {
                text.push_str(&payload);
                id.clone()
            }
            OpenItem::Reasoning {
                id,
                text,
                signature,
            } => match kind.as_str() {
                "thinking_delta" => {
                    text.push_str(&payload);
                    id.clone()
                }
                "signature_delta" => {
                    signature.push_str(
                        delta
                            .get("signature")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                    return Vec::new();
                }
                _ => return Vec::new(),
            },
            OpenItem::Tool { id, arguments, .. } if kind == "input_json_delta" => {
                arguments.push_str(&payload);
                id.clone()
            }
            _ => return Vec::new(),
        };
        let output_index = self.output_index_for(&id);
        let mut fields = Map::new();
        fields.insert("item_id".into(), Value::String(id));
        fields.insert("output_index".into(), json!(output_index));
        fields.insert("delta".into(), Value::String(payload));
        match kind.as_str() {
            "text_delta" => {
                fields.insert("content_index".into(), json!(0));
                vec![self.event("response.output_text.delta", fields)]
            }
            "thinking_delta" => {
                fields.insert("summary_index".into(), json!(0));
                vec![self.event("response.reasoning_summary_text.delta", fields)]
            }
            _ => vec![self.event("response.function_call_arguments.delta", fields)],
        }
    }

    fn output_index_for(&self, id: &str) -> u32 {
        // Output indexes are assigned at block start; the item id is the only
        // handle a delta carries, so it is what the delta looks the slot up by.
        self.indexes.get(id).copied().unwrap_or_default()
    }

    fn block_stop(&mut self, event: &Value) -> Vec<Value> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let Some(item) = self.open.remove(&index) else {
            return Vec::new();
        };
        let output_index = self.output_index_for(&open_item_id(&item));
        let mut out = Vec::new();
        match item {
            OpenItem::Text { id, text } => {
                let mut done = Map::new();
                done.insert("item_id".into(), Value::String(id.clone()));
                done.insert("output_index".into(), json!(output_index));
                done.insert("content_index".into(), json!(0));
                done.insert("text".into(), Value::String(text.clone()));
                done.insert("logprobs".into(), json!([]));
                out.push(self.event("response.output_text.done", done));
                let mut part_done = Map::new();
                part_done.insert("item_id".into(), Value::String(id.clone()));
                part_done.insert("output_index".into(), json!(output_index));
                part_done.insert("content_index".into(), json!(0));
                part_done.insert("part".into(), output_text_part(&text));
                out.push(self.event("response.content_part.done", part_done));
                self.envelope.output.push(message_output_item(&id, &text));
            }
            OpenItem::Reasoning {
                id,
                text,
                signature,
            } => {
                let mut done = Map::new();
                done.insert("item_id".into(), Value::String(id.clone()));
                done.insert("output_index".into(), json!(output_index));
                done.insert("summary_index".into(), json!(0));
                done.insert("text".into(), Value::String(text.clone()));
                out.push(self.event("response.reasoning_summary_text.done", done));
                let mut part_done = Map::new();
                part_done.insert("item_id".into(), Value::String(id.clone()));
                part_done.insert("output_index".into(), json!(output_index));
                part_done.insert("summary_index".into(), json!(0));
                part_done.insert(
                    "part".into(),
                    json!({ "type": "summary_text", "text": text }),
                );
                out.push(self.event("response.reasoning_summary_part.done", part_done));
                self.envelope
                    .output
                    .push(reasoning_output_item(&id, &text, &signature));
            }
            OpenItem::Tool {
                id,
                call_id,
                name,
                arguments,
            } => {
                let mut done = Map::new();
                done.insert("item_id".into(), Value::String(id.clone()));
                done.insert("output_index".into(), json!(output_index));
                done.insert("arguments".into(), Value::String(arguments.clone()));
                out.push(self.event("response.function_call_arguments.done", done));
                self.envelope
                    .output
                    .push(function_call_output_item(&id, &call_id, &name, &arguments));
            }
        }
        let item = self.envelope.output.last().cloned().unwrap_or(Value::Null);
        let mut fields = Map::new();
        fields.insert("output_index".into(), json!(output_index));
        fields.insert("item".into(), item);
        out.push(self.event("response.output_item.done", fields));
        out
    }

    fn message_delta(&mut self, event: &Value) {
        if let Some(reason) = event
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
        {
            self.stop_reason = reason.to_string();
        }
        self.usage.merge(event.get("usage"));
    }

    fn terminal_event(&mut self) -> Vec<Value> {
        self.terminal = true;
        let details = incomplete_details(&self.stop_reason);
        let (kind, status) = if details.is_some() {
            ("response.incomplete", "incomplete")
        } else {
            ("response.completed", "completed")
        };
        let usage = self.usage.render();
        let response = self.envelope.status_json(status, details, Some(usage));
        let mut fields = Map::new();
        fields.insert("response".into(), response);
        vec![self.event(kind, fields)]
    }

    fn failed_event(&mut self, event: &Value) -> Value {
        self.terminal = true;
        let message = event
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("the upstream stream failed");
        let mut response = self
            .envelope
            .status_json("failed", None, Some(self.usage.render()));
        response["error"] = json!({ "code": "server_error", "message": message });
        let mut fields = Map::new();
        fields.insert("response".into(), response);
        self.event("response.failed", fields)
    }
}

/// Translate a buffered Messages SSE body into Responses events.
pub fn translate_responses_sse(model: &str, body: &[u8]) -> Vec<Value> {
    let mut translator = ResponsesStreamTranslator::new(model);
    let mut out = translator.push_body(body);
    out.extend(translator.finish());
    out
}

/// Render a buffered Messages body as a Responses `response` HTTP body.
pub fn render_response_body(model: &str, body: &[u8]) -> Result<Vec<u8>, IngressError> {
    let translated = translate_responses_response(body, Some(model))?;
    Ok(serde_json::to_vec(&translated).unwrap_or_default())
}

/// Render a buffered Messages SSE body as an `event: response.*` frame stream.
pub fn render_response_stream(model: &str, body: &[u8]) -> Vec<u8> {
    let mut rendered = String::new();
    for event in translate_responses_sse(model, body) {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            .to_string();
        if let Ok(encoded) = serde_json::to_string(&event) {
            rendered.push_str("event: ");
            rendered.push_str(&kind);
            rendered.push_str("\ndata: ");
            rendered.push_str(&encoded);
            rendered.push_str("\n\n");
        }
    }
    rendered.into_bytes()
}

/// Render the Responses error envelope for a refusal.
///
/// Responses and Chat Completions share `{"error": {...}}`, but the two are
/// rendered from one place per dialect so a future divergence lands in the
/// dialect that changed rather than in both.
pub fn render_response_error(status: u16, message: &str) -> Vec<u8> {
    crate::openai_ingress::render_chat_error(status, message)
}

/// Dispatch the hidden seam: request translation by default, the response half
/// with `response`.
pub fn run_seam(response: bool, stream: bool, for_model: Option<&str>, http: bool) -> i32 {
    if !response {
        return run_translate_stdin();
    }
    run_translate_response_stdin(stream, for_model, http)
}

/// `clud __openai-responses-translate --response [--stream] [--http]` — the
/// process seam for the response half. Reads the upstream Messages body on
/// stdin; writes one Responses event per line (or the `response` object), or
/// with `--http` the exact bytes the route writes.
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
    let rendered = if http {
        if stream {
            Ok(render_response_stream(model, &body))
        } else {
            render_response_body(model, &body)
        }
    } else if stream {
        let mut rendered = String::new();
        for event in translate_responses_sse(model, &body) {
            if let Ok(encoded) = serde_json::to_string(&event) {
                rendered.push_str(&encoded);
                rendered.push('\n');
            }
        }
        Ok(rendered.into_bytes())
    } else {
        translate_responses_response(&body, requested_model).map(|response| {
            let mut encoded = serde_json::to_vec(&response).unwrap_or_default();
            encoded.push(b'\n');
            encoded
        })
    };
    match rendered {
        Ok(rendered) => match std::io::stdout().write_all(&rendered) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("could not write the translated response: {error}");
                2
            }
        },
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    fn messages_body() -> Value {
        json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-x",
            "content": [
                { "type": "thinking", "thinking": "let me look", "signature": "sig-1" },
                { "type": "text", "text": "hello there" },
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "read_file",
                    "input": { "path": "a.rs" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 7,
                "cache_read_input_tokens": 3
            }
        })
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    #[test]
    fn a_messages_body_becomes_a_responses_object() {
        let body = serde_json::to_vec(&messages_body()).unwrap();
        let response = translate_responses_response(&body, Some("claude-sonnet")).unwrap();
        assert_eq!(response["object"], "response");
        assert_eq!(response["id"], "msg_01");
        assert_eq!(response["model"], "claude-x");
        assert_eq!(response["status"], "completed");
        let output = response["output"].as_array().unwrap();
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["encrypted_content"], "sig-1");
        assert_eq!(output[0]["summary"][0]["text"], "let me look");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["type"], "output_text");
        assert_eq!(output[1]["content"][0]["text"], "hello there");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[2]["name"], "read_file");
        assert_eq!(output[2]["arguments"], "{\"path\":\"a.rs\"}");
        // cached tokens fold into input, as upstream does.
        assert_eq!(response["usage"]["input_tokens"], 13);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            3
        );
        assert_eq!(response["usage"]["total_tokens"], 20);
    }

    #[test]
    fn a_truncated_turn_is_reported_incomplete() {
        let mut body = messages_body();
        body["stop_reason"] = json!("max_tokens");
        let rendered =
            translate_responses_response(&serde_json::to_vec(&body).unwrap(), None).unwrap();
        assert_eq!(rendered["status"], "incomplete");
        assert_eq!(
            rendered["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn a_refusal_stop_reason_is_a_content_filter_incomplete() {
        let mut body = messages_body();
        body["stop_reason"] = json!("refusal");
        let rendered =
            translate_responses_response(&serde_json::to_vec(&body).unwrap(), None).unwrap();
        assert_eq!(rendered["incomplete_details"]["reason"], "content_filter");
    }

    #[test]
    fn a_requested_model_is_echoed_when_the_body_carries_none() {
        let body = json!({ "content": [], "stop_reason": "end_turn" });
        let rendered =
            translate_responses_response(&serde_json::to_vec(&body).unwrap(), Some("my-model"))
                .unwrap();
        assert_eq!(rendered["model"], "my-model");
    }

    #[test]
    fn the_stream_opens_with_created_and_in_progress() {
        let body = sse(&[json!({
            "type": "message_start",
            "message": { "id": "msg_s", "model": "claude-x", "usage": { "input_tokens": 4 } }
        })]);
        let events = translate_responses_sse("claude-x", body.as_bytes());
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "response.created",
                "response.in_progress",
                "response.failed"
            ],
            "{events:?}"
        );
        assert_eq!(events[0]["response"]["id"], "msg_s");
        assert_eq!(events[0]["sequence_number"], 0);
        assert_eq!(events[1]["sequence_number"], 1);
    }

    #[test]
    fn a_streamed_text_block_emits_deltas_and_a_completed_item() {
        let body = sse(&[
            json!({ "type": "message_start", "message": { "id": "msg_s", "model": "claude-x" } }),
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "hello " } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "world" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 2 } }),
            json!({ "type": "message_stop" }),
        ]);
        let events = translate_responses_sse("claude-x", body.as_bytes());
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ],
            "{kinds:?}"
        );
        assert_eq!(events[4]["delta"], "hello ");
        assert_eq!(events[6]["text"], "hello world");
        let completed = &events[9];
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "hello world"
        );
        // sequence_number is monotonic across the whole stream.
        for (position, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], position);
        }
    }

    #[test]
    fn a_streamed_tool_call_assembles_its_arguments() {
        let body = sse(&[
            json!({ "type": "message_start", "message": { "id": "msg_t" } }),
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "tool_use", "id": "toolu_9",
                                       "name": "read_file", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"path\":" } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": "\"a.rs\"}" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } }),
            json!({ "type": "message_stop" }),
        ]);
        let events = translate_responses_sse("claude-x", body.as_bytes());
        let done = events
            .iter()
            .find(|event| event["type"] == "response.function_call_arguments.done")
            .expect("arguments done");
        assert_eq!(done["arguments"], "{\"path\":\"a.rs\"}");
        let item = &events.last().unwrap()["response"]["output"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "toolu_9");
        assert_eq!(item["arguments"], "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn a_streamed_reasoning_block_keeps_its_signature() {
        let body = sse(&[
            json!({ "type": "message_start", "message": { "id": "msg_r" } }),
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "thinking", "thinking": "" } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "thinking_delta", "thinking": "because" } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "signature_delta", "signature": "sig-9" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_stop" }),
        ]);
        let events = translate_responses_sse("claude-x", body.as_bytes());
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert!(
            kinds.contains(&"response.reasoning_summary_text.delta"),
            "{kinds:?}"
        );
        // A signature delta carries no visible text.
        assert!(!kinds.iter().any(|kind| kind.contains("signature")));
        let item = &events.last().unwrap()["response"]["output"][0];
        assert_eq!(item["encrypted_content"], "sig-9");
        assert_eq!(item["summary"][0]["text"], "because");
    }

    #[test]
    fn a_stream_that_ends_without_message_stop_still_seals_once() {
        let body = sse(&[
            json!({ "type": "message_start", "message": { "id": "msg_cut" } }),
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "half" } }),
        ]);
        let events = translate_responses_sse("claude-x", body.as_bytes());
        let failed = events
            .iter()
            .filter(|event| event["type"] == "response.failed")
            .count();
        assert_eq!(failed, 1, "{events:?}");
        assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    }

    #[test]
    fn a_streamed_error_event_fails_the_response() {
        let body = sse(&[
            json!({ "type": "message_start", "message": { "id": "msg_e" } }),
            json!({ "type": "error",
                    "error": { "type": "overloaded_error", "message": "overloaded" } }),
        ]);
        let events = translate_responses_sse("claude-x", body.as_bytes());
        let failed = events
            .iter()
            .find(|event| event["type"] == "response.failed")
            .expect("failed event");
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(failed["response"]["error"]["message"], "overloaded");
    }

    #[test]
    fn a_redacted_thinking_block_round_trips_through_the_marker() {
        let body = json!({
            "content": [{ "type": "redacted_thinking", "data": "abc" }],
            "stop_reason": "end_turn"
        });
        let rendered =
            translate_responses_response(&serde_json::to_vec(&body).unwrap(), None).unwrap();
        assert_eq!(
            rendered["output"][0]["encrypted_content"],
            "claude-redacted-thinking:abc"
        );
    }

    #[test]
    fn the_http_stream_frames_every_event_and_ends_with_completed() {
        let body = sse(&[
            json!({ "type": "message_start", "message": { "id": "msg_h" } }),
            json!({ "type": "message_stop" }),
        ]);
        let rendered = render_response_stream("claude-x", body.as_bytes());
        let text = String::from_utf8(rendered).unwrap();
        assert!(text.contains("event: response.created\n"), "{text}");
        assert!(text.contains("event: response.completed\n"), "{text}");
        assert!(text.ends_with("\n\n"), "{text:?}");
        // Every frame carries the event name it is labelled with.
        for frame in text.split("\n\n").filter(|frame| !frame.is_empty()) {
            let name = frame
                .lines()
                .find_map(|line| line.strip_prefix("event: "))
                .expect("frame name");
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("frame data");
            let event: Value = serde_json::from_str(data).unwrap();
            assert_eq!(event["type"], name, "{frame}");
        }
    }

    #[test]
    fn the_renderer_rejects_a_body_that_is_not_json() {
        assert!(render_response_body("claude-x", b"not json").is_err());
    }
}

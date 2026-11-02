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

//! OpenAI legacy text-completions client ingress: one `POST /v1/completions`
//! body turned into an Anthropic Messages request, and the answer turned back
//! into a `text_completion` body.
//!
//! Ports CLIProxyAPI `sdk/api/handlers/openai/openai_handlers.go`
//! (`Completions` :172, `convertCompletionsRequestToChatCompletions` :203,
//! `convertChatCompletionsResponseToCompletions` :268), served at
//! `internal/api/server_routes.go:74` (`v1.POST("/completions")`). Upstream
//! implements the door as a thin adapter over its chat-completions handler;
//! clud does the same on top of its own translator
//! ([`crate::openai_ingress::translate_chat_request`] /
//! [`crate::openai_ingress::translate_chat_response`]), so the completions door
//! inherits the OpenAI ingress mapping instead of growing a second one.
//!
//! Deliberate differences from upstream, all in clud's existing style (refuse by
//! name instead of silently changing what the caller asked for):
//!
//! * `prompt` may be a string or an array of strings (joined with newlines).
//!   Upstream reads it with gjson's `.String()`, which yields `""` for an array
//!   and then substitutes the literal prompt `"Complete this:"`; a token-id
//!   array is refused here by name instead of being replaced by that guess.
//! * an empty prompt is refused (`MissingPrompt`) rather than served with
//!   upstream's invented `"Complete this:"` prefix.
//! * `n > 1`, `echo: true`, `suffix`, `best_of`, `logprobs: true` and
//!   `top_logprobs` are refused by name: Anthropic Messages returns exactly one
//!   choice, cannot echo a prompt, and reports no token log-probabilities, so
//!   answering them would be a different answer than the caller asked for.

use serde_json::{json, Map, Value};

use crate::openai_ingress::{translate_chat_request, translate_chat_response, IngressError};

/// Translate one OpenAI completions request body into an Anthropic Messages
/// body.
///
/// The completions shape carries a single `prompt` and no conversation, so the
/// adapter rewrites it into the one-user-turn chat request upstream builds and
/// hands that to the chat translator: sampling, `stop`, `max_tokens` and
/// `stream` all keep the mapping they already have there.
pub fn translate_completions_request(body: &[u8]) -> Result<Value, IngressError> {
    let request: Value = serde_json::from_slice(body).map_err(|_| IngressError::NotJson)?;
    let request = request.as_object().ok_or(IngressError::NotJson)?;

    let prompt = completions_prompt(request.get("prompt"))?;
    refuse_unsupported_fields(request)?;

    let mut chat = Map::new();
    if let Some(model) = request.get("model") {
        chat.insert("model".into(), model.clone());
    }
    chat.insert(
        "messages".into(),
        json!([{ "role": "user", "content": prompt }]),
    );
    // Copied verbatim: `translate_chat_request` owns what each one becomes.
    for key in ["max_tokens", "temperature", "top_p", "stop", "stream"] {
        if let Some(value) = request.get(key) {
            chat.insert(key.into(), value.clone());
        }
    }
    let chat = serde_json::to_vec(&Value::Object(chat)).map_err(|_| IngressError::NotJson)?;
    translate_chat_request(&chat)
}

/// The prompt as one string.
///
/// OpenAI accepts a string, an array of strings, or an array of token ids. Only
/// the first two have a text spelling; a token array would need the provider's
/// own tokenizer, so it is refused by name.
fn completions_prompt(prompt: Option<&Value>) -> Result<String, IngressError> {
    let text = match prompt {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            let mut lines: Vec<&str> = Vec::with_capacity(parts.len());
            for part in parts {
                match part.as_str() {
                    Some(line) => lines.push(line),
                    None => return Err(IngressError::UnsupportedField("prompt:token_ids".into())),
                }
            }
            lines.join("\n")
        }
        Some(Value::Null) | None => String::new(),
        Some(_) => return Err(IngressError::UnsupportedField("prompt".into())),
    };
    if text.trim().is_empty() {
        return Err(IngressError::MissingPrompt);
    }
    Ok(text)
}

/// Refuse the completions-only knobs an Anthropic Messages answer cannot honour.
///
/// Each one is named so the client learns which field cost it the request,
/// instead of receiving a silently different answer.
fn refuse_unsupported_fields(request: &Map<String, Value>) -> Result<(), IngressError> {
    if request
        .get("n")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 1)
    {
        return Err(IngressError::UnsupportedField("n".into()));
    }
    for flag in ["echo", "logprobs"] {
        if request.get(flag).and_then(Value::as_bool) == Some(true) {
            return Err(IngressError::UnsupportedField(flag.into()));
        }
    }
    for field in ["suffix", "best_of", "top_logprobs"] {
        if request.get(field).is_some_and(|value| !value.is_null()) {
            return Err(IngressError::UnsupportedField(field.into()));
        }
    }
    Ok(())
}

/// Translate one buffered Anthropic Messages body into a `text_completion`.
///
/// `model` is the model the client asked for; the chat translation this is built
/// on keeps the model the upstream body reported when it names one, so the two
/// OpenAI doors cannot disagree about what a reply is called.
pub fn translate_completions_response(
    body: &[u8],
    requested_model: Option<&str>,
) -> Result<Value, IngressError> {
    let chat = translate_chat_response(body, requested_model)?;
    let mut out = Map::new();
    for key in ["id", "created", "model", "usage"] {
        if let Some(value) = chat.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    out.insert("object".into(), json!("text_completion"));
    let choices = chat
        .get("choices")
        .and_then(Value::as_array)
        .map(|choices| choices.iter().map(completion_choice).collect::<Vec<_>>())
        .unwrap_or_default();
    out.insert("choices".into(), Value::Array(choices));
    Ok(Value::Object(out))
}

/// One `chat.completion` choice as a `text_completion` choice: same index and
/// finish reason, the assistant text as `text`, and `logprobs: null` because
/// the upstream reports none.
fn completion_choice(choice: &Value) -> Value {
    let text = choice
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "index": choice.get("index").and_then(Value::as_u64).unwrap_or(0),
        "text": text,
        "logprobs": Value::Null,
        "finish_reason": choice.get("finish_reason").cloned().unwrap_or(Value::Null),
    })
}

/// Translate a whole buffered Messages SSE body into completions chunks.
///
/// The chat translator already turns Anthropic SSE into `chat.completion.chunk`
/// frames; each one is re-shaped here. Reusing it keeps the streamed stop
/// reason, usage and error handling identical to the chat door.
pub fn translate_completions_sse(model: &str, body: &[u8]) -> Vec<Value> {
    crate::openai_ingress::translate_chat_sse(model, body)
        .iter()
        .map(completion_chunk)
        .collect()
}

/// One `chat.completion.chunk` as a `text_completion` chunk.
fn completion_chunk(chunk: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "created", "model"] {
        if let Some(value) = chunk.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    out.insert("object".into(), json!("text_completion"));
    if let Some(usage) = chunk.get("usage") {
        out.insert("usage".into(), usage.clone());
    }
    let choices = chunk
        .get("choices")
        .and_then(Value::as_array)
        .map(|choices| {
            choices
                .iter()
                .map(|choice| {
                    let text = choice
                        .get("delta")
                        .and_then(|delta| delta.get("content"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    json!({
                        "index": choice.get("index").and_then(Value::as_u64).unwrap_or(0),
                        "text": text,
                        "logprobs": Value::Null,
                        "finish_reason": choice.get("finish_reason").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    out.insert("choices".into(), Value::Array(choices));
    Value::Object(out)
}

/// Render one buffered Anthropic Messages body as the `text_completion` body
/// the completions door writes.
pub fn render_completions_body(model: &str, body: &[u8]) -> Result<Vec<u8>, IngressError> {
    let translated = translate_completions_response(body, Some(model))?;
    Ok(serde_json::to_vec(&translated).unwrap_or_default())
}

/// Render one buffered Messages SSE body as the completions stream a client
/// receives: one `data: <chunk>` frame per translated chunk, then OpenAI's
/// `data: [DONE]` sentinel.
pub fn render_completions_stream(model: &str, body: &[u8]) -> Vec<u8> {
    let mut rendered = String::new();
    for chunk in translate_completions_sse(model, body) {
        if let Ok(encoded) = serde_json::to_string(&chunk) {
            rendered.push_str("data: ");
            rendered.push_str(&encoded);
            rendered.push_str("\n\n");
        }
    }
    rendered.push_str("data: [DONE]\n\n");
    rendered.into_bytes()
}

/// The error envelope for this door.
///
/// Completions, chat and Responses all answer a refusal with OpenAI's shared
/// `{"error": {...}}` object, but each dialect renders it in its own place so a
/// future divergence lands where it changed.
pub fn render_completions_error(status: u16, message: &str) -> Vec<u8> {
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

/// Hidden seam: read one completions request body on stdin, write the Messages
/// request body the route would forward to stdout.
pub fn run_translate_stdin() -> i32 {
    use std::io::{Read, Write};

    let mut body = Vec::new();
    if std::io::stdin().read_to_end(&mut body).is_err() {
        eprintln!("could not read the request body from stdin");
        return 2;
    }
    match translate_completions_request(&body) {
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

/// `clud __openai-completions-translate --response [--stream] [--http]` — the
/// process seam for the response half. Reads the upstream Messages body on
/// stdin; writes one completions chunk per line, or with `--http` the exact
/// bytes the route writes.
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
            Ok(render_completions_stream(model, &body))
        } else {
            render_completions_body(model, &body)
        }
    } else if stream {
        let mut rendered = String::new();
        for chunk in translate_completions_sse(model, &body) {
            if let Ok(encoded) = serde_json::to_string(&chunk) {
                rendered.push_str(&encoded);
                rendered.push('\n');
            }
        }
        Ok(rendered.into_bytes())
    } else {
        translate_completions_response(&body, requested_model).map(|response| {
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
mod tests {
    use super::*;

    fn translate(request: Value) -> Value {
        translate_completions_request(request.to_string().as_bytes()).expect("translatable")
    }

    fn refuse(request: Value) -> IngressError {
        translate_completions_request(request.to_string().as_bytes()).expect_err("refused")
    }

    #[test]
    fn a_prompt_becomes_one_user_turn() {
        let out = translate(json!({ "model": "clud-claude-sonnet", "prompt": "say hi" }));
        assert_eq!(out["messages"][0]["role"], "user");
        // The chat translator emits content as text blocks, not a bare string.
        assert_eq!(out["messages"][0]["content"][0]["type"], "text");
        assert_eq!(out["messages"][0]["content"][0]["text"], "say hi");
        assert_eq!(out["model"], "clud-claude-sonnet");
        assert_eq!(out["stream"], false);
    }

    #[test]
    fn sampling_and_stop_keep_the_chat_mapping() {
        let out = translate(json!({
            "model": "m",
            "prompt": "p",
            "max_tokens": 7,
            "temperature": 0.25,
            "top_p": 0.5,
            "stop": "END",
        }));
        assert_eq!(out["max_tokens"], 7);
        assert_eq!(out["temperature"], 0.25);
        assert_eq!(out["top_p"], 0.5);
        assert_eq!(out["stop_sequences"][0], "END");
    }

    #[test]
    fn a_prompt_array_joins_with_newlines() {
        let out = translate(json!({ "model": "m", "prompt": ["a", "b"] }));
        assert_eq!(out["messages"][0]["content"][0]["text"], "a\nb");
    }

    #[test]
    fn a_token_id_prompt_is_refused_by_name() {
        assert_eq!(
            refuse(json!({ "model": "m", "prompt": [[1, 2, 3]] })),
            IngressError::UnsupportedField("prompt:token_ids".into())
        );
    }

    #[test]
    fn an_empty_prompt_is_refused_rather_than_invented() {
        assert_eq!(
            refuse(json!({ "model": "m", "prompt": "  " })),
            IngressError::MissingPrompt
        );
        assert_eq!(refuse(json!({ "model": "m" })), IngressError::MissingPrompt);
    }

    #[test]
    fn a_missing_model_still_fails_as_a_chat_request() {
        assert_eq!(refuse(json!({ "prompt": "p" })), IngressError::MissingModel);
    }

    #[test]
    fn multiple_choices_are_refused_not_narrowed_to_one() {
        assert_eq!(
            refuse(json!({ "model": "m", "prompt": "p", "n": 2 })),
            IngressError::UnsupportedField("n".into())
        );
        // n = 1 is what the gateway answers anyway.
        assert!(translate_completions_request(
            json!({ "model": "m", "prompt": "p", "n": 1 })
                .to_string()
                .as_bytes()
        )
        .is_ok());
    }

    #[test]
    fn echo_and_logprobs_are_refused_only_when_they_ask_for_something() {
        for (field, value) in [
            ("echo", json!(true)),
            ("logprobs", json!(true)),
            ("suffix", json!("...")),
            ("best_of", json!(2)),
            ("top_logprobs", json!(3)),
        ] {
            let error = refuse(json!({ "model": "m", "prompt": "p", field: value }));
            assert_eq!(error, IngressError::UnsupportedField(field.into()));
        }
        for (field, value) in [("echo", json!(false)), ("logprobs", json!(false))] {
            assert!(translate_completions_request(
                json!({ "model": "m", "prompt": "p", field: value })
                    .to_string()
                    .as_bytes()
            )
            .is_ok());
        }
    }

    #[test]
    fn stream_is_forwarded_to_the_messages_body() {
        let out = translate(json!({ "model": "m", "prompt": "p", "stream": true }));
        assert_eq!(out["stream"], true);
    }

    fn messages_response(text: &str) -> Vec<u8> {
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-wire-name",
            "content": [{ "type": "text", "text": text }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 11, "output_tokens": 3 },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn a_messages_reply_becomes_one_text_completion_choice() {
        let out = translate_completions_response(&messages_response("hello"), Some("asked-for"))
            .expect("translatable");
        assert_eq!(out["object"], "text_completion");
        assert_eq!(out["id"], "msg_1");
        // The chat translator keeps the model the upstream body reported; the
        // completions door renders whatever that translation produced.
        assert_eq!(out["model"], "claude-wire-name");
        assert_eq!(out["choices"][0]["text"], "hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["choices"][0]["logprobs"], Value::Null);
        assert_eq!(out["usage"]["prompt_tokens"], 11);
        assert_eq!(out["usage"]["completion_tokens"], 3);
    }

    #[test]
    fn a_non_messages_body_is_a_typed_error() {
        assert_eq!(
            translate_completions_response(b"not json", None),
            Err(IngressError::NotJson)
        );
    }

    fn messages_stream() -> Vec<u8> {
        [
            r#"event: message_start"#,
            r#"data: {"type":"message_start","message":{"id":"msg_2","usage":{"input_tokens":5}}}"#,
            "",
            r#"event: content_block_start"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "",
            r#"event: content_block_delta"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            "",
            r#"event: content_block_stop"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            r#"event: message_delta"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            "",
            r#"event: message_stop"#,
            r#"data: {"type":"message_stop"}"#,
            "",
        ]
        .join("\n")
        .into_bytes()
    }

    #[test]
    fn a_streamed_reply_becomes_text_completion_chunks_and_done() {
        let chunks = translate_completions_sse("asked-for", &messages_stream());
        assert!(!chunks.is_empty());
        assert!(chunks
            .iter()
            .all(|chunk| chunk["object"] == "text_completion"));
        assert!(chunks.iter().all(|chunk| chunk["model"] == "asked-for"));
        let text: String = chunks
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["text"].as_str())
            .collect();
        assert_eq!(text, "hi");
        let finish = chunks
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["finish_reason"].as_str())
            .next_back();
        assert_eq!(finish, Some("stop"));

        let rendered =
            String::from_utf8(render_completions_stream("asked-for", &messages_stream()))
                .expect("utf8");
        assert!(rendered.contains("\"object\":\"text_completion\""));
        assert!(rendered.trim_end().ends_with("data: [DONE]"));
    }

    #[test]
    fn the_error_envelope_is_openai_shaped() {
        let body = render_completions_error(400, "nope");
        let parsed: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(parsed["error"]["message"], "nope");
        assert_eq!(parsed["error"]["type"], "invalid_request_error");
    }
}

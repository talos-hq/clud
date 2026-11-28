//! OpenAI Responses SSE -> Anthropic Messages SSE translation
//! (issue #627 phase 3, step 3).
//!
//! Two layers, deliberately separable so each can be tested without the other:
//!
//! 1. [`FrameDecoder`] is byte-level. It tolerates arbitrary network
//!    fragmentation (including one byte at a time), several frames in one
//!    read, CRLF or LF or bare CR, comment/heartbeat lines, and a final frame
//!    with no trailing blank line.
//! 2. [`StreamTranslator`] is semantic. It maps Responses events onto a valid
//!    Anthropic event sequence with monotonic content-block indices.
//!
//! The invariant that shapes the whole design: **never emit a malformed
//! Anthropic block.** A `content_block_start` for a tool call cannot be sent
//! until the call's id and name are both known, so argument deltas that arrive
//! first are buffered rather than forwarded into a block that does not exist
//! yet. Downstream, a half-formed tool block is worse than a late one -- it
//! makes the client fail to parse a turn it could otherwise have used.
//!
//! Nothing here talks to the network or sees credentials; step 5 wires it into
//! [`crate::codex_bridge`].

use std::collections::HashMap;

use kernal_api::hash::Sha256Hasher as Sha256;

use crate::cache_health::TokenUsage;
use crate::codex_translate;

/// One decoded SSE frame. `event` is absent when the producer sent only data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Decode and remove the longest prefix of `pending` that is valid UTF-8,
/// replacing truly invalid bytes with U+FFFD. An incomplete sequence at the
/// very end stays in `pending` for the next read.
fn take_complete_utf8(pending: &mut Vec<u8>) -> String {
    let mut text = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                text.push_str(valid);
                pending.clear();
                return text;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                text.push_str(std::str::from_utf8(&pending[..valid_up_to]).unwrap_or_default());
                match error.error_len() {
                    // Invalid bytes: replace them and keep decoding.
                    Some(len) => {
                        text.push('\u{FFFD}');
                        pending.drain(..valid_up_to + len);
                    }
                    // An incomplete sequence at the end: wait for more bytes.
                    None => {
                        pending.drain(..valid_up_to);
                        return text;
                    }
                }
            }
        }
    }
}

/// Byte-level SSE framing.
///
/// Line terminators are normalised to `\n` on ingest (the SSE spec treats
/// CRLF, LF, and a bare CR alike). A `\r` at the very end of a read is held
/// back until the next one so a CRLF split across two TCP segments is not
/// mistaken for a bare CR followed by a blank line.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: String,
    pending_cr: bool,
    /// The incomplete UTF-8 tail of the last read, held for the next one so
    /// a character split across TCP reads is not replaced with U+FFFD (#1915).
    pending_bytes: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; return every frame that is now complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        self.pending_bytes.extend_from_slice(bytes);
        let text = take_complete_utf8(&mut self.pending_bytes);
        self.absorb(&text, false);
        self.drain(false)
    }

    /// Flush at end of stream. A producer that closes without a trailing blank
    /// line still has one frame's worth of buffered data, and dropping it would
    /// silently truncate the turn.
    pub fn finish(&mut self) -> Vec<SseFrame> {
        if !self.pending_bytes.is_empty() {
            let tail =
                String::from_utf8_lossy(&std::mem::take(&mut self.pending_bytes)).into_owned();
            self.absorb(&tail, true);
        }
        if self.pending_cr {
            self.buffer.push('\n');
            self.pending_cr = false;
        }
        self.drain(true)
    }

    fn absorb(&mut self, text: &str, _final_chunk: bool) {
        for character in text.chars() {
            match character {
                '\r' => {
                    if self.pending_cr {
                        // Two CRs in a row: the first was a line on its own.
                        self.buffer.push('\n');
                    }
                    self.pending_cr = true;
                }
                '\n' => {
                    // Either the LF of a CRLF pair or a bare LF; one newline
                    // either way.
                    self.pending_cr = false;
                    self.buffer.push('\n');
                }
                other => {
                    if self.pending_cr {
                        self.buffer.push('\n');
                        self.pending_cr = false;
                    }
                    self.buffer.push(other);
                }
            }
        }
    }

    fn drain(&mut self, flush: bool) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        while let Some(position) = self.buffer.find("\n\n") {
            let raw: String = self.buffer.drain(..position + 2).collect();
            if let Some(frame) = parse_frame(&raw) {
                frames.push(frame);
            }
        }
        if flush {
            let remainder: String = std::mem::take(&mut self.buffer);
            if let Some(frame) = parse_frame(&remainder) {
                frames.push(frame);
            }
        }
        frames
    }
}

fn parse_frame(raw: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in raw.split('\n') {
        if line.is_empty() {
            continue;
        }
        // A line beginning with a colon is a comment. Heartbeats arrive this
        // way and must not be mistaken for data.
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value),
            _ => {}
        }
    }
    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseFrame {
        event,
        data: data_lines.join("\n"),
    })
}

// ---------------------------------------------------------------------------
// Semantic translation
// ---------------------------------------------------------------------------

/// One in-flight upstream function call.
#[derive(Debug, Default)]
struct CallState {
    index: Option<u32>,
    call_id: Option<String>,
    name: Option<String>,
    /// Everything received so far, whether buffered or already emitted.
    arguments: String,
    /// How much of `arguments` has been forwarded downstream.
    emitted: usize,
    closed: bool,
}

/// The thinking block currently being assembled from one reasoning item.
#[derive(Debug)]
struct ThinkingState {
    index: u32,
    parts: usize,
    signature: Option<String>,
}

/// A safe classification of an upstream error delivered inside a 200 stream.
///
/// Every field is allowlisted: the raw error message and response body can
/// contain prompt content, account identifiers, or credential fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InBandFailure {
    pub category: &'static str,
    pub code: Option<&'static str>,
    pub request_id: Option<String>,
}

impl InBandFailure {
    fn from_upstream(value: &serde_json::Value) -> Self {
        let code = value
            .pointer("/response/error/code")
            .or_else(|| value.pointer("/error/code"))
            .or_else(|| value.get("code"))
            .and_then(serde_json::Value::as_str);
        let category = match code {
            Some("context_length_exceeded") => "context_length",
            Some("cyber_policy") => "policy",
            _ if value
                .pointer("/response/error/type")
                .or_else(|| value.pointer("/error/type"))
                .and_then(serde_json::Value::as_str)
                == Some("invalid_request") =>
            {
                "malformed_request"
            }
            _ => "unknown_invalid_request",
        };
        let code = match code {
            Some("context_length_exceeded") => Some("context_length_exceeded"),
            Some("cyber_policy") => Some("cyber_policy"),
            _ => None,
        };
        let request_id = value
            .pointer("/response/request_id")
            .or_else(|| value.pointer("/request_id"))
            .or_else(|| value.get("request_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Self {
            category,
            code,
            request_id,
        }
    }

    fn client_message(&self) -> &'static str {
        match self.category {
            "context_length" => "upstream rejected this request because its context is too long",
            "policy" => "upstream rejected this request due to provider policy",
            "malformed_request" => "upstream rejected an unsupported or malformed bridge request",
            _ => "upstream rejected the request as invalid (unclassified; enable CLUD_CODEX_BRIDGE_DEBUG=1 for a safe diagnostic)",
        }
    }
}

/// Responses events -> Anthropic events.
///
/// Emits complete SSE frames as strings so the caller can write and flush each
/// one, which is what makes a turn render progressively.
#[derive(Debug)]
pub struct StreamTranslator {
    model: String,
    message_id: String,
    started: bool,
    finished: bool,
    next_index: u32,
    text_indices: HashMap<(i64, i64), u32>,
    open_blocks: Vec<u32>,
    calls: Vec<CallState>,
    /// Upstream events do not all carry the same identifier, so a call is
    /// reachable by output index, call id, or item id.
    aliases: HashMap<String, usize>,
    thinking: Option<ThinkingState>,
    saw_tool_call: bool,
    has_text_delta: bool,
    input_tokens: u64,
    saw_input_tokens: bool,
    output_tokens: u64,
    cached_tokens: u64,
    stop_reason: Option<String>,
    /// Shortened tool name -> original, so the client sees what it sent.
    tool_names: HashMap<String, String>,
    /// Classification for a provider error delivered after HTTP 200 committed.
    /// The bridge reads this into its operator log but never forwards raw
    /// upstream detail to the harness.
    in_band_failure: Option<InBandFailure>,
    /// Set by every terminal upstream `response.failed` / `error` frame. This
    /// distinguishes a provider-declared failure from an EOF transport fault
    /// even for classifications that need no operator diagnostic.
    in_band_provider_failure: bool,
    /// Set when the stream ended in a drained account or dead credentials --
    /// the two failures a user must act on personally.
    terminal_account_failure: bool,
    authentication_failure: bool,
}

impl StreamTranslator {
    pub fn new(model: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            message_id: message_id.into(),
            started: false,
            finished: false,
            next_index: 0,
            text_indices: HashMap::new(),
            open_blocks: Vec::new(),
            calls: Vec::new(),
            aliases: HashMap::new(),
            thinking: None,
            saw_tool_call: false,
            has_text_delta: false,
            input_tokens: 0,
            saw_input_tokens: false,
            output_tokens: 0,
            cached_tokens: 0,
            stop_reason: None,
            tool_names: HashMap::new(),
            in_band_failure: None,
            in_band_provider_failure: false,
            terminal_account_failure: false,
            authentication_failure: false,
        }
    }

    /// Supply the shortening map produced by the request translator.
    pub fn with_tool_names(mut self, tool_names: HashMap<String, String>) -> Self {
        self.tool_names = tool_names;
        self
    }

    /// Terminal Responses usage in the provider's native accounting
    /// convention. It is intentionally aggregate-only so callers can track
    /// cache health without retaining stream content.
    pub fn terminal_usage(&self) -> Option<TokenUsage> {
        self.saw_input_tokens.then_some(TokenUsage {
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_tokens,
            output_tokens: self.output_tokens,
        })
    }

    /// Translate one upstream frame into zero or more Anthropic frames.
    #[expect(
        clippy::too_many_lines,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    pub fn push(&mut self, frame: &SseFrame) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        let value: serde_json::Value = match serde_json::from_str(&frame.data) {
            Ok(value) => value,
            // `[DONE]` sentinels and other non-JSON payloads carry nothing to
            // translate; they are not errors.
            Err(_) => return Vec::new(),
        };
        let kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .or(frame.event.as_deref())
            .unwrap_or("")
            .to_string();

        let mut out = Vec::new();
        match kind.as_str() {
            "response.created" | "response.in_progress" => self.ensure_started(&mut out),

            "response.output_text.delta" => {
                self.ensure_started(&mut out);
                self.finalize_thinking(&mut out);
                self.has_text_delta = true;
                let key = text_key(&value);
                let index = self.ensure_text_block(key, &mut out);
                if let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) {
                    out.push(text_delta_frame(index, delta));
                }
            }
            "response.content_part.done" | "response.output_text.done" => {
                if let Some(index) = self.text_indices.get(&text_key(&value)).copied() {
                    self.close_block(index, &mut out);
                }
            }

            // --- reasoning -------------------------------------------------
            "response.reasoning_summary_part.added" => {
                self.ensure_started(&mut out);
                if let Some(state) = self.thinking.as_mut() {
                    // Several summary parts belong to ONE thinking block; only
                    // the item's `done` carries the signature that closes it.
                    if state.parts > 0 {
                        let index = state.index;
                        state.parts += 1;
                        out.push(thinking_delta_frame(index, "\n\n"));
                    } else {
                        state.parts += 1;
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.ensure_started(&mut out);
                let index = self.ensure_thinking_block(&mut out);
                if let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) {
                    out.push(thinking_delta_frame(index, delta));
                }
            }

            "response.output_item.added" => {
                self.ensure_started(&mut out);
                let Some(item) = value.get("item") else {
                    return out;
                };
                match item.get("type").and_then(serde_json::Value::as_str) {
                    Some("function_call") => {
                        self.finalize_thinking(&mut out);
                        let slot = self.slot_for(&value, item);
                        merge_identity(&mut self.calls[slot], item);
                        if let Some(seed) =
                            item.get("arguments").and_then(serde_json::Value::as_str)
                        {
                            self.calls[slot].arguments.push_str(seed);
                        }
                        self.saw_tool_call = true;
                        self.open_call(slot, &mut out);
                    }
                    Some("reasoning") => {
                        // Close any previous item so an unterminated block
                        // cannot leak into this one.
                        self.finalize_thinking(&mut out);
                    }
                    _ => {}
                }
            }

            "response.function_call_arguments.delta" => {
                self.ensure_started(&mut out);
                self.finalize_thinking(&mut out);
                let slot = self.slot_for(&value, &serde_json::Value::Null);
                self.saw_tool_call = true;
                if let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) {
                    self.calls[slot].arguments.push_str(delta);
                }
                self.flush_call_arguments(slot, &mut out);
            }
            "response.function_call_arguments.done" => {
                let slot = self.slot_for(&value, &serde_json::Value::Null);
                if let Some(arguments) = value.get("arguments").and_then(serde_json::Value::as_str)
                {
                    // The terminal value is absolute. Adopt it only when it
                    // extends what we already have, so a stream that sent both
                    // deltas and a final value cannot double-count.
                    if arguments.starts_with(self.calls[slot].arguments.as_str()) {
                        self.calls[slot].arguments = arguments.to_string();
                    }
                }
                self.flush_call_arguments(slot, &mut out);
            }

            "response.output_item.done" => {
                let Some(item) = value.get("item") else {
                    return out;
                };
                match item.get("type").and_then(serde_json::Value::as_str) {
                    Some("function_call") => {
                        let slot = self.slot_for(&value, item);
                        merge_identity(&mut self.calls[slot], item);
                        if let Some(arguments) =
                            item.get("arguments").and_then(serde_json::Value::as_str)
                        {
                            if arguments.starts_with(self.calls[slot].arguments.as_str()) {
                                self.calls[slot].arguments = arguments.to_string();
                            }
                        }
                        self.saw_tool_call = true;
                        self.open_call(slot, &mut out);
                        self.flush_call_arguments(slot, &mut out);
                        if let Some(index) = self.calls[slot].index {
                            self.close_block(index, &mut out);
                            self.calls[slot].closed = true;
                        }
                    }
                    Some("reasoning") => {
                        let signature = item
                            .get("encrypted_content")
                            .and_then(serde_json::Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string);
                        if signature.is_some() && self.thinking.is_none() {
                            // A signature with no summary text still has to
                            // reach the client: it is the round-trip carrier.
                            self.ensure_thinking_block(&mut out);
                        }
                        if let Some(state) = self.thinking.as_mut() {
                            state.signature = signature;
                        }
                        self.finalize_thinking(&mut out);
                    }
                    Some("message") if !self.has_text_delta => {
                        // Fallback for a stream that never sent text deltas.
                        if let Some(text) = message_item_text(item) {
                            let index = self.ensure_text_block((0, 0), &mut out);
                            out.push(text_delta_frame(index, &text));
                            self.close_block(index, &mut out);
                        }
                    }
                    _ => {}
                }
            }

            "response.completed" | "response.incomplete" => {
                self.absorb_usage(&value);
                if self.stop_reason.is_none() {
                    self.stop_reason = Some(map_stop_reason(&kind, &value, self.saw_tool_call));
                }
                out.extend(self.finish());
            }
            "response.failed" | "error" => out.extend(self.fail(&value)),
            _ => {}
        }
        out
    }

    /// Close the turn: shut any open blocks, then message_delta/stop.
    pub fn finish(&mut self) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        self.finalize_thinking(&mut out);
        for index in std::mem::take(&mut self.open_blocks) {
            out.push(content_block_stop_frame(index));
        }
        let stop_reason = self.stop_reason.clone().unwrap_or_else(|| {
            if self.saw_tool_call {
                "tool_use".to_string()
            } else {
                "end_turn".to_string()
            }
        });
        // Anthropic's input_tokens excludes cached tokens; the Responses usage
        // includes them. Subtracting keeps clients from double-counting.
        let input_tokens = self.input_tokens.saturating_sub(self.cached_tokens);
        let mut usage = serde_json::json!({
            "input_tokens": input_tokens,
            "output_tokens": self.output_tokens,
        });
        if self.cached_tokens > 0 {
            usage["cache_read_input_tokens"] = serde_json::json!(self.cached_tokens);
        }
        out.push(anthropic_frame(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage,
            }),
        ));
        out.push(anthropic_frame(
            "message_stop",
            serde_json::json!({"type": "message_stop"}),
        ));
        self.finished = true;
        out
    }

    /// Terminate with a sanitized error.
    ///
    /// The upstream body is never echoed: it can carry account identifiers and
    /// key fragments, and this frame is written straight to the harness.
    pub fn fail(&mut self, upstream: &serde_json::Value) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.finalize_thinking(&mut out);
        for index in std::mem::take(&mut self.open_blocks) {
            out.push(content_block_stop_frame(index));
        }
        let failure = InBandFailure::from_upstream(upstream);
        self.in_band_provider_failure = true;
        let kind = upstream_error_type(upstream);
        if kind == "invalid_request_error" {
            self.in_band_failure = Some(failure.clone());
        }
        if matches!(kind, "billing_error" | "authentication_error") {
            self.terminal_account_failure = true;
        }
        if kind == "authentication_error" {
            self.authentication_failure = true;
        }
        out.push(anthropic_frame(
            "error",
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": kind,
                    // Synthesized from the classification, never echoed. The
                    // upstream body can quote account identifiers and key
                    // fragments, so it is read to classify and then dropped
                    // (#630). What was wrong before was not the redaction --
                    // it was that the body was never *read*, so every failure
                    // collapsed onto one constant and an out-of-credits
                    // condition could not be reported as one.
                    "message": if kind == "invalid_request_error" {
                        failure.client_message()
                    } else {
                        upstream_error_message(kind)
                    },
                },
            }),
        ));
        self.finished = true;
        out
    }

    /// Whether the stream ended in a failure the user must act on personally
    /// -- a drained account or dead credentials. The bridge uses this to
    /// surface a quota failure that arrived *inside* a 200 SSE stream, which
    /// otherwise produces HTTP 200 and no diagnostic anywhere.
    pub fn terminal_account_failure(&self) -> bool {
        self.terminal_account_failure
    }

    pub fn authentication_failure(&self) -> bool {
        self.authentication_failure
    }

    /// Whether an upstream terminal error event was translated.
    pub fn has_in_band_provider_failure(&self) -> bool {
        self.in_band_provider_failure
    }

    /// Return a redacted error classification for bridge diagnostics.
    pub fn in_band_failure(&self) -> Option<&InBandFailure> {
        self.in_band_failure.as_ref()
    }

    /// Terminate with a sanitized error and no upstream detail.
    pub fn fail_opaque(&mut self) -> Vec<String> {
        self.fail(&serde_json::Value::Null)
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn ensure_started(&mut self, out: &mut Vec<String>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(anthropic_frame(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                },
            }),
        ));
    }

    fn ensure_text_block(&mut self, key: (i64, i64), out: &mut Vec<String>) -> u32 {
        if let Some(index) = self.text_indices.get(&key) {
            return *index;
        }
        let index = self.allocate_index();
        self.text_indices.insert(key, index);
        self.open_blocks.push(index);
        out.push(anthropic_frame(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
        index
    }

    fn ensure_thinking_block(&mut self, out: &mut Vec<String>) -> u32 {
        if let Some(state) = self.thinking.as_ref() {
            return state.index;
        }
        let index = self.allocate_index();
        self.open_blocks.push(index);
        self.thinking = Some(ThinkingState {
            index,
            parts: 1,
            signature: None,
        });
        out.push(anthropic_frame(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "thinking", "thinking": ""},
            }),
        ));
        index
    }

    /// Close the open thinking block, emitting its signature exactly once.
    ///
    /// The signature is the reasoning item's `encrypted_content` verbatim --
    /// that is what the client echoes back so the next turn can carry the
    /// reasoning forward under `store: false`.
    fn finalize_thinking(&mut self, out: &mut Vec<String>) {
        let Some(state) = self.thinking.take() else {
            return;
        };
        if let Some(signature) = state.signature.as_deref() {
            out.push(anthropic_frame(
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": state.index,
                    "delta": {"type": "signature_delta", "signature": signature},
                }),
            ));
        }
        self.close_block(state.index, out);
    }

    /// Resolve the slot for a call, registering every identifier the event
    /// carries so a later event can find it by any of them.
    fn slot_for(&mut self, event: &serde_json::Value, item: &serde_json::Value) -> usize {
        let mut keys = Vec::new();
        if let Some(index) = event
            .get("output_index")
            .and_then(serde_json::Value::as_i64)
        {
            keys.push(format!("output:{index}"));
        }
        for source in [event, item] {
            for field in ["call_id", "item_id", "id"] {
                if let Some(value) = source.get(field).and_then(serde_json::Value::as_str) {
                    if !value.is_empty() {
                        let prefix = if field == "call_id" { "call" } else { "item" };
                        keys.push(format!("{prefix}:{value}"));
                    }
                }
            }
        }
        let slot = keys
            .iter()
            .find_map(|key| self.aliases.get(key).copied())
            .unwrap_or_else(|| {
                self.calls.push(CallState::default());
                self.calls.len() - 1
            });
        for key in keys {
            self.aliases.insert(key, slot);
        }
        slot
    }

    /// Open a tool block once -- and only once -- its id and name are known.
    fn open_call(&mut self, slot: usize, out: &mut Vec<String>) {
        let call = &self.calls[slot];
        if call.index.is_some() || call.closed {
            return;
        }
        let (Some(call_id), Some(name)) = (call.call_id.clone(), call.name.clone()) else {
            return;
        };
        // Restore the client's original tool name if we shortened it.
        let name = self.tool_names.get(&name).cloned().unwrap_or(name);
        let index = self.allocate_index();
        self.calls[slot].index = Some(index);
        self.open_blocks.push(index);
        out.push(anthropic_frame(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "tool_use",
                    "id": sanitize_tool_id(&call_id),
                    "name": name,
                    "input": {},
                },
            }),
        ));
    }

    /// Forward whatever argument text has not been sent yet.
    fn flush_call_arguments(&mut self, slot: usize, out: &mut Vec<String>) {
        let Some(index) = self.calls[slot].index else {
            return;
        };
        let call = &mut self.calls[slot];
        if call.emitted >= call.arguments.len() {
            return;
        }
        let pending = call.arguments[call.emitted..].to_string();
        call.emitted = call.arguments.len();
        out.push(anthropic_frame(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": pending},
            }),
        ));
    }

    fn close_block(&mut self, index: u32, out: &mut Vec<String>) {
        if let Some(position) = self.open_blocks.iter().position(|open| *open == index) {
            self.open_blocks.remove(position);
            out.push(content_block_stop_frame(index));
        }
    }

    fn allocate_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn absorb_usage(&mut self, value: &serde_json::Value) {
        let Some(usage) = value.pointer("/response/usage") else {
            return;
        };
        if let Some(input) = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            self.input_tokens = input;
            self.saw_input_tokens = true;
        }
        if let Some(output) = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            self.output_tokens = output;
        }
        if let Some(cached) = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            self.cached_tokens = cached;
        }
    }
}

fn text_key(value: &serde_json::Value) -> (i64, i64) {
    (
        value
            .get("output_index")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        value
            .get("content_index")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
    )
}

fn message_item_text(item: &serde_json::Value) -> Option<String> {
    let parts = item.get("content")?.as_array()?;
    let text: String = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
        .collect();
    (!text.is_empty()).then_some(text)
}

/// True for a byte legal in a client `tool_use.id`.
///
/// clud's own Anthropic-facing routes accept `^[a-zA-Z0-9_-]+$` for this
/// field; that is the invariant enforced here and by the tests in this
/// module, not a claim about every downstream Anthropic client.
fn is_legal_tool_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Rewrite an upstream `call_id` into a client-legal `tool_use.id`.
///
/// Must be injective (distinct upstream ids never collapse onto one id --
/// see #1535, where two ids differing only in punctuation both sanitized to
/// `a_b`) and idempotent (a client echoes this id back on a later turn as
/// part of the tool_use/tool_result pair, and the request direction passes
/// it straight through rather than re-encoding, so re-running this function
/// on its own output must be a no-op).
///
/// An id already legal and within the length cap passes through unchanged
/// (mod `shorten_identifier`), which keeps the common case byte-identical
/// and is what makes the second application above a no-op. An id with
/// illegal bytes, or an empty id, is masked byte-for-byte (illegal -> `_`)
/// for readability and then disambiguated with a hash of the *original*
/// bytes, since the mask alone is not injective (`a.b`, `a/b`, and `a:b`
/// all mask to `a_b`). The masked-and-hashed form is itself always legal
/// and within the length cap, so it takes the fast path -- and is therefore
/// a fixed point -- on any later application.
fn sanitize_tool_id(id: &str) -> String {
    if !id.is_empty() && id.bytes().all(is_legal_tool_id_byte) {
        return codex_translate::shorten_identifier(id);
    }
    let masked: String = id
        .bytes()
        .map(|byte| {
            if is_legal_tool_id_byte(byte) {
                byte as char
            } else {
                '_'
            }
        })
        .collect();
    let digest = Sha256::digest(id.as_bytes());
    let suffix: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // `masked` is pure ASCII (every byte maps to an ASCII char above), so
    // byte and char boundaries coincide and this slice never panics.
    let keep = codex_translate::MAX_IDENTIFIER_LEN.saturating_sub(suffix.len() + 1);
    let boundary = keep.min(masked.len());
    format!("{}_{}", &masked[..boundary], suffix)
}

fn map_stop_reason(kind: &str, value: &serde_json::Value, saw_tool_call: bool) -> String {
    // A turn that produced a tool call is a tool_use turn regardless of what
    // the transport called it.
    if saw_tool_call {
        return "tool_use".to_string();
    }
    let raw = value
        .pointer("/response/stop_reason")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .pointer("/response/incomplete_details/reason")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("");
    if raw.is_empty() && kind == "response.incomplete" {
        // An incomplete response with no stated reason is, in practice, a
        // length cutoff.
        return "max_tokens".to_string();
    }
    match raw {
        "max_tokens" | "max_output_tokens" => "max_tokens",
        "content_filter" => "refusal",
        "end_turn"
        | "stop_sequence"
        | "pause_turn"
        | "refusal"
        | "model_context_window_exceeded" => raw,
        // "", "stop", "completed", "tool_use", "tool_calls", "function_call"
        // and anything unrecognised.
        _ => "end_turn",
    }
    .to_string()
}

fn upstream_error_type(value: &serde_json::Value) -> &'static str {
    let code = value
        .pointer("/response/error/code")
        .or_else(|| value.pointer("/error/code"))
        .or_else(|| value.get("code"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let kind = value
        .pointer("/response/error/type")
        .or_else(|| value.pointer("/error/type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match (code, kind) {
        ("invalid_token" | "token_expired" | "invalid_api_key" | "unauthorized", _)
        | (_, "authentication_error" | "invalid_token") => "authentication_error",
        ("cyber_policy", _) | ("invalid_request", _) | (_, "invalid_request") => {
            "invalid_request_error"
        }
        ("rate_limit_exceeded", _) => "rate_limit_error",
        // `usage_limit_reached` is how the ChatGPT backend spells an
        // exhausted plan; it is a billing condition, not a throttle.
        ("insufficient_quota", _) | ("usage_limit_reached", _) | ("quota_exceeded", _) => {
            "billing_error"
        }
        ("context_length_exceeded", _) => "invalid_request_error",
        _ => "api_error",
    }
}

/// A safe, actionable message per error class.
///
/// Derived, never echoed: every string here is one we wrote.
fn upstream_error_message(kind: &str) -> &'static str {
    match kind {
        "billing_error" => {
            "upstream account quota exhausted -- check your plan usage, or switch providers with --claude"
        }
        "rate_limit_error" => "upstream rate limited this request",
        "invalid_request_error" => "upstream rejected the request as invalid",
        "authentication_error" => "upstream rejected the bridge's credentials",
        _ => "upstream provider error",
    }
}

fn merge_identity(call: &mut CallState, item: &serde_json::Value) {
    // `call_id` is the identifier a follow-up tool_result must quote; `id` is
    // the item's own handle. Prefer the former, accept the latter.
    if call.call_id.is_none() {
        if let Some(call_id) = item
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| item.get("id").and_then(serde_json::Value::as_str))
        {
            if !call_id.is_empty() {
                call.call_id = Some(call_id.to_string());
            }
        }
    }
    if call.name.is_none() {
        if let Some(name) = item.get("name").and_then(serde_json::Value::as_str) {
            if !name.is_empty() {
                call.name = Some(name.to_string());
            }
        }
    }
}

fn text_delta_frame(index: u32, text: &str) -> String {
    anthropic_frame(
        "content_block_delta",
        serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text},
        }),
    )
}

fn thinking_delta_frame(index: u32, text: &str) -> String {
    anthropic_frame(
        "content_block_delta",
        serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": text},
        }),
    )
}

fn content_block_stop_frame(index: u32) -> String {
    anthropic_frame(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": index}),
    )
}

pub(crate) fn anthropic_frame(event: &str, body: serde_json::Value) -> String {
    format!("event: {event}\ndata: {body}\n\n")
}

#[cfg(test)]
#[path = "codex_sse_tests.rs"]
mod tests;

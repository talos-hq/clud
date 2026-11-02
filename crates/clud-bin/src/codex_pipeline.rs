//! End-to-end request pipeline for the Codex bridge (issue #627 phase 3,
//! step 5).
//!
//! Chains the four pieces built in steps 1-4 into one call:
//!
//! ```text
//! Anthropic request -> codex_translate -> codex_upstream -> codex_sse -> Anthropic SSE
//! ```
//!
//! Upstream is *always* streamed, even for a non-streaming Messages request.
//! Aggregating the translated Anthropic events into a final `Message` reuses
//! the state machine that step 3 already fuzzed, rather than adding a second,
//! separately-wrong mapping for the non-streaming shape.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{collections::BTreeMap, collections::HashMap, collections::HashSet};

use crate::cache_health::TokenUsage;
use crate::codex_auth::{REFRESH_MALFORMED, REFRESH_REJECTED, REFRESH_TEMPORARY};
use crate::codex_history::{ConversationHistory, HistoryError};
use crate::codex_model::ModelSpec;
use crate::codex_sse::{FrameDecoder, InBandFailure, StreamTranslator};
use crate::codex_translate::{
    default_model_spec, translate_bytes, CompactResponse, CompactionError, ResponsesRequest,
    SystemPlacement, TranslateError, TranslateOptions,
};
use crate::codex_upstream::{
    CredentialSource, FailureClass, UpstreamClient, UpstreamError, UpstreamFailure, UpstreamTarget,
    CLUD_CREDENTIALS_EXPIRED, CREDENTIALS_EXPIRED,
};

/// "Client Closed Request". Not in the RFC status registry, but the
/// conventional code for it and unambiguous in a log — and by the time it is
/// chosen there is normally no reader left to receive it anyway.
const CLIENT_CLOSED_REQUEST: u16 = 499;

/// What the client is told when the account is out of quota.
///
/// Names the condition and an action. The previous text, "upstream provider
/// returned status 429", named neither -- and read identically for an ordinary
/// throttle, which is how a drained account went unnoticed for hours.
const QUOTA_EXHAUSTED_MESSAGE: &str =
    "upstream account quota exhausted -- check your plan usage, or switch providers with --claude";

/// What a completed stream is worth reporting even though it succeeded at the
/// HTTP layer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamSummary {
    /// Effective Responses model sent to the provider. It is public catalog
    /// metadata and lets the session HUD avoid displaying the Claude alias.
    pub model: String,
    /// Terminal provider usage, if the Responses stream supplied a complete
    /// input count. This contains aggregates only and is safe for health
    /// accounting at the bridge boundary.
    pub usage: Option<TokenUsage>,
    /// The turn ended in a drained account or dead credentials, delivered
    /// in-band. HTTP status is already committed to 200 by then, so this is
    /// the only way the caller can learn it happened.
    pub terminal_account_failure: bool,
    /// The provider rejected the upstream bearer inside an HTTP 200 stream.
    pub authentication_failure: bool,
    /// Safe upstream classification captured from an in-band 400. This is
    /// operator-only metadata; the raw error body never leaves the translator.
    pub in_band_failure: Option<InBandFailure>,
    /// Field names, item kinds, and counts from the translated Responses
    /// request. It intentionally omits all user/model/tool values.
    pub request_shape: serde_json::Value,
    /// How many orphaned tool results this turn had to rewrite into user
    /// messages (compaction wedge — see `repair_orphaned_outputs`). Non-zero
    /// means the session was about to be permanently 400ed by the Responses
    /// API, so the bridge logs it.
    pub orphaned_outputs_repaired: usize,
    /// Real tool outputs recovered from Claude's full replay after a failed
    /// turn moved them behind the final-assistant pending-input boundary.
    /// Counts only; identifiers and payloads never leave this module.
    pub pending_outputs_recovered: usize,
    /// Complete upstream `response.output_item.done` items in wire order.
    /// These are canonical Responses transcript entries and must remain opaque.
    pub output_items: Vec<serde_json::Value>,
    /// The completed turn reached the client, but could not fit in the bounded
    /// canonical transcript. The bridge must clear this conversation only after
    /// committing its downstream reply, so the next replay seeds fresh history.
    pub history_append_rejected: bool,
}

impl StreamSummary {
    /// Clears the canonical transcript after a client-visible capacity rejection.
    /// This remains in the request's `with_history` closure so no concurrent
    /// replay can observe stale history after the rejected turn's output commits.
    pub fn clear_history_after_client_commit(&self, history: &mut ConversationHistory) {
        if self.history_append_rejected {
            history.clear();
        }
    }
}

/// A completed non-streaming reply and its canonical-history disposition.
#[derive(Debug)]
pub struct Completion {
    pub message: serde_json::Value,
    pub model: String,
    pub usage: Option<TokenUsage>,
    pub history_append_rejected: bool,
    pub pending_outputs_recovered: usize,
}

impl Completion {
    /// See [`StreamSummary::clear_history_after_client_commit`].
    pub fn clear_history_after_client_commit(&self, history: &mut ConversationHistory) {
        if self.history_append_rejected {
            history.clear();
        }
    }
}

/// Operator-only diagnostic paired with an in-band invalid request.
///
/// Boxing it keeps the hot `PipelineError` result compact while preserving the
/// bounded, allowlisted failure metadata for the HTTP boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InBandDiagnostic {
    pub failure: InBandFailure,
    pub request_shape: serde_json::Value,
}

/// A provider failure delivered *inside* an otherwise-successful stream.
///
/// The ChatGPT backend commonly reports quota exhaustion this way: HTTP 200,
/// then a `response.failed` event. Both fields are ours -- the type comes from
/// classifying the upstream error object, the message is synthesized from that
/// type. No upstream byte is carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub kind: String,
    pub message: String,
    /// Operator-only metadata retained from an in-band invalid request. This
    /// cannot contain an upstream body; [`InBandFailure`] is allowlisted.
    pub diagnostic: Option<Box<InBandDiagnostic>>,
}

/// Sanitized description of an invalid canonical continuation assembled by
/// the bridge before it reaches the upstream Responses endpoint.
///
/// Only fixed item kinds and counts are retained. Function-call identifiers,
/// tool payloads, and Messages content are deliberately absent so this value
/// can be written to the always-on forensic log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationInvariantFailure {
    pub input_count: usize,
    pub input_kinds: BTreeMap<&'static str, usize>,
    pub function_call_count: usize,
    pub function_call_output_count: usize,
    pub unmatched_call_count: usize,
    /// Tool results whose originating call is absent. The *opposite*
    /// invariant to `unmatched_call_count`, and the one the Responses API
    /// rejects with "No tool call found for function call output".
    /// Tracked separately so the bridge log can tell them apart — the log is
    /// what identified this failure in the first place.
    pub orphaned_output_count: usize,
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PipelineError {
    #[error("{0}")]
    Translate(TranslateError),
    #[error("{0}")]
    Compaction(CompactionError),
    #[error("{0}")]
    Upstream(UpstreamError),
    #[error("invalid canonical continuation: {} function call(s) have no tool output", .0.unmatched_call_count)]
    ContinuationInvariant(Box<ContinuationInvariantFailure>),
    /// An in-band provider failure, classified rather than flattened.
    #[error("{}", .0.message)]
    Provider(ProviderFailure),
}

impl PipelineError {
    /// Whether the provider supplied the exact context-window error code.
    pub fn is_context_length_exceeded(&self) -> bool {
        match self {
            Self::Upstream(error) => error.is_context_length_exceeded(),
            Self::Provider(failure) => failure.diagnostic.as_ref().is_some_and(|diagnostic| {
                diagnostic.failure.code == Some("context_length_exceeded")
            }),
            Self::Translate(_) | Self::Compaction(_) | Self::ContinuationInvariant(_) => false,
        }
    }

    /// Downstream HTTP status for a failure that happens *before* any output.
    ///
    /// Once a frame has been flushed there is no status left to choose, which
    /// is why the streaming path reports late failures as an SSE `error` event
    /// instead (see [`StreamTranslator::fail`]).
    pub fn http_status(&self) -> u16 {
        match self {
            // Translation is total (#750 A1), so the only translate failure
            // left is a request that is not a valid Messages request at all.
            Self::Translate(TranslateError::Invalid(_)) => 400,
            Self::ContinuationInvariant(_) => 400,
            Self::Compaction(_) => 502,
            Self::Upstream(UpstreamError::Credentials(REFRESH_TEMPORARY)) => 503,
            Self::Upstream(UpstreamError::Credentials(REFRESH_MALFORMED)) => 502,
            // The caller already authenticated to the bridge. Its bearer is
            // not invalid just because the bridge cannot authenticate upstream.
            Self::Upstream(UpstreamError::Credentials(_)) => 502,
            Self::Upstream(UpstreamError::Status(failure)) => match failure.status() {
                401 => 502,
                400 | 403 | 404 | 413 | 422 | 429 => failure.status(),
                _ => 502,
            },
            // The classification decides the status, not the transport. A
            // quota failure that arrived inside a 200 is still a 429.
            Self::Provider(failure) => match failure.kind.as_str() {
                "billing_error" | "rate_limit_error" => 429,
                "invalid_request_error" => 400,
                "authentication_error" => 502,
                _ => 502,
            },
            Self::Upstream(UpstreamError::CompactMalformed) => 502,
            Self::Upstream(UpstreamError::Timeout) => 504,
            // 502 means "the gateway hop failed", so only the failures that
            // really are gateway failures keep it (#764). The three below are
            // not: an oversized response is a payload problem, and a cancelled
            // or hung-up request has no reader left to serve.
            Self::Upstream(UpstreamError::TooLarge) => 413,
            Self::Upstream(UpstreamError::Cancelled) => CLIENT_CLOSED_REQUEST,
            Self::Upstream(UpstreamError::Downstream(_)) => CLIENT_CLOSED_REQUEST,
            Self::Upstream(UpstreamError::Transport(_)) => 502,
        }
    }

    /// The upstream classification, where the failure carries one.
    ///
    /// Exposed so the HTTP layer can choose an error *type* from what was
    /// classified rather than re-deriving it from a status code that has
    /// already lost the distinction.
    pub fn failure_class(&self) -> Option<FailureClass> {
        match self {
            Self::Upstream(error) => error.failure().map(UpstreamFailure::class),
            Self::Provider(failure) if failure.kind == "billing_error" => {
                Some(FailureClass::Exhausted)
            }
            Self::Translate(_)
            | Self::Compaction(_)
            | Self::ContinuationInvariant(_)
            | Self::Provider(_) => None,
        }
    }

    /// Seconds to put in a `Retry-After` header, if upstream gave a hint.
    pub fn retry_after_seconds(&self) -> Option<u64> {
        match self {
            Self::Upstream(error) => error
                .failure()
                .and_then(|failure| failure.retry_after().or_else(|| failure.resets_in()))
                .map(|hint| hint.as_secs()),
            Self::Translate(_)
            | Self::Compaction(_)
            | Self::ContinuationInvariant(_)
            | Self::Provider(_) => None,
        }
    }

    /// Whether this is a terminal account-level failure -- quota exhausted or
    /// credentials that will not self-heal. These are the two the user must
    /// act on personally; everything else is the bridge's problem.
    pub fn is_terminal_account_failure(&self) -> bool {
        matches!(self.failure_class(), Some(FailureClass::Exhausted))
            || matches!(self, Self::Upstream(UpstreamError::Credentials(what)) if *what != REFRESH_TEMPORARY && *what != REFRESH_MALFORMED)
            || matches!(self, Self::Provider(failure) if failure.kind == "authentication_error")
    }

    /// The classified upstream summary, for the operator log only.
    ///
    /// Never reaches the client: [`Self::client_message`] is what the harness
    /// sees, and it deliberately carries less.
    pub fn upstream_diagnostic(&self) -> Option<String> {
        match self {
            Self::Upstream(error) => error.failure().map(UpstreamFailure::diagnostic),
            Self::Provider(failure) => Some(format!("in-band provider error: {}", failure.kind)),
            Self::Translate(_) | Self::Compaction(_) | Self::ContinuationInvariant(_) => None,
        }
    }

    /// A message safe to hand to the harness. Upstream bodies never appear:
    /// they carry account identifiers and key fragments.
    pub fn client_message(&self) -> String {
        match self {
            // Translation messages are ours and name only the offending
            // feature, so they are worth surfacing -- that is the difference
            // between "the bridge refused top_k" and an opaque 422.
            Self::Translate(error) => error.to_string(),
            Self::ContinuationInvariant(failure) => format!(
                "invalid tool continuation: {} function call(s) have no tool output",
                failure.unmatched_call_count
            ),
            Self::Compaction(_) => {
                "upstream compaction response could not safely recover the transcript".to_string()
            }
            // Already synthesized by the translator from the classification.
            Self::Provider(failure) => failure.message.clone(),
            // An expired login is the one credential failure with an action
            // attached, so it is worth forwarding verbatim. Every other reason
            // names an environment variable and stays behind the generic text.
            Self::Upstream(UpstreamError::Credentials(what))
                if matches!(
                    *what,
                    CLUD_CREDENTIALS_EXPIRED
                        | CREDENTIALS_EXPIRED
                        | REFRESH_TEMPORARY
                        | REFRESH_REJECTED
                        | REFRESH_MALFORMED
                ) =>
            {
                what.to_string()
            }
            Self::Upstream(UpstreamError::Credentials(_)) => {
                "the Codex bridge has no upstream credentials".to_string()
            }
            Self::Upstream(UpstreamError::Status(failure)) => {
                // Lead with what happened, not with a number. "status 429"
                // names no cause, no remedy, and reads identically for an
                // ordinary throttle and a drained account -- which is how a
                // real exhaustion went unnoticed until the balance was gone.
                let mut message = match failure.class() {
                    FailureClass::Exhausted => QUOTA_EXHAUSTED_MESSAGE.to_string(),
                    _ if failure.status() == 429 => {
                        "upstream rate limited this request".to_string()
                    }
                    _ => format!("upstream provider returned status {}", failure.status()),
                };
                // Prefer the server's own `Retry-After` over a body hint; it is
                // the value the retry loop honoured, so the two agree.
                if let Some(resets) = failure.retry_after().or_else(|| failure.resets_in()) {
                    message.push_str(&format!("; retry in {}", humanize(resets)));
                }
                if let Some(id) = failure.request_id() {
                    message.push_str(&format!(" (request-id {id})"));
                }
                message
            }
            Self::Upstream(UpstreamError::CompactMalformed) => {
                "upstream compaction response could not safely recover the transcript".to_string()
            }
            Self::Upstream(UpstreamError::Timeout) => "upstream request timed out".to_string(),
            Self::Upstream(UpstreamError::Transport(_)) => "upstream unreachable".to_string(),
            Self::Upstream(UpstreamError::TooLarge) => {
                "upstream response exceeded the size budget".to_string()
            }
            Self::Upstream(UpstreamError::Cancelled) => "request cancelled".to_string(),
            Self::Upstream(UpstreamError::Downstream(_)) => {
                "downstream client disconnected".to_string()
            }
        }
    }
}

/// Render a duration the way a person reads a clock, not a counter.
///
/// A quota reset is routinely days out, and `442242s` is a number a user has
/// to do arithmetic on before it means anything.
fn humanize(duration: Duration) -> String {
    let total = duration.as_secs();
    let (days, hours, minutes) = (
        total / 86_400,
        (total % 86_400) / 3_600,
        (total % 3_600) / 60,
    );
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{total}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Accumulates translated Anthropic SSE frames into one `Message` object.
///
/// Anthropic's non-streaming reply is exactly the stream's fixed point, so
/// this walks the same events rather than introducing a parallel mapping.
#[derive(Debug, Default)]
pub struct MessageAggregator {
    id: String,
    model: String,
    blocks: Vec<serde_json::Value>,
    text: Vec<String>,
    partial_json: Vec<String>,
    stop_reason: String,
    input_tokens: u64,
    output_tokens: u64,
    errored: bool,
    provider_error: Option<ProviderFailure>,
}

impl MessageAggregator {
    pub fn new() -> Self {
        Self {
            stop_reason: "end_turn".to_string(),
            ..Self::default()
        }
    }

    pub fn push_frame(&mut self, frame: &str) {
        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => {
                if let Some(message) = value.get("message") {
                    self.id = string_at(message, "id");
                    self.model = string_at(message, "model");
                    self.input_tokens = message
                        .pointer("/usage/input_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                }
            }
            Some("content_block_start") => {
                self.flush_open_block();
                if let Some(block) = value.get("content_block") {
                    self.blocks.push(block.clone());
                }
            }
            Some("content_block_delta") => {
                match value.pointer("/delta/type").and_then(|kind| kind.as_str()) {
                    Some("text_delta") => self.text.push(string_at_pointer(&value, "/delta/text")),
                    Some("input_json_delta") => self
                        .partial_json
                        .push(string_at_pointer(&value, "/delta/partial_json")),
                    _ => {}
                }
            }
            Some("content_block_stop") => self.flush_open_block(),
            Some("message_delta") => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
                    self.stop_reason = reason.to_string();
                }
                if let Some(tokens) = value
                    .pointer("/usage/output_tokens")
                    .and_then(serde_json::Value::as_u64)
                {
                    self.output_tokens = tokens;
                }
                // message_start carried a placeholder; the real count only
                // arrives with the terminal usage.
                if let Some(tokens) = value
                    .pointer("/usage/input_tokens")
                    .and_then(serde_json::Value::as_u64)
                {
                    self.input_tokens = tokens;
                }
            }
            Some("error") => {
                self.errored = true;
                // Keep the classification the translator already computed.
                // Reducing it to a bool is what forced `complete` to relabel a
                // billing failure as a transport failure, which then became a
                // 502 `api_error` -- the signal erased by the code holding it.
                if let Some(error) = value.get("error") {
                    self.provider_error = Some(ProviderFailure {
                        kind: string_at(error, "type"),
                        message: string_at(error, "message"),
                        diagnostic: None,
                    });
                }
            }
            _ => {}
        }
    }

    /// Fold the deltas collected since the last block boundary into the block
    /// they belong to.
    fn flush_open_block(&mut self) {
        let text = std::mem::take(&mut self.text).concat();
        let json = std::mem::take(&mut self.partial_json).concat();
        let Some(block) = self.blocks.last_mut() else {
            return;
        };
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") if !text.is_empty() => {
                block["text"] = serde_json::json!(text);
            }
            Some("tool_use") if !json.is_empty() => {
                // Only write when something was buffered. `finish` may flush a
                // block that `content_block_stop` already closed, and an
                // unconditional write would clobber the parsed arguments with
                // the empty-buffer reading.
                // Fragments are only valid JSON once joined. An unparseable
                // join means the turn was truncated; an empty object is the
                // safe reading, since a malformed `input` would make the
                // client reject a turn it could otherwise partly use.
                block["input"] = serde_json::from_str(&json).unwrap_or(serde_json::json!({}));
            }
            _ => {}
        }
    }

    pub fn errored(&self) -> bool {
        self.errored
    }

    pub fn provider_error(&self) -> Option<&ProviderFailure> {
        self.provider_error.as_ref()
    }

    pub fn finish(mut self) -> serde_json::Value {
        self.flush_open_block();
        serde_json::json!({
            "id": self.id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": self.blocks,
            "stop_reason": self.stop_reason,
            "stop_sequence": null,
            "usage": {
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
            },
        })
    }
}

fn string_at(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn string_at_pointer(value: &serde_json::Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

struct AttemptOutcome {
    usage: Option<TokenUsage>,
    terminal_account_failure: bool,
    authentication_failure: bool,
    in_band_failure: Option<InBandFailure>,
    in_band_provider_failure: bool,
    output_items: Vec<serde_json::Value>,
    /// `response.created` is merely a protocol envelope. This becomes true
    /// only when the upstream frame can yield text, reasoning, or a tool call.
    visible: bool,
    /// Frames are withheld until output becomes visible. A pre-output failure
    /// can then recover without leaking a first `message_start` or error.
    deferred_frames: Vec<String>,
    /// A transport-level context failure that happened before any visible
    /// output. It is retained as a control-flow signal so the pipeline can
    /// compact and retry without first emitting a misleading error frame.
    pre_output_failure: Option<UpstreamError>,
}

fn emit_deferred_frames(
    frames: &[String],
    sink: &mut dyn FnMut(&str) -> Result<(), UpstreamError>,
) -> Result<(), PipelineError> {
    for frame in frames {
        sink(frame).map_err(PipelineError::Upstream)?;
    }
    Ok(())
}

fn history_error(error: HistoryError) -> PipelineError {
    PipelineError::Translate(TranslateError::Invalid(error.to_string()))
}

fn record_successful_turn(
    history: &mut ConversationHistory,
    pending_input: &[serde_json::Value],
    output_items: &[serde_json::Value],
) -> Result<bool, PipelineError> {
    match history.append_successful_turn(pending_input, output_items) {
        Ok(()) => Ok(false),
        Err(HistoryError::HistoryLimit | HistoryError::ItemTooLarge) => Ok(true),
        Err(error) => Err(history_error(error)),
    }
}

fn capture_output_item(
    frame: &crate::codex_sse::SseFrame,
    output_items: &mut Vec<serde_json::Value>,
) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
        return;
    };
    if value.get("type").and_then(serde_json::Value::as_str) == Some("response.output_item.done") {
        if let Some(item) = value.get("item") {
            output_items.push(item.clone());
        }
    }
}

fn response_terminated(frame: &crate::codex_sse::SseFrame) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(&frame.data)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref(),
        Some("response.completed" | "response.incomplete")
    )
}

fn anthropic_frames_have_visible_output(frames: &[String]) -> bool {
    frames.iter().any(|frame| {
        matches!(
            frame.lines().next(),
            Some("event: content_block_start" | "event: content_block_delta")
        )
    })
}

fn append_pending_input(input: &mut Vec<serde_json::Value>, pending_input: &[serde_json::Value]) {
    input.extend(pending_input.iter().cloned());
}

fn is_developer_item(item: &serde_json::Value) -> bool {
    item.get("type").and_then(serde_json::Value::as_str) == Some("message")
        && item.get("role").and_then(serde_json::Value::as_str) == Some("developer")
}

fn response_item_kind(item: &serde_json::Value) -> &'static str {
    match item.get("type").and_then(serde_json::Value::as_str) {
        Some("message") => "message",
        Some("function_call") => "function_call",
        Some("function_call_output") => "function_call_output",
        Some("reasoning") => "reasoning",
        Some("compaction") => "compaction",
        _ => "other",
    }
}

fn call_id(item: &serde_json::Value) -> Option<&str> {
    item.get("call_id").and_then(serde_json::Value::as_str)
}

fn consume_matching_output(
    unresolved: &mut HashMap<String, usize>,
    item: &serde_json::Value,
) -> bool {
    if response_item_kind(item) != "function_call_output" {
        return false;
    }
    let Some(id) = call_id(item) else {
        return false;
    };
    let Some(remaining) = unresolved.get_mut(id) else {
        return false;
    };
    *remaining = remaining.saturating_sub(1);
    if *remaining == 0 {
        unresolved.remove(id);
    }
    true
}

/// Recover real outputs that a failed turn left outside the normal pending
/// suffix.
///
/// Canonical history deliberately commits neither input nor partial output
/// when an upstream turn fails. Claude nevertheless persists the tool result
/// that triggered that turn, followed by a synthetic or partial assistant
/// error. On its next request, the final-assistant suffix starts *after* the
/// still-needed result. The complete Messages replay retains it, so reconcile
/// only outputs required by unresolved canonical calls. Calls with no real
/// replayed output remain invalid and are rejected by the validator below.
fn recover_pending_outputs(
    history: &[serde_json::Value],
    translated_input: &[serde_json::Value],
    pending_input: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut unresolved = HashMap::<String, usize>::new();
    for item in history {
        match response_item_kind(item) {
            "function_call" => {
                if let Some(id) = call_id(item) {
                    *unresolved.entry(id.to_string()).or_insert(0) += 1;
                }
            }
            "function_call_output" => {
                consume_matching_output(&mut unresolved, item);
            }
            _ => {}
        }
    }
    for item in pending_input {
        consume_matching_output(&mut unresolved, item);
    }

    let mut recovered = Vec::new();
    for item in translated_input {
        if consume_matching_output(&mut unresolved, item) {
            recovered.push(item.clone());
        }
        if unresolved.is_empty() {
            break;
        }
    }
    recovered
}

/// Render a `function_call_output`'s payload as text.
///
/// The Responses API accepts either a bare string or a structured value here,
/// so both shapes have to survive the conversion below.
fn function_call_output_text(item: &serde_json::Value) -> String {
    match item.get("output") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Rewrite tool results whose originating call is not in `context` into
/// ordinary user messages, returning how many were rewritten.
///
/// Why this exists: compaction replaces the canonical history wholesale, and a
/// tool result that was already in flight is appended *after* the replacement.
/// Its `function_call` went out with the old history, so the request carries a
/// `function_call_output` with nothing to attach to and the Responses API
/// rejects the whole turn:
///
/// ```text
/// invalid_request_error: No tool call found for function call output
/// with call_id ...
/// ```
///
/// That is a *permanent* 400 — the orphan is then persisted, so every later
/// turn replays it and the session is wedged until the process restarts.
///
/// Converting rather than dropping is deliberate. Dropping makes the model
/// re-issue the tool call, and these tools merge PRs and delete directories;
/// replaying a side effect is worse than losing a transcript nicety.
/// Synthesizing the missing `function_call` is not possible faithfully — the
/// output item carries only `call_id` and `output`, never the tool `name` or
/// its arguments — and replaying the *original* call from pre-compaction
/// history trades this violation for a different one on reasoning models,
/// which reject a `function_call` whose paired `reasoning` item is absent.
/// A plain user message satisfies every pairing rule.
pub(crate) fn repair_orphaned_outputs(
    pending: &mut [serde_json::Value],
    context: &[serde_json::Value],
) -> usize {
    let known: HashSet<&str> = context
        .iter()
        .chain(pending.iter())
        .filter(|item| response_item_kind(item) == "function_call")
        .filter_map(call_id)
        .collect();

    let orphans: Vec<usize> = pending
        .iter()
        .enumerate()
        .filter(|(_, item)| response_item_kind(item) == "function_call_output")
        .filter(|(_, item)| call_id(item).is_none_or(|id| !known.contains(id)))
        .map(|(index, _)| index)
        .collect();

    for index in &orphans {
        let item = &pending[*index];
        let id = call_id(item).unwrap_or("unknown").to_string();
        let text = function_call_output_text(item);
        pending[*index] = serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!(
                    "[tool result for call {id}; the call itself was elided by \
                     history compaction]: {text}"
                ),
            }],
        });
    }
    orphans.len()
}

/// Reject a continuation the Responses API cannot accept before spending a
/// network request on it. The scan retains call ids only in this stack frame;
/// the returned failure contains fixed categories and counts exclusively.
fn validate_continuation_input(
    upstream_input: &[serde_json::Value],
    translated_input: &[serde_json::Value],
) -> Result<(), PipelineError> {
    let mut input_kinds = BTreeMap::new();
    let mut unresolved = HashMap::<&str, usize>::new();
    let mut unidentified_calls = 0_usize;
    let mut orphaned_outputs = 0_usize;
    let mut function_call_count = 0;
    let mut function_call_output_count = 0;

    for item in upstream_input {
        *input_kinds.entry(response_item_kind(item)).or_insert(0) += 1;
        match response_item_kind(item) {
            "function_call" => {
                function_call_count += 1;
                if let Some(id) = call_id(item) {
                    *unresolved.entry(id).or_insert(0) += 1;
                } else {
                    unidentified_calls += 1;
                }
            }
            "function_call_output" => {
                function_call_output_count += 1;
                // An output only ever *decremented* here, so one with no call
                // at all was silently accepted and sent upstream, which is
                // precisely the request the API refuses. Count it.
                match call_id(item).and_then(|id| unresolved.get_mut(id).map(|r| (id, r))) {
                    Some((id, remaining)) => {
                        *remaining = remaining.saturating_sub(1);
                        if *remaining == 0 {
                            unresolved.remove(id);
                        }
                    }
                    None => orphaned_outputs += 1,
                }
            }
            _ => {}
        }
    }

    let unmatched_call_count = unresolved.values().sum::<usize>() + unidentified_calls;
    if unmatched_call_count == 0 && orphaned_outputs == 0 {
        return Ok(());
    }

    let source = if unidentified_calls == 0
        && unresolved.keys().any(|missing_id| {
            translated_input.iter().any(|item| {
                response_item_kind(item) == "function_call_output"
                    && call_id(item) == Some(*missing_id)
            })
        }) {
        "pending_input_extraction"
    } else {
        "canonical_history"
    };
    Err(PipelineError::ContinuationInvariant(Box::new(
        ContinuationInvariantFailure {
            input_count: upstream_input.len(),
            input_kinds,
            function_call_count,
            function_call_output_count,
            unmatched_call_count,
            orphaned_output_count: orphaned_outputs,
            source,
        },
    )))
}

/// Verify that a bridge-owned transcript can be sent to the Responses
/// compaction endpoint. Unlike an ordinary continuation there is no pending
/// Messages suffix that could complete an outstanding tool call here.
pub(crate) fn validate_canonical_history(
    history: &[serde_json::Value],
) -> Result<(), PipelineError> {
    validate_continuation_input(history, &[])
}

/// Replayed Messages history is display state, not a second canonical
/// transcript. A continuation carries the current developer instruction in
/// place of the historical one plus its complete pending message suffix.
fn history_aware_input(
    history: &[serde_json::Value],
    translated_input: &[serde_json::Value],
    pending_input: &[serde_json::Value],
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>, usize) {
    if history.is_empty() {
        // The production wedge arrives HERE, not through the pipeline's own
        // compaction recovery. When `/context_compact` cannot validate the
        // canonical history — "Claude may start compaction while a tool batch
        // is outstanding" — the bridge calls
        // `begin_harness_compaction_fallback`, which `clear_items()` the whole
        // history. The harness then keeps sending its OWN compacted
        // transcript, and if its compaction dropped the assistant turn holding
        // the `tool_use` while keeping the user turn holding the
        // `tool_result`, the translated input is orphaned on arrival.
        //
        // With history empty there is nothing to pair against, so this branch
        // has to repair the client's input directly. It is also why the
        // failure survives a daemon restart: the poison lives in the harness's
        // transcript, which it resends verbatim every turn.
        let mut input = translated_input.to_vec();
        repair_orphaned_outputs(&mut input, &[]);
        return (input.clone(), input, 0);
    }

    let recovered = recover_pending_outputs(history, translated_input, pending_input);
    let pending_outputs_recovered = recovered.len();
    let mut turn_input = translated_input
        .iter()
        .filter(|item| is_developer_item(item))
        .cloned()
        .collect::<Vec<_>>();
    append_pending_input(&mut turn_input, &recovered);
    append_pending_input(&mut turn_input, pending_input);
    let prior = history
        .iter()
        .filter(|item| !is_developer_item(item))
        .cloned()
        .collect::<Vec<_>>();
    // Repair `turn_input` itself, not the assembled copy: this same vector is
    // what `record_successful_turn` persists. Repairing only what goes
    // upstream would send a valid request and then store the orphan again,
    // so the fix would work exactly once.
    repair_orphaned_outputs(&mut turn_input, &prior);
    // #2551: the current instruction leads, as it did on the first turn. A
    // provider prompt cache matches prefixes; placing it after the history
    // moved it every turn and re-billed the whole request.
    let (developer, rest): (Vec<_>, Vec<_>) =
        turn_input.iter().cloned().partition(is_developer_item);
    let mut upstream_input = developer;
    append_pending_input(&mut upstream_input, &prior);
    append_pending_input(&mut upstream_input, &rest);
    // And sweep the assembled input, which also covers an orphan that is
    // already resident in `history`. Without this the tightened validator
    // would simply move the wedge from upstream to a local 400 — the same
    // dead session with a different error message.
    repair_orphaned_outputs(&mut upstream_input, &[]);
    (upstream_input, turn_input, pending_outputs_recovered)
}

fn translated_request_shape(
    request: &crate::codex_translate::ResponsesRequest,
) -> serde_json::Value {
    let input_kinds = request
        .input
        .iter()
        .map(|item| match item {
            crate::codex_translate::InputItem::Message { .. } => "message",
            crate::codex_translate::InputItem::FunctionCall { .. } => "function_call",
            crate::codex_translate::InputItem::FunctionCallOutput { .. } => "function_call_output",
            crate::codex_translate::InputItem::Reasoning { .. } => "reasoning",
            crate::codex_translate::InputItem::Compaction { .. } => "compaction",
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "top_level_fields": [
            "model", "input", "stream", "store", "include", "instructions", "tools",
            "tool_choice", "parallel_tool_calls", "reasoning", "prompt_cache_key", "service_tier"
        ],
        "input_count": request.input.len(),
        "input_kinds": input_kinds,
        "tool_count": request.tools.as_ref().map_or(0, Vec::len),
        "has_instructions": request.instructions.is_some(),
        "has_tool_choice": request.tool_choice.is_some(),
        "has_reasoning": request.reasoning.is_some(),
        "has_prompt_cache_key": request.prompt_cache_key.is_some(),
        "has_service_tier": request.service_tier.is_some(),
    })
}

pub struct Pipeline<C: CredentialSource> {
    client: UpstreamClient<C>,
    default_model: ModelSpec,
}

impl<C: CredentialSource> std::fmt::Debug for Pipeline<C> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pipeline")
            .field("default_model", &self.default_model)
            .finish()
    }
}

impl<C: CredentialSource> Pipeline<C> {
    pub fn new(client: UpstreamClient<C>) -> Self {
        Self {
            client,
            default_model: default_model_spec(),
        }
    }

    pub(crate) fn credential_target(&self) -> Result<UpstreamTarget, UpstreamError> {
        self.client.credential_target()
    }

    /// Pin the selection used when a request carries no model of its own.
    /// This is how a launch-time `--model terra@high` reaches the wire.
    pub fn with_default_model(mut self, model: ModelSpec) -> Self {
        self.default_model = model;
        self
    }

    /// Build a typed compact request from canonical history, send it through
    /// the selected credential route, and return the validated opaque
    /// replacement. The caller atomically replaces its history only after this
    /// succeeds.
    fn compact_history(
        &self,
        request: &ResponsesRequest,
        history: Vec<serde_json::Value>,
        cancel: &AtomicBool,
    ) -> Result<Vec<serde_json::Value>, PipelineError> {
        let compact = request
            .compact_request_for_history(history)
            .map_err(PipelineError::Compaction)?;
        let body = serde_json::to_vec(&compact).map_err(|error| {
            PipelineError::Translate(TranslateError::Invalid(error.to_string()))
        })?;
        let response: CompactResponse = self
            .client
            .compact(&body, cancel)
            .map_err(PipelineError::Upstream)?;
        crate::codex_translate::compact_output_as_values(response)
            .map_err(PipelineError::Compaction)
    }

    /// Compact the bridge-owned transcript for an explicit lifecycle control
    /// request. The control request has no Messages body from which to derive
    /// per-turn options, so it uses the launch's selected model and the same
    /// stable prompt-cache key as ordinary turns.
    pub fn compact_canonical_history(
        &self,
        history: Vec<serde_json::Value>,
        cancel: &AtomicBool,
    ) -> Result<Vec<serde_json::Value>, PipelineError> {
        let request = ResponsesRequest {
            model: self.default_model.model.clone(),
            input: Vec::new(),
            stream: false,
            store: false,
            include: Vec::new(),
            instructions: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: Some(true),
            reasoning: None,
            prompt_cache_key: Some(self.client.session_id().to_string()),
            service_tier: None,
        };
        self.compact_history(&request, history, cancel)
    }

    /// Compatibility helper for callers without canonical history.
    pub fn compact(
        &self,
        request_body: &[u8],
        cancel: &AtomicBool,
    ) -> Result<ResponsesRequest, PipelineError> {
        let (request, _) = self.prepare_request(request_body)?;
        let compact = request
            .compact_request()
            .map_err(PipelineError::Compaction)?;
        let body = serde_json::to_vec(&compact).map_err(|error| {
            PipelineError::Translate(TranslateError::Invalid(error.to_string()))
        })?;
        let response: CompactResponse = self
            .client
            .compact(&body, cancel)
            .map_err(PipelineError::Upstream)?;
        request
            .retry_with_compaction(response)
            .map_err(PipelineError::Compaction)
    }

    /// Translate, send, and stream the reply as Anthropic SSE frames.
    ///
    /// `sink` sees each frame as it is produced; the caller writes and flushes
    /// it so the turn renders progressively.
    pub fn stream(
        &self,
        request_body: &[u8],
        message_id: &str,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&str) -> Result<(), UpstreamError>,
    ) -> Result<StreamSummary, PipelineError> {
        self.stream_inner(request_body, message_id, cancel, sink, None)
    }

    /// As [`Self::stream`], committing the canonical translated input and
    /// complete opaque upstream output only after a successful turn.
    pub fn stream_with_history(
        &self,
        request_body: &[u8],
        message_id: &str,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&str) -> Result<(), UpstreamError>,
        history: &mut ConversationHistory,
    ) -> Result<StreamSummary, PipelineError> {
        self.stream_inner(request_body, message_id, cancel, sink, Some(history))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    fn stream_attempt(
        &self,
        request: &ResponsesRequest,
        upstream_input: Option<&[serde_json::Value]>,
        message_id: &str,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&str) -> Result<(), UpstreamError>,
        tool_names: std::collections::HashMap<String, String>,
    ) -> Result<AttemptOutcome, PipelineError> {
        let model = request.model.clone();
        let upstream_body = if let Some(input) = upstream_input {
            let mut request = serde_json::to_value(request).map_err(|error| {
                PipelineError::Translate(TranslateError::Invalid(error.to_string()))
            })?;
            request["input"] = serde_json::Value::Array(input.to_vec());
            serde_json::to_vec(&request).map_err(|error| {
                PipelineError::Translate(TranslateError::Invalid(error.to_string()))
            })?
        } else {
            serde_json::to_vec(request).map_err(|error| {
                PipelineError::Translate(TranslateError::Invalid(error.to_string()))
            })?
        };
        let mut decoder = FrameDecoder::new();
        let mut translator = StreamTranslator::new(model, message_id).with_tool_names(tool_names);
        let mut output_items = Vec::new();
        let mut completed = false;
        let mut visible = false;
        let mut deferred_frames: Vec<String> = Vec::new();
        let delivered = AtomicBool::new(false);
        let mut forward = |frames: Vec<String>| -> Result<(), UpstreamError> {
            if anthropic_frames_have_visible_output(&frames) && !visible {
                visible = true;
                for frame in std::mem::take(&mut deferred_frames) {
                    // Commit before calling the writer: an I/O error may occur
                    // after it has flushed bytes downstream, where replay would
                    // duplicate a visible frame.
                    delivered.store(true, Ordering::Release);
                    sink(&frame)?;
                }
            }
            for frame in frames {
                if visible {
                    // See the deferred-frame commit boundary above.
                    delivered.store(true, Ordering::Release);
                    sink(&frame)?;
                } else {
                    deferred_frames.push(frame);
                }
            }
            Ok(())
        };
        let result = {
            let mut pump = |chunk: &[u8]| -> Result<(), UpstreamError> {
                for frame in decoder.push(chunk) {
                    completed |= response_terminated(&frame);
                    capture_output_item(&frame, &mut output_items);
                    forward(translator.push(&frame))?;
                }
                Ok(())
            };
            self.client
                .stream_with_commit(&upstream_body, cancel, &mut pump, &delivered)
        };
        match result {
            Ok(_) => {
                for frame in decoder.finish() {
                    completed |= response_terminated(&frame);
                    capture_output_item(&frame, &mut output_items);
                    forward(translator.push(&frame)).map_err(PipelineError::Upstream)?;
                }
                if completed {
                    forward(translator.finish()).map_err(PipelineError::Upstream)?;
                }
            }
            Err(error) => {
                // `delivered` is the commit boundary shared with the
                // upstream client. It is equivalent to `!visible` here, but
                // avoids borrowing the frame-forwarding closure while its
                // error path is still being examined.
                if !delivered.load(Ordering::Acquire) && error.is_context_length_exceeded() {
                    return Ok(AttemptOutcome {
                        usage: translator.terminal_usage(),
                        terminal_account_failure: false,
                        authentication_failure: false,
                        in_band_failure: None,
                        in_band_provider_failure: false,
                        output_items,
                        visible,
                        deferred_frames,
                        pre_output_failure: Some(error),
                    });
                }
                if !translator.is_finished() {
                    forward(translator.fail_opaque()).map_err(PipelineError::Upstream)?;
                }
                return Err(PipelineError::Upstream(error));
            }
        }
        let in_band_failure = translator.in_band_failure().cloned();
        let in_band_provider_failure = translator.has_in_band_provider_failure();
        let terminal_account_failure = translator.terminal_account_failure();
        let authentication_failure = translator.authentication_failure();
        let usage = translator.terminal_usage();
        if !completed {
            if !in_band_provider_failure && !translator.is_finished() {
                forward(translator.fail_opaque()).map_err(PipelineError::Upstream)?;
            }
            if !in_band_provider_failure {
                return Err(PipelineError::Upstream(UpstreamError::Transport(
                    "stream ended before a terminal response event",
                )));
            }
        }
        Ok(AttemptOutcome {
            usage,
            terminal_account_failure,
            authentication_failure,
            in_band_failure,
            in_band_provider_failure,
            output_items,
            visible,
            deferred_frames,
            pre_output_failure: None,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    fn stream_inner(
        &self,
        request_body: &[u8],
        message_id: &str,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&str) -> Result<(), UpstreamError>,
        history: Option<&mut ConversationHistory>,
    ) -> Result<StreamSummary, PipelineError> {
        let (request, tool_names, request_shape, translated_input) = self.prepare(request_body)?;
        let model = request.model.clone();
        let pending_input = crate::codex_translate::pending_input_items_as_values(request_body)
            .map_err(PipelineError::Translate)?;
        let (mut upstream_input, turn_input, pending_outputs_recovered) = match history.as_ref() {
            Some(history) => {
                let assembled =
                    history_aware_input(&history.snapshot(), &translated_input, &pending_input);
                validate_continuation_input(&assembled.0, &translated_input)?;
                assembled
            }
            None => (translated_input.clone(), translated_input, 0),
        };

        let first = self.stream_attempt(
            &request,
            Some(&upstream_input),
            message_id,
            cancel,
            sink,
            tool_names.clone(),
        )?;
        let should_recover = !first.visible
            && (first
                .pre_output_failure
                .as_ref()
                .is_some_and(UpstreamError::is_context_length_exceeded)
                || matches!(
                    first
                        .in_band_failure
                        .as_ref()
                        .and_then(|failure| failure.code),
                    Some("context_length_exceeded")
                ))
            && history
                .as_ref()
                .is_some_and(|history| !history.snapshot().is_empty());
        if !should_recover {
            if let Some(error) = first.pre_output_failure {
                return Err(PipelineError::Upstream(error));
            }
            emit_deferred_frames(&first.deferred_frames, sink)?;
            if first.in_band_provider_failure {
                return Ok(StreamSummary {
                    model,
                    usage: first.usage,
                    orphaned_outputs_repaired: 0,
                    pending_outputs_recovered,
                    terminal_account_failure: first.terminal_account_failure,
                    authentication_failure: first.authentication_failure,
                    in_band_failure: first.in_band_failure,
                    request_shape,
                    output_items: Vec::new(),
                    history_append_rejected: false,
                });
            }
            let history_append_rejected = match history {
                Some(history) => record_successful_turn(history, &turn_input, &first.output_items)?,
                None => false,
            };
            return Ok(StreamSummary {
                model,
                usage: first.usage,
                orphaned_outputs_repaired: 0,
                pending_outputs_recovered,
                terminal_account_failure: first.terminal_account_failure,
                authentication_failure: first.authentication_failure,
                in_band_failure: first.in_band_failure,
                request_shape,
                output_items: first.output_items,
                history_append_rejected,
            });
        }

        // The request's first failure produced no Anthropic-visible output. Only
        // this exact classified code earns one recovery cycle; every compact
        // endpoint failure and every retry failure returns normally below.
        let compact_output = self.compact_history(
            &request,
            history
                .as_ref()
                .expect("guarded by should_recover")
                .snapshot(),
            cancel,
        )?;
        let history = history.expect("canonical history requires a history store");
        history
            .replace_history(&compact_output)
            .map_err(history_error)?;
        upstream_input = compact_output;
        // This is where the wedge was born. `replace_history` just discarded
        // the `function_call` that `turn_input`'s pending tool result belongs
        // to, so re-appending it unchanged produces exactly the request the
        // Responses API refuses — and `record_successful_turn` below then
        // persists the orphan, poisoning every subsequent turn.
        let mut turn_input = turn_input;
        let repaired = repair_orphaned_outputs(&mut turn_input, &upstream_input);
        append_pending_input(&mut upstream_input, &turn_input);
        // Re-validate: the check at assembly ran against the pre-compaction
        // history, so it says nothing about the input actually being sent now.
        validate_continuation_input(&upstream_input, &[])?;
        let retry = self.stream_attempt(
            &request,
            Some(&upstream_input),
            message_id,
            cancel,
            sink,
            tool_names,
        )?;
        if let Some(error) = retry.pre_output_failure {
            return Err(PipelineError::Upstream(error));
        }
        emit_deferred_frames(&retry.deferred_frames, sink)?;
        if retry.in_band_provider_failure {
            return Ok(StreamSummary {
                model,
                usage: retry.usage,
                orphaned_outputs_repaired: repaired,
                pending_outputs_recovered,
                terminal_account_failure: retry.terminal_account_failure,
                authentication_failure: retry.authentication_failure,
                in_band_failure: retry.in_band_failure,
                request_shape,
                output_items: Vec::new(),
                history_append_rejected: false,
            });
        }
        let history_append_rejected =
            record_successful_turn(history, &turn_input, &retry.output_items)?;
        Ok(StreamSummary {
            model,
            usage: retry.usage,
            orphaned_outputs_repaired: repaired,
            pending_outputs_recovered,
            terminal_account_failure: retry.terminal_account_failure,
            authentication_failure: retry.authentication_failure,
            in_band_failure: retry.in_band_failure,
            request_shape,
            output_items: retry.output_items,
            history_append_rejected,
        })
    }

    /// Same path, aggregated into a single Anthropic `Message`.
    pub fn complete(
        &self,
        request_body: &[u8],
        message_id: &str,
        cancel: &AtomicBool,
    ) -> Result<serde_json::Value, PipelineError> {
        self.complete_inner(request_body, message_id, cancel, None)
            .map(|completion| completion.message)
    }

    /// As [`Self::complete`], recording a completed non-streaming turn.
    pub fn complete_with_history(
        &self,
        request_body: &[u8],
        message_id: &str,
        cancel: &AtomicBool,
        history: &mut ConversationHistory,
    ) -> Result<Completion, PipelineError> {
        self.complete_inner(request_body, message_id, cancel, Some(history))
    }

    fn complete_inner(
        &self,
        request_body: &[u8],
        message_id: &str,
        cancel: &AtomicBool,
        history: Option<&mut ConversationHistory>,
    ) -> Result<Completion, PipelineError> {
        let mut aggregator = MessageAggregator::new();
        let mut sink = |frame: &str| -> Result<(), UpstreamError> {
            aggregator.push_frame(frame);
            Ok(())
        };
        let summary = self.stream_inner(request_body, message_id, cancel, &mut sink, history)?;
        if aggregator.errored() {
            // A recovered pre-output failure deliberately produces no frames
            // from the first attempt, so only a retry failure can reach here.
            // Propagate the classification, not a generic transport failure.
            // The old relabel turned every in-band provider error -- including
            // a drained account -- into `502 api_error`.
            let mut failure = aggregator
                .provider_error()
                .cloned()
                .unwrap_or(ProviderFailure {
                    kind: "api_error".to_string(),
                    message: "upstream stream failed".to_string(),
                    diagnostic: None,
                });
            if failure.kind == "invalid_request_error" {
                failure.diagnostic = summary.in_band_failure.map(|failure| {
                    Box::new(InBandDiagnostic {
                        failure,
                        request_shape: summary.request_shape,
                    })
                });
            }
            return Err(PipelineError::Provider(failure));
        }
        Ok(Completion {
            message: aggregator.finish(),
            model: summary.model,
            usage: summary.usage,
            history_append_rejected: summary.history_append_rejected,
            pending_outputs_recovered: summary.pending_outputs_recovered,
        })
    }

    /// Translate the request and resolve common upstream options.
    fn prepare_request(
        &self,
        request_body: &[u8],
    ) -> Result<(ResponsesRequest, std::collections::HashMap<String, String>), PipelineError> {
        let target = self
            .client
            .credentials()
            .resolve()
            .map_err(PipelineError::Upstream)?;
        let options = TranslateOptions {
            model: target
                .model_override()
                .cloned()
                .unwrap_or_else(|| self.default_model.clone()),
            system_placement: if target.uses_codex_backend() {
                SystemPlacement::DeveloperMessage
            } else {
                SystemPlacement::Instructions
            },
            prompt_cache_key: target
                .prompt_cache_key()
                .map(str::to_string)
                .or_else(|| Some(self.client.session_id().to_string())),
            service_tier: None,
        };
        let translated =
            translate_bytes(request_body, &options).map_err(PipelineError::Translate)?;
        Ok((translated.request, translated.tool_names))
    }

    /// Translate the request before a history-aware caller assembles its
    /// canonical upstream transcript.
    #[allow(clippy::type_complexity)]
    fn prepare(
        &self,
        request_body: &[u8],
    ) -> Result<
        (
            ResponsesRequest,
            std::collections::HashMap<String, String>,
            serde_json::Value,
            Vec<serde_json::Value>,
        ),
        PipelineError,
    > {
        let (request, tool_names) = self.prepare_request(request_body)?;
        let request_shape = translated_request_shape(&request);
        let translated_input = crate::codex_translate::input_items_as_values(&request.input)
            .map_err(PipelineError::Translate)?;
        Ok((request, tool_names, request_shape, translated_input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_upstream::{ApiKeyCredentials, UpstreamConfig, UpstreamTarget};
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Minimal Responses-shaped server: records the request body and replies
    /// with a scripted SSE stream.
    struct FakeResponses {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeResponses {
        fn start(reply: Vec<u8>) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread_requests = Arc::clone(&requests);
            let thread_shutdown = Arc::clone(&shutdown);
            let handle = std::thread::spawn(move || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut raw = Vec::new();
                    let mut byte = [0_u8; 1];
                    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut byte) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => raw.push(byte[0]),
                        }
                    }
                    let head = String::from_utf8_lossy(&raw).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    let mut body = vec![0_u8; length];
                    if length > 0 {
                        let _ = stream.read_exact(&mut body);
                    }
                    thread_requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&body).to_string());
                    let _ = stream.write_all(&reply);
                    let _ = stream.flush();
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            });
            Self {
                base_url: format!("http://{addr}"),
                requests,
                shutdown,
                handle: Some(handle),
            }
        }

        fn request(&self) -> serde_json::Value {
            let raw = self.requests.lock().unwrap().first().cloned().unwrap();
            serde_json::from_str(&raw).expect("upstream request must be JSON")
        }
    }

    impl Drop for FakeResponses {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn sse(events: &[serde_json::Value]) -> Vec<u8> {
        let body: String = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap_or("message")
                )
            })
            .collect();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn pipeline(base_url: &str) -> Pipeline<ApiKeyCredentials> {
        Pipeline::new(UpstreamClient::new(
            ApiKeyCredentials::new("sk-test", Some(base_url.to_string())).unwrap(),
            UpstreamConfig {
                connect_timeout: Duration::from_secs(2),
                read_timeout: Duration::from_secs(2),
                first_frame_timeout: Some(Duration::from_secs(2)),
                overall_timeout: Duration::from_secs(10),
                retry_delay: Duration::from_millis(10),
                ..UpstreamConfig::default()
            },
        ))
    }

    fn text_reply() -> Vec<u8> {
        sse(&[
            json!({"type": "response.created"}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                   "content_index": 0, "delta": "Hello "}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                   "content_index": 0, "delta": "world"}),
            json!({"type": "response.output_text.done", "output_index": 0, "content_index": 0}),
            json!({"type": "response.completed",
                   "response": {"usage": {"input_tokens": 7, "output_tokens": 2}}}),
        ])
    }

    #[test]
    fn streaming_request_reaches_upstream_translated_and_returns_anthropic_sse() {
        let server = FakeResponses::start(text_reply());
        let pipeline = pipeline(&server.base_url);
        let mut frames = Vec::new();
        pipeline
            .stream(
                br#"{"model":"claude-sonnet-5","max_tokens":32,"system":"be brief",
                     "messages":[{"role":"user","content":"hi"}],"stream":true}"#,
                "msg_x",
                &AtomicBool::new(false),
                &mut |frame| {
                    frames.push(frame.to_string());
                    Ok(())
                },
            )
            .unwrap();

        // What the fake actually received is the real assertion: a translation
        // regression must fail here, not merely change our own output.
        let sent = server.request();
        assert_eq!(sent["model"], "gpt-6-sol");
        assert_eq!(sent["instructions"], "be brief");
        assert!(sent.get("max_output_tokens").is_none());
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(sent["input"][0]["content"][0]["text"], "hi");

        let rendered = frames.concat();
        assert!(rendered.contains("event: message_start"));
        assert!(rendered.contains(r#""text":"Hello "#));
        assert!(rendered.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }

    /// A non-streaming request still streams upstream; the events are folded
    /// back into one Message.
    #[test]
    fn non_streaming_request_aggregates_into_one_message() {
        let server = FakeResponses::start(text_reply());
        let message = pipeline(&server.base_url)
            .complete(
                br#"{"messages":[{"role":"user","content":"hi"}],"stream":false}"#,
                "msg_agg",
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(
            server.request()["stream"],
            true,
            "upstream is always streamed"
        );
        assert_eq!(
            message,
            json!({
                "id": "msg_agg",
                "type": "message",
                "role": "assistant",
                "model": "gpt-6-sol",
                "content": [{"type": "text", "text": "Hello world"}],
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": {"input_tokens": 7, "output_tokens": 2},
            })
        );
    }

    #[test]
    fn tool_calls_aggregate_with_parsed_input() {
        let server = FakeResponses::start(sse(&[
            json!({"type": "response.created"}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "function_call", "call_id": "c1", "name": "weather"}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                   "delta": "{\"city\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                   "delta": "\"Paris\"}"}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"type": "function_call", "call_id": "c1", "name": "weather"}}),
            json!({"type": "response.completed", "response": {"usage": {}}}),
        ]));
        let message = pipeline(&server.base_url)
            .complete(
                br#"{"messages":[{"role":"user","content":"weather?"}]}"#,
                "msg_tool",
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(
            message["content"][0],
            json!({"type": "tool_use", "id": "c1", "name": "weather",
                   "input": {"city": "Paris"}})
        );
    }

    #[test]
    fn tool_definitions_and_choice_reach_upstream() {
        let server = FakeResponses::start(text_reply());
        pipeline(&server.base_url)
            .complete(
                br#"{"messages":[{"role":"user","content":"x"}],
                     "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
                     "tool_choice":{"type":"any","disable_parallel_tool_use":true}}"#,
                "msg_tools",
                &AtomicBool::new(false),
            )
            .unwrap();
        let sent = server.request();
        assert_eq!(sent["tools"][0]["type"], "function");
        assert_eq!(sent["tools"][0]["name"], "lookup");
        assert_eq!(sent["tool_choice"], "required");
        assert_eq!(sent["parallel_tool_calls"], false);
    }

    /// #750 A1: translation is total, so a request carrying Anthropic-only
    /// fields still reaches upstream -- with those fields dropped rather than
    /// turned into a 4xx the user cannot act on.
    #[test]
    fn anthropic_only_fields_are_dropped_and_the_request_still_flows() {
        let server = FakeResponses::start(text_reply());
        pipeline(&server.base_url)
            .complete(
                br#"{"messages":[{"role":"user","content":"x"}],"top_k":5,
                     "stop_sequences":["STOP"],"temperature":0.7,"max_tokens":16}"#,
                "msg_total",
                &AtomicBool::new(false),
            )
            .expect("a droppable field must not fail the request");

        let sent = server.request();
        for banned in [
            "top_k",
            "stop_sequences",
            "temperature",
            "max_output_tokens",
        ] {
            assert!(sent.get(banned).is_none(), "{banned} must not be forwarded");
        }
    }

    /// Conformance defaults must survive the whole pipeline, not just the
    /// translator unit tests.
    #[test]
    fn conformance_defaults_reach_upstream() {
        let server = FakeResponses::start(text_reply());
        pipeline(&server.base_url)
            .complete(
                br#"{"messages":[{"role":"user","content":"x"}]}"#,
                "msg_conf",
                &AtomicBool::new(false),
            )
            .unwrap();
        let sent = server.request();
        assert_eq!(sent["store"], false);
        assert_eq!(sent["include"][0], "reasoning.encrypted_content");
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["reasoning"]["effort"], "low");
        assert_eq!(sent["parallel_tool_calls"], true);
        // A stable cache key is what buys prompt-cache hits across turns.
        assert!(sent["prompt_cache_key"]
            .as_str()
            .is_some_and(|k| !k.is_empty()));
    }

    #[test]
    fn provider_prompt_cache_key_overrides_the_bridge_conversation_identity() {
        struct CacheKeyCredentials {
            base_url: String,
        }

        impl CredentialSource for CacheKeyCredentials {
            fn resolve(&self) -> Result<crate::codex_upstream::UpstreamTarget, UpstreamError> {
                Ok(
                    crate::codex_upstream::UpstreamTarget::new(
                        self.base_url.clone(),
                        "Bearer test",
                    )
                    .with_prompt_cache_key(Some("provider-authoritative-key".to_string())),
                )
            }
        }

        let server = FakeResponses::start(text_reply());
        let pipeline = Pipeline::new(
            UpstreamClient::new(
                CacheKeyCredentials {
                    base_url: server.base_url.clone(),
                },
                UpstreamConfig::default(),
            )
            .with_session_id("bridge-conversation-key".to_string()),
        );
        pipeline
            .complete(
                br#"{"messages":[{"role":"user","content":"x"}]}"#,
                "msg_cache_override",
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(
            server.request()["prompt_cache_key"],
            "provider-authoritative-key"
        );
    }

    /// The billed default, asserted as a literal on the wire rather than
    /// through the catalog's provider-default row — a metadata-based assertion
    /// follows the row wherever it goes and cannot notice a change in what the
    /// user is charged for. #1254 deliberately restores Sol as the reviewed
    /// main-session default while retaining its catalog-native low effort.
    #[test]
    fn the_billed_default_is_sol_at_low() {
        let server = FakeResponses::start(text_reply());
        pipeline(&server.base_url)
            .complete(
                br#"{"messages":[{"role":"user","content":"x"}]}"#,
                "msg_default",
                &AtomicBool::new(false),
            )
            .unwrap();
        let sent = server.request();
        assert_eq!(sent["model"], "gpt-6-sol");
        assert_eq!(sent["reasoning"]["effort"], "low");
    }

    #[test]
    fn malformed_json_is_a_400() {
        let server = FakeResponses::start(text_reply());
        let error = pipeline(&server.base_url)
            .complete(b"not json", "msg", &AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(error.http_status(), 400);
    }

    #[test]
    fn missing_upstream_credentials_are_a_gateway_failure_that_names_no_secret() {
        struct NoCredentials;
        impl CredentialSource for NoCredentials {
            fn resolve(&self) -> Result<UpstreamTarget, UpstreamError> {
                Err(UpstreamError::Credentials("OPENAI_API_KEY is not set"))
            }
        }
        let pipeline = Pipeline::new(UpstreamClient::new(
            NoCredentials,
            UpstreamConfig::default(),
        ));
        let error = pipeline
            .complete(
                br#"{"messages":[{"role":"user","content":"x"}]}"#,
                "msg",
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert_eq!(error.http_status(), 502);
        assert!(!error.client_message().contains("OPENAI_API_KEY"));
    }

    #[test]
    fn upstream_status_maps_to_a_downstream_status_without_the_body() {
        let body = r#"{"error":{"message":"key sk-secret for org org_1"}}"#;
        let reply = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let server = FakeResponses::start(reply);
        let error = pipeline(&server.base_url)
            .complete(
                br#"{"messages":[{"role":"user","content":"x"}]}"#,
                "msg",
                &AtomicBool::new(false),
            )
            .unwrap_err();

        assert_eq!(error.http_status(), 403);
        let rendered = format!("{} {:?}", error.client_message(), error);
        assert!(!rendered.contains("sk-secret"));
        assert!(!rendered.contains("org_1"));
        // The diagnostic is richer than the client message, and is still clean:
        // it is the only place the body is allowed to have left a trace.
        let diagnostic = error.upstream_diagnostic().expect("a status diagnostic");
        assert!(diagnostic.contains("status=403"), "{diagnostic}");
        assert!(!diagnostic.contains("sk-secret"), "{diagnostic}");
        assert!(!diagnostic.contains("org_1"), "{diagnostic}");
    }

    #[test]
    fn upstream_auth_rejection_is_not_a_downstream_bearer_rejection() {
        let failure = UpstreamFailure::from_parts(
            401,
            |_| None,
            r#"{"error":{"message":"Bearer secret-token"}}"#,
            Duration::ZERO,
        );
        let error = PipelineError::Upstream(UpstreamError::Status(failure));
        assert_eq!(error.http_status(), 502);
        assert!(!error.client_message().contains("secret-token"));

        let rejected = PipelineError::Upstream(UpstreamError::Credentials(REFRESH_REJECTED));
        assert_eq!(rejected.http_status(), 502);
        assert!(rejected.client_message().contains("clud auth login codex"));

        let transient = PipelineError::Upstream(UpstreamError::Credentials(REFRESH_TEMPORARY));
        assert_eq!(transient.http_status(), 503);
    }

    /// 502 means "the gateway hop failed". Failures that are not that must not
    /// borrow it, or every one of them becomes indistinguishable in a log.
    #[test]
    fn non_gateway_failures_no_longer_report_502() {
        let cases = [
            (UpstreamError::TooLarge, 413_u16),
            (UpstreamError::Cancelled, 499),
            (UpstreamError::Downstream("client hung up"), 499),
            (UpstreamError::Timeout, 504),
            // These two genuinely are gateway failures and keep 502.
            (UpstreamError::Transport("connection failed"), 502),
        ];
        for (error, expected) in cases {
            let rendered = PipelineError::Upstream(error.clone());
            assert_eq!(rendered.http_status(), expected, "{error:?}");
            assert_ne!(
                rendered.client_message(),
                "upstream provider error",
                "{error:?} still reports the old catch-all message"
            );
        }
    }

    /// An unmapped upstream 5xx is still a 502 — that part was always right.
    #[test]
    fn an_unmapped_upstream_status_is_still_a_502() {
        let failure = UpstreamFailure::from_parts(503, |_| None, "overloaded", Duration::ZERO);
        let error = PipelineError::Upstream(UpstreamError::Status(failure));
        assert_eq!(error.http_status(), 502);
    }

    /// The reported failure, end to end at the message layer: the class is
    /// named, and the reset is rendered as a clock rather than a raw count of
    /// seconds a user has to do arithmetic on.
    #[test]
    fn an_exhausted_account_says_so_and_renders_the_reset_readably() {
        let body = r#"{"error":{"code":"usage_limit_reached","resets_in_seconds":442242}}"#;
        let failure = UpstreamFailure::from_parts(429, |_| None, body, Duration::ZERO);
        let error = PipelineError::Upstream(UpstreamError::Status(failure));
        assert_eq!(error.http_status(), 429);
        assert_eq!(error.failure_class(), Some(FailureClass::Exhausted));
        assert!(error.is_terminal_account_failure());

        let message = error.client_message();
        assert!(message.contains("quota exhausted"), "{message}");
        // 442242s -- the number the original report showed the user.
        assert!(message.contains("5d 2h"), "{message}");
        assert!(!message.contains("442242"), "{message}");
        assert_eq!(error.retry_after_seconds(), Some(442_242));
    }

    /// An ordinary throttle is not an exhaustion and keeps its own wording.
    #[test]
    fn a_plain_429_is_a_rate_limit_not_a_billing_failure() {
        let failure = UpstreamFailure::from_parts(429, |_| None, "slow down", Duration::ZERO);
        let error = PipelineError::Upstream(UpstreamError::Status(failure));
        assert_eq!(error.failure_class(), Some(FailureClass::Transient));
        assert!(!error.is_terminal_account_failure());
        assert!(error.client_message().contains("rate limited"));
    }

    #[test]
    fn humanize_reads_like_a_clock() {
        assert_eq!(humanize(Duration::from_secs(45)), "45s");
        assert_eq!(humanize(Duration::from_secs(600)), "10m");
        assert_eq!(humanize(Duration::from_secs(3_600)), "1h 0m");
        assert_eq!(humanize(Duration::from_secs(442_242)), "5d 2h");
    }

    /// The request id is an opaque correlation handle, and it is the difference
    /// between a report that can be traced upstream and one that cannot.
    #[test]
    fn the_request_id_reaches_the_client_message_but_the_body_does_not() {
        let body = r#"{"error":{"message":"key sk-secret for org org_1"}}"#;
        let failure = UpstreamFailure::from_parts(
            502,
            |name| (name == "x-request-id").then(|| "req_xyz".to_string()),
            body,
            Duration::ZERO,
        );
        let error = PipelineError::Upstream(UpstreamError::Status(failure));
        let message = error.client_message();
        assert!(message.contains("req_xyz"), "{message}");
        assert!(!message.contains("sk-secret"), "{message}");
        assert!(!message.contains("org_1"), "{message}");
    }

    #[test]
    fn an_expired_login_is_the_one_credential_reason_forwarded_verbatim() {
        let expired = PipelineError::Upstream(UpstreamError::Credentials(CREDENTIALS_EXPIRED));
        assert_eq!(expired.http_status(), 502);
        assert!(expired.client_message().contains("codex login"));

        // Every other reason stays generic: they name environment variables.
        let missing =
            PipelineError::Upstream(UpstreamError::Credentials("OPENAI_API_KEY is not set"));
        assert!(!missing.client_message().contains("OPENAI_API_KEY"));
    }

    /// A mid-stream failure cannot change the status, so the turn is closed
    /// in-band with a sanitized SSE error rather than left hanging.
    #[test]
    fn a_mid_stream_failure_emits_an_sse_error_after_the_partial_output() {
        let head = "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"partial\"}\n\n";
        // Claims far more than it sends, then hangs up.
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 65535\r\nConnection: close\r\n\r\n{head}"
        )
        .into_bytes();
        let server = FakeResponses::start(reply);
        let mut frames = Vec::new();
        let error = pipeline(&server.base_url)
            .stream(
                br#"{"messages":[{"role":"user","content":"x"}],"stream":true}"#,
                "msg_mid",
                &AtomicBool::new(false),
                &mut |frame| {
                    frames.push(frame.to_string());
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(matches!(error, PipelineError::Upstream(_)));
        let rendered = frames.concat();
        assert!(
            rendered.contains("partial"),
            "partial output should survive"
        );
        assert!(rendered.contains("event: content_block_stop"));
        assert!(rendered.contains("event: error"));
        assert!(rendered.contains("upstream provider error"));
    }

    #[test]
    fn aggregator_folds_multiple_blocks_and_reports_errors() {
        let mut aggregator = MessageAggregator::new();
        for frame in [
            r#"event: message_start
data: {"type":"message_start","message":{"id":"m1","model":"gpt","usage":{"input_tokens":3}}}

"#,
            r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a"}}

"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"b"}}

"#,
            r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}

"#,
            r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}

"#,
        ] {
            aggregator.push_frame(frame);
        }
        assert!(!aggregator.errored());
        let message = aggregator.finish();
        assert_eq!(message["id"], "m1");
        assert_eq!(message["content"][0]["text"], "ab");
        assert_eq!(message["usage"]["input_tokens"], 3);
        assert_eq!(message["usage"]["output_tokens"], 9);

        let mut failed = MessageAggregator::new();
        failed.push_frame("event: error\ndata: {\"type\":\"error\"}\n\n");
        assert!(failed.errored());
    }

    /// Truncated tool arguments must not produce a block whose `input` is
    /// invalid JSON; an empty object is the safe reading.
    #[test]
    fn truncated_tool_arguments_aggregate_to_an_empty_object() {
        let mut aggregator = MessageAggregator::new();
        aggregator.push_frame(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"c\",\"name\":\"n\",\"input\":{}}}\n\n",
        );
        aggregator.push_frame(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
        );
        let message = aggregator.finish();
        assert_eq!(message["content"][0]["input"], json!({}));
    }

    #[test]
    fn cancellation_propagates_out_of_the_pipeline() {
        let server = FakeResponses::start(text_reply());
        let error = pipeline(&server.base_url)
            .complete(
                br#"{"messages":[{"role":"user","content":"x"}]}"#,
                "msg",
                &AtomicBool::new(true),
            )
            .unwrap_err();
        assert_eq!(error, PipelineError::Upstream(UpstreamError::Cancelled));
    }

    /// #2551: with an unchanged instruction, each request's input must start
    /// with the previous request's input, or the provider re-bills it all.
    #[test]
    fn successive_turns_extend_the_previous_request_as_a_prefix() {
        let msg = |role: &str, text: &str| {
            json!({"type": "message", "role": role, "content": [{"type": "input_text", "text": text}]})
        };
        let dev = msg("developer", "system prompt");
        let first = vec![dev.clone(), msg("user", "u1")];
        let (request1, stored1, _) = history_aware_input(&[], &first, &first[1..]);
        let mut history = stored1;
        history.push(msg("assistant", "a1"));

        let second = vec![dev.clone(), msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")];
        let (request2, _, _) = history_aware_input(&history, &second, &second[3..]);

        assert!(
            request2.starts_with(&request1),
            "turn 2 must extend turn 1 as a prefix:\n{request1:#?}\n{request2:#?}"
        );
        assert_eq!(request2.iter().filter(|item| is_developer_item(item)).count(), 1);
    }

    #[test]
    fn continuation_replaces_the_prior_developer_instruction() {
        let history = vec![
            json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "old instruction"}]}),
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "earlier user"}]}),
        ];
        let translated = vec![
            json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "current instruction"}]}),
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "earlier user"}]}),
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "current user"}]}),
        ];
        let pending = vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "current user"}]}),
        ];

        let (upstream, turn, recovered) = history_aware_input(&history, &translated, &pending);

        assert_eq!(recovered, 0);
        assert_eq!(turn.len(), 2);
        assert_eq!(turn[0]["content"][0]["text"], "current instruction");
        // The instruction leads the request, as on the first turn: a
        // provider prompt cache matches prefixes, so moving it behind the
        // history would miss on every continuation turn (#2551).
        assert_eq!(upstream.len(), 3);
        assert_eq!(upstream[0]["content"][0]["text"], "current instruction");
        assert_eq!(upstream[1]["content"][0]["text"], "earlier user");
        assert_eq!(upstream[2]["content"][0]["text"], "current user");
        assert!(
            !serde_json::to_string(&upstream)
                .expect("serialize upstream transcript")
                .contains("old instruction"),
            "a continuation must replace rather than deduplicate developer instructions"
        );
    }

    // ----------------------------------------------------------------
    // Orphaned tool results after history compaction.
    //
    // Production failure (bridge.jsonl, fastled session): every turn for
    // 4.5 hours returned
    //   invalid_request_error: No tool call found for function call output
    //   with call_id ...
    // classified `permanent`, so no retry, and the orphan was persisted —
    // wedging the session until the process restarted.
    // ----------------------------------------------------------------
    fn call_item(call_id: &str) -> serde_json::Value {
        json!({"type": "function_call", "call_id": call_id, "name": "bash", "arguments": "{}"})
    }

    fn output_item(call_id: &str, output: serde_json::Value) -> serde_json::Value {
        json!({"type": "function_call_output", "call_id": call_id, "output": output})
    }

    fn orphan_count(items: &[serde_json::Value]) -> usize {
        let known: std::collections::HashSet<&str> = items
            .iter()
            .filter(|item| response_item_kind(item) == "function_call")
            .filter_map(call_id)
            .collect();
        items
            .iter()
            .filter(|item| response_item_kind(item) == "function_call_output")
            .filter(|item| call_id(item).is_none_or(|id| !known.contains(id)))
            .count()
    }

    #[test]
    fn assembly_repairs_a_tool_result_whose_call_compaction_elided() {
        // The exact post-compaction shape: history is just the compaction
        // summary, and the in-flight tool result arrives after it.
        let history = vec![json!({"type": "compaction", "id": "cmp_1"})];
        let pending = vec![output_item("call_1", json!("18C"))];

        let (upstream_input, turn_input, recovered) = history_aware_input(&history, &[], &pending);

        assert_eq!(recovered, 0);
        assert_eq!(
            orphan_count(&upstream_input),
            0,
            "the request sent upstream must contain no orphaned tool result"
        );
        // `turn_input` is what `record_successful_turn` persists. Repairing
        // only the outgoing copy would store the orphan again and the fix
        // would work exactly once.
        assert_eq!(
            orphan_count(&turn_input),
            0,
            "the PERSISTED suffix must be repaired too, or the wedge returns"
        );
        // The result itself survives, as a user message.
        let text = serde_json::to_string(&turn_input).unwrap();
        assert!(text.contains("18C"), "tool output must not be lost: {text}");
        assert!(text.contains("call_1"));
        assert!(text.contains("compaction"));
        // And it is now a plain message, so no pairing rule applies to it.
        assert!(turn_input
            .iter()
            .all(|item| response_item_kind(item) != "function_call_output"));
    }

    #[test]
    fn assembly_leaves_a_properly_paired_tool_result_alone() {
        let history = vec![
            json!({"type": "message", "role": "user", "content": []}),
            call_item("call_1"),
        ];
        let pending = vec![output_item("call_1", json!("18C"))];

        let (upstream_input, turn_input, recovered) = history_aware_input(&history, &[], &pending);

        assert_eq!(recovered, 0);
        // Untouched: it is still a function_call_output, matched by its call.
        assert_eq!(response_item_kind(&turn_input[0]), "function_call_output");
        assert_eq!(orphan_count(&upstream_input), 0);
        validate_continuation_input(&upstream_input, &[]).expect("paired input stays valid");
    }

    /// #960: a failed turn leaves these real outputs in Claude's replay but
    /// outside the normal final-assistant pending suffix.
    #[test]
    fn assembly_recovers_failed_turn_outputs_in_replay_order() {
        let history = vec![call_item("call_1"), call_item("call_2")];
        let translated = vec![
            call_item("call_1"),
            call_item("call_2"),
            output_item("call_1", json!("one")),
            output_item("call_2", json!("two")),
            json!({"type": "message", "role": "assistant", "content": []}),
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}),
        ];
        let pending = vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}),
        ];

        let (upstream_input, turn_input, recovered) =
            history_aware_input(&history, &translated, &pending);

        assert_eq!(recovered, 2);
        assert_eq!(call_id(&turn_input[0]), Some("call_1"));
        assert_eq!(call_id(&turn_input[1]), Some("call_2"));
        assert_eq!(turn_input[2]["content"][0]["text"], "continue");
        validate_continuation_input(&upstream_input, &translated)
            .expect("real replayed outputs complete canonical history");
    }

    #[test]
    fn validator_catches_an_orphaned_output() {
        // The blind spot: outputs only ever *decremented* the unresolved-call
        // map, so one with no call at all passed straight through to upstream.
        let input = vec![
            json!({"type": "compaction", "id": "cmp_1"}),
            output_item("call_1", json!("18C")),
        ];
        let error = validate_continuation_input(&input, &[]).unwrap_err();
        match error {
            PipelineError::ContinuationInvariant(failure) => {
                assert_eq!(failure.orphaned_output_count, 1);
                // Distinct from the opposite invariant, so the bridge log can
                // tell the two apart.
                assert_eq!(failure.unmatched_call_count, 0);
            }
            other => panic!("expected ContinuationInvariant, got {other:?}"),
        }
    }

    #[test]
    fn repair_preserves_structured_output_and_reports_a_count() {
        let mut pending = vec![
            output_item("call_1", json!({"stdout": "ok", "exit": 0})),
            output_item("call_2", json!("plain")),
        ];
        let repaired = repair_orphaned_outputs(&mut pending, &[]);
        assert_eq!(repaired, 2);
        let text = serde_json::to_string(&pending).unwrap();
        assert!(
            text.contains("stdout"),
            "structured output must survive: {text}"
        );
        assert!(text.contains("plain"));
    }

    #[test]
    fn repair_is_a_no_op_when_the_call_is_present() {
        let mut pending = vec![call_item("call_1"), output_item("call_1", json!("18C"))];
        let before = pending.clone();
        assert_eq!(repair_orphaned_outputs(&mut pending, &[]), 0);
        assert_eq!(pending, before);
    }

    #[test]
    fn empty_history_repairs_the_harness_transcript_that_actually_wedged_production() {
        // The real sequence from the fastled session:
        //   /context_compact -> validate_canonical_history fails (tool batch
        //   outstanding) -> begin_harness_compaction_fallback -> clear_items()
        // History is now EMPTY, and the harness keeps resending its own
        // compacted transcript whose assistant tool_use was dropped while the
        // user tool_result survived.
        //
        // This branch returns early, so it must repair the client's input
        // directly — and it is why the wedge survives a daemon restart: the
        // poison lives in the harness transcript, resent verbatim every turn.
        let translated = vec![
            json!({"type": "message", "role": "user", "content": []}),
            output_item("call_1", json!("18C")),
        ];
        let (upstream_input, turn_input, recovered) = history_aware_input(&[], &translated, &[]);

        assert_eq!(recovered, 0);
        assert_eq!(orphan_count(&upstream_input), 0);
        assert_eq!(orphan_count(&turn_input), 0);
        validate_continuation_input(&upstream_input, &[])
            .expect("the repaired request must be sendable");
        assert!(serde_json::to_string(&upstream_input)
            .unwrap()
            .contains("18C"));
    }
}

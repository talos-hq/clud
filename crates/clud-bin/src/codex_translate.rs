//! Anthropic Messages request -> OpenAI Responses request translation
//! (issues #627 phase 3 step 2, #750 conformance).
//!
//! HTTP-free by design: the bridge owns sockets and credentials, and keeping
//! the mapping a pure function is what lets the table tests drive every shape
//! directly.
//!
//! # Conformance
//!
//! The behaviour here follows two live implementations rather than a reading
//! of the API surface: CLIProxyAPI's `internal/translator/codex/claude/` (MIT)
//! and the `openai/codex` client (Apache-2.0). Where they disagree, the
//! difference is auth-mode dependent and is modelled as
//! [`SystemPlacement`]. Behaviour is matched; no code is copied.
//!
//! The most important consequence, and a reversal of this module's first
//! version: **translation is total.** Anthropic fields with no Responses
//! equivalent are dropped, not rejected. The bridge sits between two clients we
//! do not control, so a 4xx we invent is a failure the user cannot act on --
//! Claude Code really does send `stop_sequences` and replayed `thinking`
//! blocks. Only genuinely malformed input is an error.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::backend::ModelProvider;
use crate::codex_model::{Effort, ModelSpec, SelectionError};
use crate::provider_catalog;

/// Return the new pending input for a continuation request.
///
/// The harness replays its display history every turn, while canonical
/// Responses history already owns successful turns. A continuation therefore
/// contributes the complete message suffix after its final assistant turn.
/// That suffix is normally one user message, but Claude Code persists large
/// parallel tool batches as consecutive user messages with one result apiece.
/// Keeping only `messages.last()` silently drops every earlier result and
/// leaves canonical `function_call` items unmatched upstream (#839).
///
/// A request with no assistant turn retains the legacy final-message fallback.
/// The first request is separately seeded from [`Translated::request`], so
/// this fallback matters only to compatibility clients continuing an already
/// populated bridge history without replaying an assistant message.
pub fn pending_input_items_as_values(
    body: &[u8],
) -> Result<Vec<serde_json::Value>, TranslateError> {
    let request: MessagesRequest = serde_json::from_slice(body)
        .map_err(|error| invalid(format!("could not decode request: {error}")))?;
    let last = request
        .messages
        .len()
        .checked_sub(1)
        .ok_or_else(|| invalid("messages must not be empty"))?;
    let pending_start = request
        .messages
        .iter()
        .rposition(|message| message.role == "assistant")
        .map_or(last, |assistant| {
            if assistant < last {
                assistant + 1
            } else {
                last
            }
        });
    let shell_calls = shell_call_ids(&request);
    let mut items = Vec::new();
    for message in &request.messages[pending_start..] {
        translate_message(message, &mut items, &shell_calls)?;
    }
    input_items_as_values(&items)
}

/// Responses rejects identifiers longer than this.
pub(crate) const MAX_IDENTIFIER_LEN: usize = 64;

/// Claude Code tags one system block for billing attribution. It is transport
/// metadata, not instruction text, and forwarding it upstream pollutes the
/// prompt.
const CLAUDE_ATTRIBUTION_PREFIX: &str = "x-anthropic-billing-header:";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TranslateError {
    /// The request does not satisfy the Messages API's own contract.
    #[error("invalid Messages request: {0}")]
    Invalid(String),
}

/// Serialize translated input as opaque Responses items for canonical history.
///
/// Each item is encoded directly from its wire representation, preserving the
/// per-item ordering required by Responses tool and reasoning transcripts.
pub fn input_items_as_values(
    items: &[InputItem],
) -> Result<Vec<serde_json::Value>, TranslateError> {
    items
        .iter()
        .map(|item| {
            serde_json::to_value(item)
                .map_err(|error| invalid(format!("could not encode Responses input: {error}")))
        })
        .collect()
}

fn invalid(what: impl Into<String>) -> TranslateError {
    TranslateError::Invalid(what.into())
}

/// Where an Anthropic `system` prompt belongs upstream.
///
/// The two references disagree, and both are right in context:
///
/// - `openai/codex` puts its system prompt in `instructions` and never emits a
///   `system`/`developer` message.
/// - CLIProxyAPI leaves `instructions` empty and prepends a `developer`
///   message, because it targets `chatgpt.com/backend-api/codex`, where
///   `instructions` is expected to be *Codex's own* base prompt. A proxy
///   impersonating Codex cannot put a foreign client's prompt there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemPlacement {
    /// Direct platform API (`api.openai.com/v1/responses`).
    Instructions,
    /// Codex backend (`chatgpt.com/backend-api/codex/responses`).
    DeveloperMessage,
}

/// The default selection when a request does not carry one of its own.
///
/// The selected row lives only in the shared provider catalog; the translator
/// consumes it rather than maintaining a second model/default table. Sol at
/// low effort is the reviewed fallback (#1254); explicit and saved provider
/// selections still win. A request naming a model through
/// [`resolve_selection`] still wins.
pub fn default_model_spec() -> ModelSpec {
    let model = provider_catalog::reviewed_default_model(ModelProvider::Codex)
        .expect("the Codex catalog must contain exactly one reviewed provider default");
    ModelSpec {
        model: crate::codex_runtime::wire_id(model.cli_id, model.wire_id),
        effort: None,
    }
}

#[derive(Debug, Clone)]
pub struct TranslateOptions {
    /// Model *and* optional effort. Carried as one value because the two are
    /// selected together (`terra@max`) and a split would let them drift.
    pub model: ModelSpec,
    pub system_placement: SystemPlacement,
    /// Buys prompt-cache hits across turns. Omitting it silently pays full
    /// input price on every request.
    pub prompt_cache_key: Option<String>,
    pub service_tier: Option<String>,
}

impl Default for TranslateOptions {
    fn default() -> Self {
        Self {
            model: default_model_spec(),
            system_placement: SystemPlacement::Instructions,
            prompt_cache_key: None,
            service_tier: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Incoming: Anthropic Messages
// ---------------------------------------------------------------------------

/// Unknown fields are tolerated: Anthropic adds request fields regularly and a
/// `deny_unknown_fields` here would turn every additive API change into an
/// outage.
#[derive(Debug, Default, Deserialize)]
pub struct MessagesRequest {
    pub model: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    pub system: Option<SystemPrompt>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub stream: bool,
    pub thinking: Option<Thinking>,
    /// Where `/effort`, `--effort`, `CLAUDE_CODE_EFFORT_LEVEL` and the
    /// `/model` effort slider land. Previously unmodelled, and because
    /// unknown fields are tolerated (deliberately, see above), the user's
    /// effort choice was dropped without a trace — every request ran at the
    /// ladder's `medium` no matter what was selected.
    pub output_config: Option<OutputConfig>,
    // `max_tokens`, `temperature`, `top_p`, `top_k`, `stop_sequences` and
    // `metadata` are deliberately not modelled. Neither reference forwards
    // them, and reasoning models reject sampling parameters outright.
}

/// The harness's own output/effort settings block. Only `effort` is read;
/// structured-output format and task budget are not ours to interpret.
#[derive(Debug, Default, Deserialize)]
pub struct OutputConfig {
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Deserialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Content,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        /// #1533: absent on some compat-route blocks. Without the default the
        /// whole request failed to decode (untagged `Content`), not just this
        /// block; `tool_use_arguments` maps the resulting `null` to `{}`.
        #[serde(default)]
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<ToolResultContent>,
        #[serde(default)]
        is_error: bool,
    },
    Thinking {
        #[serde(default)]
        signature: Option<String>,
    },
    RedactedThinking {},
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 {
        media_type: String,
        data: String,
    },
    Url {
        url: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ToolChoice {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: Option<String>,
    #[serde(default)]
    pub disable_parallel_tool_use: bool,
}

#[derive(Debug, Deserialize)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub kind: String,
    pub budget_tokens: Option<i64>,
}

// ---------------------------------------------------------------------------
// Outgoing: OpenAI Responses
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Vec<InputItem>,
    /// Both references hardcode this. `complete()` folds the events back into
    /// a single message rather than maintaining a second mapping.
    pub stream: bool,
    /// Codex sends `false` for every non-Azure provider. Leaving it unset
    /// inherits server-side retention, which is a posture, not a default.
    pub store: bool,
    /// Load-bearing with `store: false`: the server keeps no state, so
    /// reasoning has to round-trip as `encrypted_content`.
    pub include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponsesToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CompactRequest {
    pub model: String,
    /// Canonical Responses items supplied verbatim by the history store.
    ///
    /// Compaction must retain opaque server fields such as generated IDs and
    /// encrypted reasoning payloads, so this deliberately is not `InputItem`.
    pub input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    pub parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Codex sends this object even when no text-specific option is selected.
    pub text: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CompactResponse {
    pub output: Vec<InputItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompactionError {
    #[error("cannot compact a request without pending input")]
    MissingPendingInput,
    #[error("compact response contained no output")]
    EmptyOutput,
    #[error("compact response was malformed")]
    MalformedOutput,
    #[error("compact response contained unsupported output")]
    UnsupportedOutput,
}

impl ResponsesRequest {
    /// Builds a non-streaming compaction payload from the typed request input.
    ///
    /// This compatibility path separates the final pending item before
    /// compaction; recovery callers retain opaque entries via
    /// [`Self::compact_request_for_history`] instead.
    pub fn compact_request(&self) -> Result<CompactRequest, CompactionError> {
        let (_, history) = self
            .input
            .split_last()
            .ok_or(CompactionError::MissingPendingInput)?;
        let input = history
            .iter()
            .cloned()
            .map(|item| serde_json::to_value(item).map_err(|_| CompactionError::MalformedOutput))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CompactRequest {
            model: self.model.clone(),
            input,
            instructions: self.instructions.clone(),
            tools: self.tools.clone(),
            parallel_tool_calls: self.parallel_tool_calls.unwrap_or(true),
            reasoning: self.reasoning.clone(),
            service_tier: self.service_tier.clone(),
            prompt_cache_key: self.prompt_cache_key.clone(),
            text: serde_json::json!({}),
        })
    }

    /// Rebuilds the inference request from the server's compaction replacement
    /// and exactly one pending final item. No compaction result can overwrite
    /// the transcript unless it contains a typed, nonempty compaction item.
    pub fn retry_with_compaction(&self, compact: CompactResponse) -> Result<Self, CompactionError> {
        let pending = self
            .input
            .last()
            .cloned()
            .ok_or(CompactionError::MissingPendingInput)?;
        let compact = validated_compact_output(compact)?;
        let mut retry = self.clone();
        retry.input = compact;
        retry.input.push(pending);
        Ok(retry)
    }

    /// Build the compaction request from the canonical transcript rather than
    /// the display-oriented Messages replay body.
    pub fn compact_request_for_history(
        &self,
        history: Vec<serde_json::Value>,
    ) -> Result<CompactRequest, CompactionError> {
        if history.is_empty() {
            return Err(CompactionError::MissingPendingInput);
        }
        Ok(CompactRequest {
            model: self.model.clone(),
            input: history,
            instructions: self.instructions.clone(),
            tools: self.tools.clone(),
            parallel_tool_calls: self.parallel_tool_calls.unwrap_or(true),
            reasoning: self.reasoning.clone(),
            service_tier: self.service_tier.clone(),
            prompt_cache_key: self.prompt_cache_key.clone(),
            text: serde_json::json!({}),
        })
    }
}

/// Validate compact output once, then preserve its opaque wire form for the
/// canonical transcript and the recovery request.
pub fn compact_output_as_values(
    compact: CompactResponse,
) -> Result<Vec<serde_json::Value>, CompactionError> {
    validated_compact_output(compact)?
        .into_iter()
        .map(|item| serde_json::to_value(item).map_err(|_| CompactionError::MalformedOutput))
        .collect()
}

fn validated_compact_output(compact: CompactResponse) -> Result<Vec<InputItem>, CompactionError> {
    if compact.output.is_empty() {
        return Err(CompactionError::EmptyOutput);
    }
    if compact.output.iter().any(|item| {
        !matches!(item, InputItem::Compaction { encrypted_content, .. } if !encrypted_content.is_empty())
    }) {
        return Err(CompactionError::UnsupportedOutput);
    }
    Ok(compact.output)
}

/// A Messages `content` array does not map one-to-one onto Responses items: an
/// assistant turn holding text plus two `tool_use` blocks becomes one message
/// item and two `function_call` items. Order is preserved because the model
/// reads the result as a transcript.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Message {
        role: String,
        content: Vec<ContentPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        /// A JSON-encoded *string*, not an object.
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: FunctionCallOutput,
    },
    Reasoning {
        summary: Vec<serde_json::Value>,
        encrypted_content: String,
    },
    /// Server-produced transcript replacement returned by `/responses/compact`.
    /// It must travel back unchanged as Responses input on the retry.
    Compaction {
        encrypted_content: String,
        /// Retain the server-issued id and fields introduced after this bridge
        /// version. Compaction entries are opaque retry input, not a schema the
        /// bridge is allowed to reconstruct.
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
}

/// `function_call_output.output` is untagged upstream: a plain string, or an
/// array of content parts when the tool returned structured content.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum FunctionCallOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText { text: String },
    InputImage { image_url: String },
    OutputText { text: String },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
    pub strict: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ResponsesToolChoice {
    Mode(&'static str),
    Function {
        #[serde(rename = "type")]
        kind: &'static str,
        name: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Reasoning {
    pub effort: &'static str,
}

/// A translated request plus the state the response side needs to undo the
/// identifier rewriting this module had to perform.
#[derive(Debug)]
pub struct Translated {
    pub request: ResponsesRequest,
    /// Shortened tool name -> original, so the client sees the names it sent.
    pub tool_names: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Shorten to fit the upstream identifier limit, stably and collision-safely.
///
/// MCP tool names routinely exceed 64 characters, as do some `tool_use` ids.
/// The hash suffix keeps distinct inputs distinct; the mapping is recorded so
/// the response side can reverse it.
pub fn shorten_identifier(id: &str) -> String {
    if id.len() <= MAX_IDENTIFIER_LEN {
        return id.to_string();
    }
    let digest = Sha256::digest(id.as_bytes());
    // 8 bytes (64 bits) rather than the 4 (32 bits) this used to keep: once
    // `sanitize_tool_id` (codex_sse.rs) stopped colliding distinct ids by
    // construction, this truncation became the only residual collision
    // surface for identifiers over 64 chars, so it gets the wider margin.
    let suffix: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let keep = MAX_IDENTIFIER_LEN - suffix.len() - 1;
    // Truncate on a char boundary; identifiers are ASCII in practice but a
    // panic here would be a denial of service triggered by request content.
    let mut boundary = keep.min(id.len());
    while boundary > 0 && !id.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}_{}", &id[..boundary], suffix)
}

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

/// Whether a signature is a GPT reasoning `encrypted_content` blob.
///
/// Replaying a Claude-native or Gemini signature into a Codex `reasoning` item
/// is a hard upstream error, so foreign signatures are dropped rather than
/// forwarded. The framing checked here is Fernet: version byte `0x80`, then an
/// 8-byte timestamp, a 16-byte IV, a 16-byte-aligned ciphertext, and a 32-byte
/// HMAC.
pub fn is_gpt_reasoning_signature(signature: &str) -> bool {
    if !signature.starts_with("gAAAA") || signature.trim() != signature {
        return false;
    }
    let Ok(decoded) = base64_decode_urlsafe(signature) else {
        return false;
    };
    if decoded.len() < 73 || decoded[0] != 0x80 {
        return false;
    }
    decoded.len().saturating_sub(1 + 8 + 16 + 32) % 16 == 0
}

fn base64_decode_urlsafe(value: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE
        .decode(value)
        .or_else(|_| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| ())
        })
        .map_err(|_| ())
}

/// Map Anthropic's token-denominated thinking budget onto the effort ladder.
///
/// `None` means *the request stated no budget*, which is not the same as
/// "medium" — it defers to the model's own catalog default. That distinction
/// is load-bearing: the harness sends `thinking: {"type":"adaptive"}` with no
/// `budget_tokens` for model ids it does not recognize, i.e. for every id the
/// bridge serves, so treating a missing budget as an explicit `medium` pinned
/// every request to `medium` regardless of the model.
///
/// Thresholds follow CLIProxyAPI's `ConvertBudgetToLevel` with two
/// corrections for this model family: the sub-512 band emitted `minimal`,
/// which **no gpt-5.6 model accepts** (the family starts at `low`), and the
/// ladder topped out at `xhigh` so `max` — supported by all three — was
/// unreachable from any budget.
fn effort_for_budget(budget: i64) -> Option<Effort> {
    // One ladder, one owner: `provider_catalog::EffortLevel::from_budget` also
    // backs the CLIProxyAPI-compatible numeric model-id suffix.
    Effort::from_budget(budget)
}

fn effort_from_thinking(thinking: Option<&Thinking>) -> Option<Effort> {
    let thinking = thinking?;
    match thinking.kind.as_str() {
        "disabled" => Some(Effort::None),
        // "enabled", "adaptive", "auto" and anything else fall through to the
        // budget ladder rather than erroring: an unknown mode is not a reason
        // to fail a request.
        _ => effort_for_budget(thinking.budget_tokens.unwrap_or(-1)),
    }
}

/// Resolve effort across all four channels, most explicit first.
///
/// 1. `@effort` on the model id — the only channel that cannot be dropped in
///    transit, so it outranks everything.
/// 2. `output_config.effort` — the native `/effort` control, when the harness
///    decides to send it.
/// 3. The `thinking` budget ladder — inferred, and only when a budget was
///    actually stated.
/// 4. The model's own catalog default.
///
/// A *stated but unsupported* `output_config.effort` is a **400, not a
/// fallback** (#821). It used to fall through to the next channel on the
/// theory that the harness's own field should never fail a turn, but the
/// channel below it is the model's default, so the request then ran at an
/// effort the user did not choose and had no way to observe. `minimal` is the
/// case that forced this: it is a real Responses value, so `/effort minimal`
/// looks accepted, but gpt-5.6 accepts only `none`/`low`/`medium`/`high`/
/// `xhigh`/`max` and the turn silently ran at `medium`. Failing loudly with
/// the accepted values is the only outcome the user can act on.
///
/// An *absent* or empty effort is still "unspecified" and defers, because
/// that is the harness declining to send the field rather than the user
/// naming a value.
fn effort_for(request: &MessagesRequest, spec: &ModelSpec) -> Result<Effort, TranslateError> {
    let stated = request
        .output_config
        .as_ref()
        .and_then(|config| config.effort.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let effort = if let Some(effort) = spec.effort {
        effort
    } else if let Some(value) = stated {
        Effort::parse(value).ok_or_else(|| {
            invalid(
                SelectionError::UnknownEffort {
                    given: value.to_string(),
                }
                .to_string(),
            )
        })?
    } else {
        effort_from_thinking(request.thinking.as_ref()).unwrap_or_else(|| spec.effective_effort())
    };
    if let Some(model) = crate::codex_model::model_by_id(&spec.model) {
        if !model.supported_efforts.contains(&effort) {
            return Err(invalid(format!(
                "unsupported effort '{}' for model '{}'",
                effort.as_str(),
                spec.model
            )));
        }
    }
    Ok(effort)
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// Resolve the upstream selection from what the client asked for.
///
/// Claude Code sends its own model id, which is meaningless upstream, so any
/// `claude*` id resolves to the configured default. Any other id is parsed as
/// a `<model>[@<effort>]` selection -- that is how a user pins a specific
/// Codex model and effort from `/model`, and it works because a custom
/// `ANTHROPIC_BASE_URL` makes the gateway the owner of the model namespace,
/// so the harness forwards the string without validating it itself. The
/// gateway does validate: `codex_bridge::serve_codex_discovery_messages`
/// refuses a non-`claude*` id it cannot resolve in clud's catalog **unless it
/// is the id pinned at launch** (#1022). So what reaches here over HTTP is
/// either a catalog-known id it already rewrote, or an uncataloged full id the
/// user selected themselves -- which `ModelSpec::parse` forwards by design.
/// Both are valid inputs here; neither needs a catalog row.
///
/// A selection that does not parse is a **400, not a fallback**. Silently
/// substituting the default for `/model tera` would bill a model the user did
/// not choose and give them no way to notice.
///
/// The `claude*` branch is the deliberate exception to that rule (#1007). It
/// cannot be a 400: the harness's own side-model calls carry built-in
/// `claude-*` ids no matter what was launched, so refusing the prefix would
/// break every session. The cost is that a user's pick from the built-in
/// Anthropic lineup lands here looking the same and is served by the default.
/// The bridge reports that case instead of failing it --
/// `codex_bridge::is_anthropic_main_model_pick` warns once and logs a
/// `model_substituted` event -- because the picker cannot be constrained from
/// clud's side (DD-054).
pub fn resolve_selection(
    requested: Option<&str>,
    default: &ModelSpec,
) -> Result<ModelSpec, TranslateError> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(model) if !model.to_ascii_lowercase().starts_with("claude") => {
            ModelSpec::parse(model).map_err(|error| invalid(error.to_string()))
        }
        _ => Ok(default.clone()),
    }
}

fn system_text(system: &SystemPrompt) -> String {
    let mut parts = Vec::new();
    match system {
        SystemPrompt::Text(text) => {
            if !is_attribution(text) && !text.is_empty() {
                parts.push(text.clone());
            }
        }
        SystemPrompt::Blocks(blocks) => {
            for block in blocks {
                if block.kind != "text" {
                    continue;
                }
                if let Some(text) = block.text.as_ref() {
                    if !text.is_empty() && !is_attribution(text) {
                        parts.push(text.clone());
                    }
                }
            }
        }
    }
    parts.join("\n\n")
}

fn is_attribution(text: &str) -> bool {
    text.trim_start().starts_with(CLAUDE_ATTRIBUTION_PREFIX)
}

fn image_url(source: &ImageSource) -> Option<String> {
    match source {
        // Responses takes one URL field for both, so base64 travels as a data
        // URL.
        ImageSource::Base64 { media_type, data } => {
            Some(format!("data:{media_type};base64,{data}"))
        }
        ImageSource::Url { url } => Some(url.clone()),
        // An unrepresentable source is dropped, not fatal.
        ImageSource::Other => None,
    }
}

/// Flatten a tool result. Structured content becomes an array of parts;
/// anything else becomes a string.
fn tool_result_output(content: Option<&ToolResultContent>, is_error: bool) -> FunctionCallOutput {
    let prefix = if is_error { "ERROR: " } else { "" };
    match content {
        None => FunctionCallOutput::Text(prefix.trim_end().to_string()),
        Some(ToolResultContent::Text(text)) => FunctionCallOutput::Text(format!("{prefix}{text}")),
        Some(ToolResultContent::Blocks(blocks)) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text } => parts.push(ContentPart::InputText {
                        text: format!("{prefix}{text}"),
                    }),
                    // Images inside a tool result are representable, and
                    // dropping them would look to the model like the tool
                    // returned nothing.
                    ContentBlock::Image { source } => {
                        if let Some(url) = image_url(source) {
                            parts.push(ContentPart::InputImage { image_url: url });
                        }
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                FunctionCallOutput::Text(String::new())
            } else {
                FunctionCallOutput::Parts(parts)
            }
        }
    }
}

fn push_message(items: &mut Vec<InputItem>, role: &str, parts: Vec<ContentPart>) {
    if !parts.is_empty() {
        items.push(InputItem::Message {
            role: role.to_string(),
            content: parts,
        });
    }
}

fn shell_call_ids(request: &MessagesRequest) -> std::collections::HashSet<&str> {
    request
        .messages
        .iter()
        .flat_map(|message| match &message.content {
            Content::Blocks(blocks) => blocks.as_slice(),
            Content::Text(_) => &[],
        })
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, .. }
                if matches!(
                    name.as_str(),
                    "Bash" | "bash" | "Shell" | "shell" | "shell_command" | "exec_command"
                ) =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect()
}

fn capped_tool_result(
    content: Option<&ToolResultContent>,
    is_error: bool,
    shell: bool,
) -> Result<FunctionCallOutput, TranslateError> {
    let output = tool_result_output(content, is_error);
    if !shell {
        return Ok(output);
    }
    let limit = super::tool_result_cap::configured_limit();
    let cap = |text: &str| {
        super::tool_result_cap::cap(text, limit)
            .map_err(|error| invalid(format!("could not spill shell tool output: {error}")))
    };
    match output {
        FunctionCallOutput::Text(text) => Ok(FunctionCallOutput::Text(cap(&text)?)),
        FunctionCallOutput::Parts(mut parts) => {
            let text = parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::InputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if limit == 0 || text.len() <= limit {
                return Ok(FunctionCallOutput::Parts(parts));
            }
            let capped = cap(&text)?;
            let mut first = true;
            parts.retain_mut(|part| match part {
                ContentPart::InputText { text } if first => {
                    *text = capped.clone();
                    first = false;
                    true
                }
                ContentPart::InputText { .. } => false,
                _ => true,
            });
            Ok(FunctionCallOutput::Parts(parts))
        }
    }
}

fn translate_message(
    message: &Message,
    items: &mut Vec<InputItem>,
    shell_calls: &std::collections::HashSet<&str>,
) -> Result<(), TranslateError> {
    // A `system` role inside `messages` is not a Responses role. Wrapping it as
    // a user turn keeps the instruction rather than dropping or rejecting it.
    let (role, wrap_system) = match message.role.as_str() {
        "assistant" => ("assistant", false),
        "user" => ("user", false),
        _ => ("user", true),
    };
    let text_part = |text: String| {
        let text = if wrap_system {
            format!("<system-reminder>{text}</system-reminder>")
        } else {
            text
        };
        if role == "assistant" {
            ContentPart::OutputText { text }
        } else {
            ContentPart::InputText { text }
        }
    };

    match &message.content {
        Content::Text(text) => push_message(items, role, vec![text_part(text.clone())]),
        Content::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text } => parts.push(text_part(text.clone())),
                    ContentBlock::Image { source } => {
                        if let Some(url) = image_url(source) {
                            parts.push(ContentPart::InputImage { image_url: url });
                        }
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        // Flush text accumulated before the call so the
                        // transcript keeps its original order.
                        push_message(items, role, std::mem::take(&mut parts));
                        items.push(InputItem::FunctionCall {
                            call_id: shorten_identifier(id),
                            name: shorten_identifier(name),
                            arguments: tool_use_arguments(input),
                        });
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        push_message(items, role, std::mem::take(&mut parts));
                        items.push(InputItem::FunctionCallOutput {
                            call_id: shorten_identifier(tool_use_id),
                            output: capped_tool_result(
                                content.as_ref(),
                                *is_error,
                                shell_calls.contains(tool_use_id.as_str()),
                            )?,
                        });
                    }
                    ContentBlock::Thinking { signature } => {
                        // The round-trip carrier: the signature *is* the
                        // reasoning item's `encrypted_content`. A foreign or
                        // absent signature is dropped, because replaying one
                        // upstream is a hard error.
                        let Some(signature) = signature else { continue };
                        if !is_gpt_reasoning_signature(signature) {
                            continue;
                        }
                        push_message(items, role, std::mem::take(&mut parts));
                        items.push(InputItem::Reasoning {
                            summary: Vec::new(),
                            encrypted_content: signature.clone(),
                        });
                    }
                    ContentBlock::RedactedThinking {} | ContentBlock::Other => {}
                }
            }
            push_message(items, role, parts);
        }
    }
    Ok(())
}

/// Normalise a Claude tool schema into Responses `parameters`.
///
/// An object schema with no `properties` is rejected upstream, so an empty map
/// is injected. Claude-only keys are stripped.
fn normalize_tool_parameters(schema: Option<&serde_json::Value>) -> serde_json::Value {
    let mut schema = match schema {
        Some(serde_json::Value::Object(map)) => map.clone(),
        _ => return serde_json::json!({"type": "object", "properties": {}}),
    };
    for key in ["$schema", "cache_control", "defer_loading"] {
        schema.remove(key);
    }
    schema
        .entry("type")
        .or_insert_with(|| serde_json::json!("object"));
    if schema.get("type").and_then(serde_json::Value::as_str) == Some("object")
        && !schema.contains_key("properties")
    {
        schema.insert(
            "properties".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }
    serde_json::Value::Object(schema)
}

fn translate_tool_choice(
    choice: &ToolChoice,
    tool_names: &HashMap<String, String>,
) -> (Option<ResponsesToolChoice>, Option<bool>) {
    let mapped = match choice.kind.as_str() {
        "any" => ResponsesToolChoice::Mode("required"),
        "none" => ResponsesToolChoice::Mode("none"),
        "tool" => match choice.name.as_ref() {
            Some(name) => {
                let shortened = shorten_identifier(name);
                // Prefer the shortened form we actually sent in `tools`.
                let name = tool_names
                    .iter()
                    .find(|(short, _)| short.as_str() == shortened)
                    .map(|(short, _)| short.clone())
                    .unwrap_or(shortened);
                ResponsesToolChoice::Function {
                    kind: "function",
                    name,
                }
            }
            // A malformed choice falls back to auto rather than failing.
            None => ResponsesToolChoice::Mode("auto"),
        },
        // "auto" and anything unrecognised.
        _ => ResponsesToolChoice::Mode("auto"),
    };
    // Anthropic expresses this as an opt-out; Responses as an opt-in.
    let parallel = Some(!choice.disable_parallel_tool_use);
    (Some(mapped), parallel)
}

/// Translate a decoded Messages request.
pub fn translate_request(
    request: &MessagesRequest,
    options: &TranslateOptions,
) -> Result<Translated, TranslateError> {
    if request.messages.is_empty() {
        return Err(invalid("messages must not be empty"));
    }

    let mut input = Vec::new();
    let system = request.system.as_ref().map(system_text).unwrap_or_default();
    let instructions = match (options.system_placement, system.is_empty()) {
        (_, true) => None,
        (SystemPlacement::Instructions, false) => Some(system.clone()),
        (SystemPlacement::DeveloperMessage, false) => {
            input.push(InputItem::Message {
                role: "developer".to_string(),
                content: vec![ContentPart::InputText { text: system }],
            });
            None
        }
    };

    let shell_calls = shell_call_ids(request);
    for message in &request.messages {
        translate_message(message, &mut input, &shell_calls)?;
    }

    let mut tool_names = HashMap::new();
    let tools = request.tools.as_ref().map(|definitions| {
        definitions
            .iter()
            .map(|definition| {
                let short = shorten_identifier(&definition.name);
                if short != definition.name {
                    tool_names.insert(short.clone(), definition.name.clone());
                }
                ResponsesTool {
                    kind: "function",
                    name: short,
                    description: definition.description.clone(),
                    parameters: normalize_tool_parameters(definition.input_schema.as_ref()),
                    strict: false,
                }
            })
            .collect::<Vec<_>>()
    });

    let (tool_choice, parallel_tool_calls) = match &request.tool_choice {
        Some(choice) => translate_tool_choice(choice, &tool_names),
        // Both references default to allowing parallel calls.
        None => (None, Some(true)),
    };
    // Upstream rejects `tool_choice` without `tools`.
    let tool_choice = tools.as_ref().and(tool_choice);

    let selection = resolve_selection(request.model.as_deref(), &options.model)?;
    let effort = effort_for(request, &selection)?;

    Ok(Translated {
        request: ResponsesRequest {
            model: selection.model,
            input,
            stream: true,
            store: false,
            include: vec!["reasoning.encrypted_content"],
            instructions,
            tools,
            tool_choice,
            parallel_tool_calls,
            reasoning: Some(Reasoning {
                effort: effort.as_str(),
            }),
            prompt_cache_key: options.prompt_cache_key.clone(),
            service_tier: options.service_tier.clone(),
        },
        tool_names,
    })
}

/// Serialize a `tool_use` block's `input` as Responses `arguments` (#1533).
///
/// Anthropic requires `input` to be an object, but a block recorded on a
/// compat route (OpenRouter) can reach the Codex route after a failover with
/// `input` missing, `null`, or as a stringified JSON object. Serializing those
/// verbatim sent `"null"` or a double-encoded string, which the model reads as
/// an empty or garbled call. Missing/`null` becomes `{}`, and a string that
/// already holds a JSON object is passed through unencoded.
fn tool_use_arguments(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Null => "{}".to_string(),
        serde_json::Value::String(text)
            if serde_json::from_str::<serde_json::Value>(text)
                .is_ok_and(|value| value.is_object()) =>
        {
            text.clone()
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

/// Decode raw request bytes and translate them in one step.
pub fn translate_bytes(
    body: &[u8],
    options: &TranslateOptions,
) -> Result<Translated, TranslateError> {
    let request: MessagesRequest = serde_json::from_slice(body)
        .map_err(|error| invalid(format!("could not decode request: {error}")))?;
    translate_request(&request, options)
}

/// Whether the client asked for a streamed reply. Read separately because the
/// upstream request always streams regardless.
pub fn wants_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("stream").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_model::codex_models;
    use serde_json::json;

    fn options() -> TranslateOptions {
        TranslateOptions::default()
    }

    fn translate(value: serde_json::Value) -> Result<Translated, TranslateError> {
        translate_bytes(value.to_string().as_bytes(), &options())
    }

    fn ok(value: serde_json::Value) -> ResponsesRequest {
        translate(value)
            .expect("translation should succeed")
            .request
    }

    /// A valid Fernet-framed blob, so signature round-tripping can be tested
    /// without a live account.
    fn fake_signature() -> String {
        use base64::Engine as _;
        let mut raw = vec![0x80_u8];
        raw.extend_from_slice(&[0_u8; 8]); // timestamp
        raw.extend_from_slice(&[1_u8; 16]); // IV
        raw.extend_from_slice(&[2_u8; 16]); // one ciphertext block
        raw.extend_from_slice(&[3_u8; 32]); // HMAC
        base64::engine::general_purpose::URL_SAFE.encode(raw)
    }

    #[test]
    fn conformance_defaults_are_always_present() {
        let out = ok(json!({"messages": [{"role": "user", "content": "hi"}]}));
        // Both references hardcode these three.
        assert!(out.stream);
        assert!(!out.store);
        assert_eq!(out.include, vec!["reasoning.encrypted_content"]);
        // Effort is always set, defaulting to low for the reviewed Sol row.
        assert_eq!(out.reasoning, Some(Reasoning { effort: "low" }));
        assert_eq!(out.parallel_tool_calls, Some(true));
    }

    /// Regression guard for #750 A2: neither reference forwards sampling
    /// parameters, and reasoning models reject them.
    #[test]
    fn sampling_parameters_never_reach_upstream() {
        let out = ok(json!({
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40,
            "stop_sequences": ["STOP"],
            "metadata": {"user_id": "u"},
        }));
        let wire = serde_json::to_value(&out).unwrap();
        for banned in [
            "temperature",
            "top_p",
            "top_k",
            "max_output_tokens",
            "max_tokens",
            "stop_sequences",
            "metadata",
        ] {
            assert!(wire.get(banned).is_none(), "{banned} must not be sent");
        }
    }

    /// #750 A1: translation is total. Every one of these used to be a 4xx.
    #[test]
    fn previously_rejected_inputs_are_accepted() {
        let cases = [
            json!({"messages": [{"role": "user", "content": "x"}], "top_k": 5}),
            json!({"messages": [{"role": "user", "content": "x"}],
                   "stop_sequences": ["STOP"]}),
            json!({"messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "...", "signature": "not-a-gpt-sig"}]}]}),
            json!({"messages": [{"role": "system", "content": "be terse"}]}),
            json!({"messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": [
                    {"type": "image", "source": {"type": "url", "url": "u"}}]}]}]}),
            json!({"messages": [{"role": "user", "content": [
                {"type": "document", "source": {}}]}]}),
            json!({"messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "file", "file_id": "f"}}]}]}),
            json!({"messages": [{"role": "user", "content": "x"}],
                   "tool_choice": {"type": "sometimes"}}),
            json!({"messages": [{"role": "user", "content": "x"}],
                   "thinking": {"type": "maybe"}}),
        ];
        for case in cases {
            assert!(translate(case.clone()).is_ok(), "must not reject: {case}");
        }
    }

    #[test]
    fn only_malformed_requests_are_errors() {
        assert!(translate(json!({"messages": []})).is_err());
        assert!(translate_bytes(b"not json", &options()).is_err());
    }

    #[test]
    fn system_placement_follows_the_auth_mode() {
        let body = json!({
            "system": "be brief",
            "messages": [{"role": "user", "content": "x"}]
        });

        let direct = translate_bytes(body.to_string().as_bytes(), &options())
            .unwrap()
            .request;
        assert_eq!(direct.instructions.as_deref(), Some("be brief"));
        assert!(matches!(direct.input[0], InputItem::Message { ref role, .. } if role == "user"));

        let codex_backend = translate_bytes(
            body.to_string().as_bytes(),
            &TranslateOptions {
                system_placement: SystemPlacement::DeveloperMessage,
                ..options()
            },
        )
        .unwrap()
        .request;
        // instructions belongs to Codex's own prompt on that backend.
        assert_eq!(codex_backend.instructions, None);
        assert_eq!(
            codex_backend.input[0],
            InputItem::Message {
                role: "developer".into(),
                content: vec![ContentPart::InputText {
                    text: "be brief".into()
                }],
            }
        );
    }

    #[test]
    fn claude_code_billing_attribution_is_stripped() {
        let out = ok(json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: acct-1"},
                {"type": "text", "text": "real instructions"}
            ],
            "messages": [{"role": "user", "content": "x"}]
        }));
        assert_eq!(out.instructions.as_deref(), Some("real instructions"));
    }

    #[test]
    fn a_system_role_message_becomes_a_wrapped_user_turn() {
        let out = ok(json!({"messages": [{"role": "system", "content": "be terse"}]}));
        assert_eq!(
            out.input,
            vec![InputItem::Message {
                role: "user".into(),
                content: vec![ContentPart::InputText {
                    text: "<system-reminder>be terse</system-reminder>".into()
                }],
            }]
        );
    }

    #[test]
    fn thinking_blocks_round_trip_as_reasoning_items() {
        let signature = fake_signature();
        let out = ok(json!({
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": signature},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        }));
        assert_eq!(
            out.input[1],
            InputItem::Reasoning {
                summary: vec![],
                encrypted_content: signature,
            }
        );
        assert_eq!(
            out.input[2],
            InputItem::Message {
                role: "assistant".into(),
                content: vec![ContentPart::OutputText {
                    text: "answer".into()
                }],
            }
        );
    }

    /// Replaying a foreign signature is a hard upstream error, so it is
    /// dropped rather than forwarded.
    #[test]
    fn foreign_or_missing_signatures_are_dropped() {
        for signature in [json!(null), json!("claude-native-sig"), json!("gAAAAshort")] {
            let out = ok(json!({
                "messages": [{"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "x", "signature": signature}]}]
            }));
            assert!(
                !out.input
                    .iter()
                    .any(|item| matches!(item, InputItem::Reasoning { .. })),
                "signature {signature} must not round-trip"
            );
        }
        assert!(is_gpt_reasoning_signature(&fake_signature()));
        assert!(!is_gpt_reasoning_signature(&format!(
            " {} ",
            fake_signature()
        )));
    }

    #[test]
    fn tool_schemas_are_normalised() {
        let out = ok(json!({
            "messages": [{"role": "user", "content": "x"}],
            "tools": [
                {"name": "a", "input_schema": {"type": "object", "$schema": "http://x",
                 "cache_control": {"type": "ephemeral"}}},
                {"name": "b"},
                {"name": "c", "input_schema": "nonsense"}
            ]
        }));
        let tools = out.tools.expect("tools");
        // An object schema with no `properties` is rejected upstream.
        assert_eq!(
            tools[0].parameters,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            tools[1].parameters,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            tools[2].parameters,
            json!({"type": "object", "properties": {}})
        );
        assert!(tools.iter().all(|tool| !tool.strict));
        assert!(tools.iter().all(|tool| tool.kind == "function"));
    }

    #[test]
    fn oversized_identifiers_are_shortened_reversibly() {
        let long_name = format!("mcp__{}", "n".repeat(80));
        let long_id = format!("call_{}", "i".repeat(80));
        let translated = translate_bytes(
            json!({
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": long_id, "name": long_name, "input": {}}]}],
                "tools": [{"name": long_name}]
            })
            .to_string()
            .as_bytes(),
            &options(),
        )
        .unwrap();

        let tools = translated.request.tools.as_ref().unwrap();
        assert!(tools[0].name.len() <= MAX_IDENTIFIER_LEN);
        assert_eq!(
            translated
                .tool_names
                .get(&tools[0].name)
                .map(String::as_str),
            Some(long_name.as_str()),
            "the response side must be able to restore the original name"
        );
        match &translated.request.input[0] {
            InputItem::FunctionCall { call_id, name, .. } => {
                assert!(call_id.len() <= MAX_IDENTIFIER_LEN);
                assert!(name.len() <= MAX_IDENTIFIER_LEN);
            }
            other => panic!("expected a function call, got {other:?}"),
        }
        // Stable and distinct.
        assert_eq!(shorten_identifier(&long_id), shorten_identifier(&long_id));
        assert_ne!(
            shorten_identifier(&format!("{long_id}a")),
            shorten_identifier(&format!("{long_id}b"))
        );
        assert_eq!(shorten_identifier("short"), "short");
    }

    #[test]
    fn tool_results_keep_structured_content() {
        let out = ok(json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": "plain"},
                {"type": "tool_result", "tool_use_id": "b", "content": [
                    {"type": "text", "text": "line"},
                    {"type": "image", "source": {"type": "base64",
                     "media_type": "image/png", "data": "QUJD"}}
                ]},
                {"type": "tool_result", "tool_use_id": "c", "content": "boom",
                 "is_error": true}
            ]}]
        }));
        assert_eq!(
            out.input,
            vec![
                InputItem::FunctionCallOutput {
                    call_id: "a".into(),
                    output: FunctionCallOutput::Text("plain".into()),
                },
                InputItem::FunctionCallOutput {
                    call_id: "b".into(),
                    output: FunctionCallOutput::Parts(vec![
                        ContentPart::InputText {
                            text: "line".into()
                        },
                        ContentPart::InputImage {
                            image_url: "data:image/png;base64,QUJD".into()
                        },
                    ]),
                },
                InputItem::FunctionCallOutput {
                    call_id: "c".into(),
                    output: FunctionCallOutput::Text("ERROR: boom".into()),
                },
            ]
        );
    }

    #[test]
    fn multi_turn_tool_loop_preserves_transcript_order() {
        let out = ok(json!({
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "call_1", "name": "weather",
                     "input": {"city": "Paris"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "18C"}
                ]}
            ]
        }));
        assert_eq!(out.input.len(), 4);
        assert!(matches!(&out.input[1], InputItem::Message { role, .. } if role == "assistant"));
        assert!(
            matches!(&out.input[2], InputItem::FunctionCall { call_id, .. } if call_id == "call_1")
        );
        assert!(matches!(
            &out.input[3],
            InputItem::FunctionCallOutput { .. }
        ));
    }

    /// #1533: a `tool_use` block produced on a compat route (OpenRouter) can
    /// reach the Codex translator after a failover with its `input` missing,
    /// `null`, or as a stringified JSON object. Each must replay as an
    /// object-shaped `arguments` string, never `"null"` or a double-encoded
    /// string, and an ordinary object (empty or not) must survive unchanged.
    #[test]
    fn tool_use_input_always_replays_as_object_arguments() {
        let out = ok(json!({
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "missing", "name": "Bash"},
                    {"type": "tool_use", "id": "null", "name": "Bash", "input": null},
                    {"type": "tool_use", "id": "string", "name": "Bash",
                     "input": "{\"command\":\"ls\"}"},
                    {"type": "tool_use", "id": "empty", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "full", "name": "Bash",
                     "input": {"command": "pwd"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "missing", "content": "a"},
                    {"type": "tool_result", "tool_use_id": "null", "content": "b"},
                    {"type": "tool_result", "tool_use_id": "string", "content": "c"},
                    {"type": "tool_result", "tool_use_id": "empty", "content": "d"},
                    {"type": "tool_result", "tool_use_id": "full", "content": "e"}
                ]}
            ]
        }));
        let arguments: Vec<(String, String)> = out
            .input
            .iter()
            .filter_map(|item| match item {
                InputItem::FunctionCall {
                    call_id, arguments, ..
                } => Some((call_id.clone(), arguments.clone())),
                _ => None,
            })
            .collect();
        let expected = [
            ("missing", "{}"),
            ("null", "{}"),
            ("string", r#"{"command":"ls"}"#),
            ("empty", "{}"),
            ("full", r#"{"command":"pwd"}"#),
        ];
        assert_eq!(
            arguments,
            expected
                .iter()
                .map(|(id, args)| (id.to_string(), args.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn tool_choice_modes_map_and_control_parallelism() {
        let base = |choice: serde_json::Value| {
            json!({
                "messages": [{"role": "user", "content": "x"}],
                "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
                "tool_choice": choice
            })
        };
        assert_eq!(
            ok(base(json!({"type": "auto"}))).tool_choice,
            Some(ResponsesToolChoice::Mode("auto"))
        );
        assert_eq!(
            ok(base(json!({"type": "any"}))).tool_choice,
            Some(ResponsesToolChoice::Mode("required"))
        );
        assert_eq!(
            ok(base(json!({"type": "none"}))).tool_choice,
            Some(ResponsesToolChoice::Mode("none"))
        );
        assert_eq!(
            ok(base(json!({"type": "tool", "name": "lookup"}))).tool_choice,
            Some(ResponsesToolChoice::Function {
                kind: "function",
                name: "lookup".into()
            })
        );
        assert_eq!(
            ok(base(
                json!({"type": "auto", "disable_parallel_tool_use": true})
            ))
            .parallel_tool_calls,
            Some(false)
        );
        // tool_choice without tools is rejected upstream.
        let no_tools = ok(json!({
            "messages": [{"role": "user", "content": "x"}],
            "tool_choice": {"type": "any"}
        }));
        assert_eq!(no_tools.tool_choice, None);
    }

    /// The ladder, with the two corrections this family needs: nothing maps
    /// to `minimal` (unsupported by every gpt-5.6 model, so every small-budget
    /// request used to be rejected upstream for a reason the user could not
    /// see), and a large enough budget can reach `max`.
    #[test]
    fn the_budget_ladder_never_emits_minimal_and_can_reach_max() {
        let effort = |thinking: serde_json::Value| {
            ok(json!({
                "messages": [{"role": "user", "content": "x"}],
                "thinking": thinking
            }))
            .reasoning
            .unwrap()
            .effort
        };
        let enabled = |budget: i64| effort(json!({"type": "enabled", "budget_tokens": budget}));

        assert_eq!(effort(json!({"type": "disabled"})), "none");
        assert_eq!(enabled(0), "none");
        // Was `minimal`, which upstream rejects.
        assert_eq!(enabled(512), "low");
        assert_eq!(enabled(1024), "low");
        assert_eq!(enabled(8192), "medium");
        assert_eq!(enabled(24576), "high");
        assert_eq!(enabled(32768), "xhigh");
        // Was unreachable at any budget.
        assert_eq!(enabled(200_000), "max");

        for budget in [-1, 0, 512, 1024, 8192, 24576, 32768, 200_000, i64::MAX] {
            assert_ne!(enabled(budget), "minimal", "budget {budget}");
        }
    }

    /// A stated budget is a choice; a missing one is not. The harness sends
    /// `adaptive` with no budget for model ids it does not recognize -- which
    /// is every id the bridge serves -- so reading that as an explicit
    /// `medium` pinned every request to `medium` regardless of the model.
    #[test]
    fn an_absent_budget_defers_to_the_models_own_default() {
        let effort_for_model = |model: &str, thinking: serde_json::Value| {
            translate_bytes(
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "x"}],
                    "thinking": thinking,
                })
                .to_string()
                .as_bytes(),
                &options(),
            )
            .unwrap()
            .request
            .reasoning
            .unwrap()
            .effort
        };

        // sol's catalog default is `low`, terra's and luna's is `medium`.
        assert_eq!(effort_for_model("sol", json!({"type": "adaptive"})), "low");
        assert_eq!(
            effort_for_model("terra", json!({"type": "adaptive"})),
            "medium"
        );
        // A stated budget still wins over the model default.
        assert_eq!(
            effort_for_model("sol", json!({"type": "enabled", "budget_tokens": 30000})),
            "xhigh"
        );
    }

    /// `/effort` reaches the wire. Before this, `output_config` was not
    /// modelled at all and the field was dropped without a trace.
    #[test]
    fn output_config_effort_is_read_and_the_suffix_outranks_it() {
        let effort = |model: &str, config: serde_json::Value| {
            translate_bytes(
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "x"}],
                    "output_config": config,
                })
                .to_string()
                .as_bytes(),
                &options(),
            )
            .unwrap()
            .request
            .reasoning
            .unwrap()
            .effort
        };

        assert_eq!(effort("terra", json!({"effort": "xhigh"})), "xhigh");
        // The suffix is the channel that cannot be dropped in transit, so it
        // wins when both are present.
        assert_eq!(effort("terra@low", json!({"effort": "xhigh"})), "low");
    }

    /// An `/effort` value the gpt-5.6 family does not accept must be rejected
    /// at the bridge boundary, not silently downgraded (#821).
    ///
    /// `minimal` is the motivating case: it is a real Responses value, but
    /// gpt-5.6 supports only `none`/`low`/`medium`/`high`/`xhigh`/`max`. It
    /// used to fall through to terra's `medium` default, so a user who typed
    /// `/effort minimal` silently ran at a different effort than they chose.
    #[test]
    fn an_unsupported_output_config_effort_is_a_400_not_a_silent_downgrade() {
        let translate = |model: &str, config: serde_json::Value| {
            translate_bytes(
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "x"}],
                    "output_config": config,
                })
                .to_string()
                .as_bytes(),
                &options(),
            )
        };

        // Every effort gpt-5.6 accepts reaches the outgoing payload verbatim.
        for accepted in ["none", "low", "medium", "high", "xhigh", "max"] {
            let reasoning = translate("terra", json!({"effort": accepted}))
                .unwrap_or_else(|error| panic!("{accepted} must translate: {error}"))
                .request
                .reasoning
                .unwrap();
            assert_eq!(reasoning.effort, accepted);
        }

        for rejected in ["minimal", "wat", "ultra"] {
            let error = translate("terra", json!({"effort": rejected}))
                .expect_err("unsupported effort must not translate");
            let TranslateError::Invalid(message) = error;
            // Actionable: names what was wrong and what is accepted.
            assert!(message.contains(rejected), "{message}");
            assert!(message.contains("xhigh"), "{message}");
        }

        // An absent or empty effort is "unspecified", not an error: it must
        // still defer to the lower-precedence channels.
        assert_eq!(
            translate("terra", json!({}))
                .unwrap()
                .request
                .reasoning
                .unwrap()
                .effort,
            "medium"
        );
    }

    #[test]
    fn a_claude_id_resolves_to_the_default_and_anything_else_is_honoured() {
        let default = ModelSpec::parse("terra").unwrap();
        let resolved = |requested| resolve_selection(requested, &default).unwrap();

        // A real Codex id passes through verbatim -- this is how `/model`
        // pins a model through a gateway that never validates the string.
        assert_eq!(resolved(Some("gpt-5.6-luna")).model, "gpt-5.6-luna");
        // ... and the short name is expanded on the way.
        assert_eq!(resolved(Some("sol")).model, "gpt-6-sol");

        // The harness's own ids are meaningless upstream.
        for claude in ["claude-opus-4-8", "Claude-3", "   "] {
            assert_eq!(resolved(Some(claude)), default, "{claude}");
        }
        assert_eq!(resolved(None), default);
        assert_eq!(default_model_spec().model, "gpt-6-sol");
    }

    /// Issue #820: every model the picker can name has to reach the wire as
    /// itself. The row's `value` is exactly what `/model` sends back, so this
    /// is the other half of the picker contract — a label that resolved to
    /// some other model would be worse than no label at all.
    #[test]
    fn every_picker_reachable_model_translates_to_its_own_wire_model() {
        for model in codex_models() {
            let alias = model.legacy_aliases.first().copied().unwrap();
            for spelling in [alias, model.wire_id] {
                let out = translate_bytes(
                    json!({
                        "model": spelling,
                        "messages": [{"role": "user", "content": "x"}],
                    })
                    .to_string()
                    .as_bytes(),
                    &options(),
                )
                .unwrap_or_else(|error| panic!("{spelling} must translate: {error}"))
                .request;
                assert_eq!(
                    out.model, model.wire_id,
                    "{spelling} must bill {}",
                    model.wire_id
                );
                // ... at the model's own default effort, not a global one.
                assert_eq!(
                    out.reasoning.unwrap().effort,
                    model.default_effort.unwrap().as_str(),
                    "{spelling}"
                );
            }

            // The `@effort` a picker row was launched with survives the round
            // trip through the picker's own value.
            let pinned = format!("{}@max", model.wire_id);
            let out = translate_bytes(
                json!({
                    "model": pinned,
                    "messages": [{"role": "user", "content": "x"}],
                })
                .to_string()
                .as_bytes(),
                &options(),
            )
            .unwrap_or_else(|error| panic!("{pinned} must translate: {error}"))
            .request;
            assert_eq!(out.model, model.wire_id);
            assert_eq!(out.reasoning.unwrap().effort, "max");
        }
    }

    /// A typo must not be quietly billed as the default model.
    #[test]
    fn an_unparseable_selection_is_a_400_not_a_fallback() {
        let default = ModelSpec::parse("terra").unwrap();
        let error = resolve_selection(Some("tera"), &default).unwrap_err();
        let TranslateError::Invalid(message) = error;
        assert!(message.contains("tera"), "{message}");
        assert!(message.contains("terra"), "{message}");
    }

    #[test]
    fn cache_key_and_service_tier_are_forwarded_when_set() {
        let out = translate_bytes(
            json!({"messages": [{"role": "user", "content": "x"}]})
                .to_string()
                .as_bytes(),
            &TranslateOptions {
                prompt_cache_key: Some("session-1".into()),
                service_tier: Some("priority".into()),
                ..options()
            },
        )
        .unwrap()
        .request;
        assert_eq!(out.prompt_cache_key.as_deref(), Some("session-1"));
        assert_eq!(out.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn wire_shape_matches_the_reference_request() {
        let out = ok(json!({"messages": [{"role": "user", "content": "hi"}]}));
        let default_model = default_model_spec().model;
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({
                "model": default_model,
                "stream": true,
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "parallel_tool_calls": true,
                "reasoning": {"effort": "low"},
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}]
                }]
            })
        );
    }

    #[test]
    fn stream_intent_is_read_from_the_client_request() {
        assert!(wants_stream(br#"{"stream":true}"#));
        assert!(!wants_stream(br#"{"stream":false}"#));
        assert!(!wants_stream(b"{}"));
        assert!(!wants_stream(b"garbage"));
    }

    #[test]
    fn pending_input_keeps_every_result_from_a_split_parallel_batch() {
        for count in [2_usize, 7, 8] {
            let calls = (0..count)
                .map(|index| {
                    json!({
                        "type": "tool_use",
                        "id": format!("call_{index}"),
                        "name": "Bash",
                        "input": {"command": format!("probe-{index}")},
                    })
                })
                .collect::<Vec<_>>();
            let mut messages = vec![
                json!({"role": "user", "content": "inspect"}),
                json!({"role": "assistant", "content": calls}),
            ];
            messages.extend((0..count).map(|index| {
                json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": format!("call_{index}"),
                        "content": format!("result-{index}"),
                    }],
                })
            }));
            let body = json!({"messages": messages}).to_string();

            let pending = pending_input_items_as_values(body.as_bytes()).unwrap();
            assert_eq!(pending.len(), count, "parallel batch size {count}");
            for (index, item) in pending.iter().enumerate() {
                assert_eq!(item["type"], "function_call_output");
                assert_eq!(item["call_id"], format!("call_{index}"));
                assert_eq!(item["output"], format!("result-{index}"));
            }
        }
    }

    #[test]
    fn pending_input_keeps_multiple_results_in_one_message() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "inspect"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_0", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_0", "content": "zero"},
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "one"},
                ]},
            ]
        })
        .to_string();

        let pending = pending_input_items_as_values(body.as_bytes()).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0]["call_id"], "call_0");
        assert_eq!(pending[1]["call_id"], "call_1");
    }

    #[test]
    fn pending_input_without_an_assistant_keeps_the_final_message_fallback() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "replayed"},
                {"role": "user", "content": "pending"},
            ]
        })
        .to_string();

        let pending = pending_input_items_as_values(body.as_bytes()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["content"][0]["text"], "pending");
    }

    #[test]
    fn pending_input_keeps_a_final_assistant_prefill() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "Complete this sentence"},
                {"role": "assistant", "content": "The answer is"},
            ]
        })
        .to_string();

        let pending = pending_input_items_as_values(body.as_bytes()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["role"], "assistant");
        assert_eq!(pending[0]["content"][0]["text"], "The answer is");
    }

    #[test]
    fn compaction_uses_history_and_retries_with_compaction_then_pending_user_input() {
        let mut request = ok(json!({
            "system": "shared instructions",
            "messages": [
                {"role": "user", "content": "earlier"},
                {"role": "assistant", "content": "earlier answer"},
                {"role": "user", "content": "pending"}
            ],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"}
        }));
        request.service_tier = Some("priority".into());
        request.prompt_cache_key = Some("session-1".into());
        let compact = request
            .compact_request()
            .expect("history and pending input");
        let body = serde_json::to_value(&compact).unwrap();

        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert_eq!(body["input"][0]["content"][0]["text"], "earlier");
        assert_eq!(body["instructions"], "shared instructions");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["prompt_cache_key"], "session-1");
        assert_eq!(body["text"], json!({}));
        for generation_only in ["stream", "store", "include", "tool_choice"] {
            assert!(
                body.get(generation_only).is_none(),
                "{generation_only} must not compact"
            );
        }

        let retry = request
            .retry_with_compaction(CompactResponse {
                output: vec![InputItem::Compaction {
                    encrypted_content: "summary".into(),
                    extra: serde_json::Map::new(),
                }],
            })
            .expect("compaction output is retry input");
        assert_eq!(retry.input.len(), 2);
        assert!(
            matches!(retry.input[0], InputItem::Compaction { ref encrypted_content, .. } if encrypted_content == "summary")
        );
        assert!(
            matches!(&retry.input[1], InputItem::Message { role, content }
            if role == "user" && matches!(&content[..], [ContentPart::InputText { text }] if text == "pending"))
        );
    }

    #[test]
    fn compaction_preserves_a_pending_tool_result_continuation() {
        let request = ok(json!({
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "weather", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "18C"}]}
            ]
        }));
        let compact = request
            .compact_request()
            .expect("history and pending tool result");
        assert_eq!(compact.input.len(), 2);
        let retry = request
            .retry_with_compaction(CompactResponse {
                output: vec![InputItem::Compaction {
                    encrypted_content: "summary".into(),
                    extra: serde_json::Map::new(),
                }],
            })
            .expect("compaction output is retry input");
        assert!(
            matches!(&retry.input[1], InputItem::FunctionCallOutput { call_id, output }
            if call_id == "call_1" && matches!(output, FunctionCallOutput::Text(text) if text == "18C"))
        );
    }

    #[test]
    fn invalid_compaction_output_cannot_replace_history() {
        let request = ok(json!({
            "messages": [
                {"role": "user", "content": "earlier"},
                {"role": "user", "content": "pending"}
            ]
        }));
        for output in [
            vec![],
            vec![InputItem::Message {
                role: "assistant".into(),
                content: vec![ContentPart::OutputText {
                    text: "not a compaction".into(),
                }],
            }],
            vec![InputItem::Compaction {
                encrypted_content: String::new(),
                extra: serde_json::Map::new(),
            }],
        ] {
            assert!(request
                .retry_with_compaction(CompactResponse { output })
                .is_err());
        }
        let malformed: Result<CompactResponse, _> =
            serde_json::from_str(r#"{"output":[{"type":"compaction"}]}"#);
        assert!(
            malformed.is_err(),
            "missing encrypted_content must not deserialize"
        );
    }
}

#[cfg(test)]
#[path = "tool_result_cap_translate_tests.rs"]
mod tool_result_cap_translate_tests;

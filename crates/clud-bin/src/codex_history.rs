//! Canonical, in-memory Responses transcripts for the Claude-to-Codex bridge.
//!
//! A Messages request is a replayable view, not an authoritative Responses
//! transcript: it loses server-generated item ids and reasoning
//! `encrypted_content`.  This store consequently accepts already-translated
//! input plus verbatim upstream output items, and never rebuilds either from
//! display text.  One [`ConversationStore::with_history`] closure owns a whole
//! turn so another request for the same conversation cannot observe a partial
//! compaction replacement.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Deterministic identity for a Messages client that provides no conversation
/// identity. A bridge is owned by one foreground harness session, so this
/// fallback is stable for that bridge's lifetime and is evicted at shutdown.
pub const BRIDGE_SESSION_CONVERSATION: &str = "bridge-session";

/// Provider-private transcript ownership inside a unified gateway session.
///
/// Claude and DeepSeek receive the caller's complete Anthropic transcript on
/// every request. Codex additionally retains opaque Responses items, so that
/// canonical history is valid only while consecutive requests stay on the
/// Codex route. Crossing any provider boundary starts a new route epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRoute {
    Claude,
    Codex,
    DeepSeek,
    Kimi,
    OpenRouter,
}

impl ConversationRoute {
    /// Stable, non-sensitive name for logs, notices, and `clud route status`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::DeepSeek => "deepseek",
            Self::Kimi => "kimi",
            Self::OpenRouter => "openrouter",
        }
    }
}

/// A bounded, non-sensitive key for one Claude session and (when present) one
/// sub-agent. Raw Claude identifiers never enter the history map or logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationKey {
    pub id: String,
    pub session_prefix: String,
}

impl ConversationKey {
    pub fn from_headers(session_id: Option<&str>, agent_id: Option<&str>) -> Self {
        match session_id {
            Some(session_id) if !session_id.is_empty() => {
                let prefix = format!("session-{}-", digest(session_id));
                let id = match agent_id.filter(|agent| !agent.is_empty()) {
                    Some(agent_id) => format!("{prefix}agent-{}", digest(agent_id)),
                    None => format!("{prefix}main"),
                };
                Self {
                    id,
                    session_prefix: prefix,
                }
            }
            _ => match agent_id.filter(|agent| !agent.is_empty()) {
                Some(agent_id) => Self {
                    id: format!("{BRIDGE_SESSION_CONVERSATION}-agent-{}", digest(agent_id)),
                    session_prefix: format!("{BRIDGE_SESSION_CONVERSATION}-"),
                },
                None => Self {
                    id: BRIDGE_SESSION_CONVERSATION.to_string(),
                    session_prefix: BRIDGE_SESSION_CONVERSATION.to_string(),
                },
            },
        }
    }

    /// Derive a headerless conversation beneath this bridge's random fallback
    /// root. The root is generated once per listener, so it remains stable for
    /// an ordinary headerless turn while never sharing an upstream cache key
    /// with another bridge launch.
    pub fn bridge_fallback(random_root: &str, agent_id: Option<&str>) -> Self {
        let prefix = format!("bridge-{random_root}-");
        let id = match agent_id.filter(|agent| !agent.is_empty()) {
            Some(agent_id) => format!("{prefix}agent-{}", digest(agent_id)),
            None => format!("{prefix}main"),
        };
        Self {
            id,
            session_prefix: prefix,
        }
    }

    /// Fixed, non-sensitive scope label for forensic diagnostics.
    pub fn scope(&self) -> &'static str {
        if self.id == BRIDGE_SESSION_CONVERSATION || self.id.starts_with("bridge-") {
            "fallback"
        } else if self.id.contains("-agent-") {
            "agent"
        } else {
            "main"
        }
    }
}

fn digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

const DEFAULT_MAX_CONVERSATIONS: usize = 32;
const DEFAULT_MAX_ITEMS_PER_CONVERSATION: usize = 16_384;
const DEFAULT_MAX_BYTES_PER_CONVERSATION: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLimits {
    pub max_conversations: usize,
    pub max_items_per_conversation: usize,
    pub max_bytes_per_conversation: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_conversations: DEFAULT_MAX_CONVERSATIONS,
            max_items_per_conversation: DEFAULT_MAX_ITEMS_PER_CONVERSATION,
            max_bytes_per_conversation: DEFAULT_MAX_BYTES_PER_CONVERSATION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    #[error("invalid bridge conversation id")]
    InvalidConversationId,
    #[error("bridge conversation capacity reached")]
    ConversationLimit,
    #[error("Responses history item exceeds capacity")]
    ItemTooLarge,
    #[error("Responses history capacity reached")]
    HistoryLimit,
}

/// Thread-safe collection of per-conversation transcripts.
///
/// There is intentionally no long-lived global singleton. The bridge owns this
/// value and calls [`Self::clear`] as its listener stops, making the lifetime
/// both bounded and explicit.
#[derive(Debug, Clone)]
pub struct ConversationStore {
    limits: HistoryLimits,
    conversations: Arc<Mutex<HashMap<String, Arc<Mutex<ConversationHistory>>>>>,
}

impl Default for ConversationStore {
    fn default() -> Self {
        Self::new(HistoryLimits::default())
    }
}

impl ConversationStore {
    pub fn new(limits: HistoryLimits) -> Self {
        Self {
            limits,
            conversations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Run one indivisible conversation operation.
    ///
    /// The closure intentionally executes while the per-conversation mutex is
    /// held. Network callers use it for the entire request/replace/append
    /// lifecycle, which prevents concurrent turns from interleaving their
    /// input/output pairs or observing half of a compaction replacement.
    pub fn with_history<T>(
        &self,
        conversation_id: &str,
        operation: impl FnOnce(&mut ConversationHistory) -> Result<T, HistoryError>,
    ) -> Result<T, HistoryError> {
        validate_conversation_id(conversation_id)?;
        let conversation = {
            let mut conversations = lock(&self.conversations);
            if let Some(conversation) = conversations.get(conversation_id) {
                Arc::clone(conversation)
            } else {
                if conversations.len() >= self.limits.max_conversations {
                    return Err(HistoryError::ConversationLimit);
                }
                let conversation = Arc::new(Mutex::new(ConversationHistory::new(self.limits)));
                conversations.insert(conversation_id.to_string(), Arc::clone(&conversation));
                conversation
            }
        };
        let mut history = lock(&conversation);
        operation(&mut history)
    }

    /// Evict all history at the bridge session boundary.
    pub fn clear(&self) {
        lock(&self.conversations).clear();
    }

    /// Evict every child key belonging to one Claude session.
    pub fn clear_session(&self, session_prefix: &str) {
        lock(&self.conversations).retain(|key, _| !key.starts_with(session_prefix));
    }

    #[cfg(test)]
    fn conversation_count(&self) -> usize {
        lock(&self.conversations).len()
    }
}

/// One ordered canonical Responses transcript.
#[derive(Debug)]
pub struct ConversationHistory {
    limits: HistoryLimits,
    items: Vec<StoredItem>,
    bytes: usize,
    route: Option<ConversationRoute>,
    reset_after_harness_compaction: bool,
}

#[derive(Debug)]
struct StoredItem {
    value: Value,
    bytes: usize,
}

impl ConversationHistory {
    fn new(limits: HistoryLimits) -> Self {
        Self {
            limits,
            items: Vec::new(),
            bytes: 0,
            route: None,
            reset_after_harness_compaction: false,
        }
    }

    /// Enter one provider route while the conversation lock is held.
    ///
    /// A provider change invalidates every provider-private item from the
    /// previous epoch. The next Codex request then reseeds from the complete
    /// Anthropic-visible transcript supplied by Claude Code.
    pub fn enter_route(&mut self, route: ConversationRoute) -> bool {
        let changed = self.route.is_some_and(|previous| previous != route);
        if changed {
            self.clear();
        }
        self.route = Some(route);
        changed
    }

    /// Clear this transcript while its conversation mutex remains held.
    ///
    /// A client-visible turn that cannot fit calls this after its reply has
    /// committed. The next replay then seeds fresh history instead of extending
    /// stale canonical state.
    pub fn clear(&mut self) {
        self.clear_items();
        self.reset_after_harness_compaction = false;
    }

    /// Fall back from provider-side compaction to Claude's own compaction.
    ///
    /// Claude's compaction inference may replay the old Messages transcript
    /// and temporarily extend or repopulate this cache. The matching
    /// `SessionStart(compact)` control must clear it so the next
    /// ordinary turn seeds from Claude's compacted transcript instead.
    /// Valid history stays intact for the summary inference's cached prefix;
    /// callers discard it immediately only when canonical validation fails.
    pub fn begin_harness_compaction_fallback(&mut self, invalid_history: bool) {
        if invalid_history {
            self.clear_items();
        }
        self.reset_after_harness_compaction = true;
    }

    /// Complete a pending harness-compaction fallback.
    ///
    /// Returns whether a reset was pending, allowing the HTTP boundary to log
    /// only real state transitions.
    pub fn finish_harness_compaction_fallback(&mut self) -> bool {
        if !self.reset_after_harness_compaction {
            return false;
        }
        self.clear_items();
        self.reset_after_harness_compaction = false;
        true
    }

    fn clear_items(&mut self) {
        self.items.clear();
        self.bytes = 0;
    }

    /// Copy the complete canonical transcript. Values are opaque Responses
    /// items; callers must not deserialize and reconstruct them.
    pub fn snapshot(&self) -> Vec<Value> {
        self.items.iter().map(|item| item.value.clone()).collect()
    }

    /// Append a successful turn atomically, input before output.
    ///
    /// The caller must invoke this only after upstream completed successfully.
    /// A failed attempt therefore contributes neither speculative input nor
    /// partial output to the authoritative transcript.
    pub fn append_successful_turn(
        &mut self,
        input: &[Value],
        output: &[Value],
    ) -> Result<(), HistoryError> {
        let mut turn = Vec::with_capacity(input.len() + output.len());
        turn.extend(input.iter().cloned());
        turn.extend(output.iter().cloned());
        self.append_items(turn)
    }

    /// Atomically replace all old history with upstream's compact output.
    pub fn replace_history(&mut self, compact_output: &[Value]) -> Result<(), HistoryError> {
        let replacement = self.encode_replacement(compact_output.iter().cloned())?;
        self.bytes = replacement.iter().map(|item| item.bytes).sum();
        self.items = replacement;
        Ok(())
    }

    /// Install a provider-side lifecycle compaction and cancel any stale
    /// harness-fallback reset left by a missed `SessionStart(compact)` hook.
    pub fn install_provider_compaction(
        &mut self,
        compact_output: &[Value],
    ) -> Result<(), HistoryError> {
        self.replace_history(compact_output)?;
        self.reset_after_harness_compaction = false;
        Ok(())
    }

    fn append_items(&mut self, values: Vec<Value>) -> Result<(), HistoryError> {
        let encoded = self.encode_items(values)?;
        self.bytes += encoded.iter().map(|item| item.bytes).sum::<usize>();
        self.items.extend(encoded);
        Ok(())
    }

    fn encode_items(
        &self,
        values: impl IntoIterator<Item = Value>,
    ) -> Result<Vec<StoredItem>, HistoryError> {
        let encoded = encode(values)?;
        let item_count = self.items.len() + encoded.len();
        let total_bytes = self.bytes + encoded.iter().map(|(_, bytes)| bytes).sum::<usize>();
        validate_encoded(&encoded, self.limits)?;
        if item_count > self.limits.max_items_per_conversation
            || total_bytes > self.limits.max_bytes_per_conversation
        {
            return Err(HistoryError::HistoryLimit);
        }
        Ok(to_stored(encoded))
    }

    fn encode_replacement(
        &self,
        values: impl IntoIterator<Item = Value>,
    ) -> Result<Vec<StoredItem>, HistoryError> {
        let encoded = encode(values)?;
        validate_encoded(&encoded, self.limits)?;
        if encoded.len() > self.limits.max_items_per_conversation
            || encoded.iter().map(|(_, bytes)| bytes).sum::<usize>()
                > self.limits.max_bytes_per_conversation
        {
            return Err(HistoryError::HistoryLimit);
        }
        Ok(to_stored(encoded))
    }
}

fn encode(values: impl IntoIterator<Item = Value>) -> Result<Vec<(Value, usize)>, HistoryError> {
    values
        .into_iter()
        .map(|value| {
            let bytes = serde_json::to_vec(&value)
                .map(|encoded| encoded.len())
                .map_err(|_| HistoryError::ItemTooLarge)?;
            Ok((value, bytes))
        })
        .collect()
}

fn validate_encoded(encoded: &[(Value, usize)], limits: HistoryLimits) -> Result<(), HistoryError> {
    if encoded
        .iter()
        .any(|(_, bytes)| *bytes > limits.max_bytes_per_conversation)
    {
        return Err(HistoryError::ItemTooLarge);
    }
    Ok(())
}

fn to_stored(encoded: Vec<(Value, usize)>) -> Vec<StoredItem> {
    encoded
        .into_iter()
        .map(|(value, bytes)| StoredItem { value, bytes })
        .collect()
}

fn validate_conversation_id(conversation_id: &str) -> Result<(), HistoryError> {
    (!conversation_id.is_empty()
        && conversation_id.len() <= 128
        && conversation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    .then_some(())
    .ok_or(HistoryError::InvalidConversationId)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn item(kind: &str, fields: serde_json::Value) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("type".into(), Value::String(kind.into()));
        object.extend(fields.as_object().unwrap().clone());
        Value::Object(object)
    }

    #[test]
    fn claude_identity_keys_isolate_agents_without_retaining_raw_ids() {
        let main = ConversationKey::from_headers(Some("session-raw-123456"), None);
        let child =
            ConversationKey::from_headers(Some("session-raw-123456"), Some("agent-raw-abcdef"));
        let sibling =
            ConversationKey::from_headers(Some("session-raw-123456"), Some("agent-raw-123456"));
        let other_session =
            ConversationKey::from_headers(Some("session-other-654321"), Some("agent-raw-abcdef"));
        assert_ne!(main.id, child.id);
        assert_ne!(child.id, sibling.id);
        assert_ne!(child.id, other_session.id);
        assert_eq!(main.scope(), "main");
        assert_eq!(child.scope(), "agent");
        assert_eq!(sibling.scope(), "agent");
        assert_eq!(
            ConversationKey::from_headers(None, None).scope(),
            "fallback"
        );
        for raw in ["session-raw-123456", "agent-raw-abcdef", "agent-raw-123456"] {
            assert!(!child.id.contains(raw));
        }
        assert!(child.id.len() <= 128);
    }

    #[test]
    fn clearing_a_session_evicts_all_agent_descendants_only() {
        let store = ConversationStore::default();
        let session_a = ConversationKey::from_headers(Some("session-a"), None);
        let child_a = ConversationKey::from_headers(Some("session-a"), Some("agent-a"));
        let child_b = ConversationKey::from_headers(Some("session-b"), Some("agent-b"));
        for key in [&session_a, &child_a, &child_b] {
            store
                .with_history(&key.id, |history| {
                    history.append_successful_turn(&[item("message", serde_json::json!({}))], &[])
                })
                .unwrap();
        }
        store.clear_session(&session_a.session_prefix);
        assert_eq!(store.conversation_count(), 1);
        assert!(
            store
                .with_history(&child_b.id, |history| Ok(history.snapshot().len()))
                .unwrap()
                > 0
        );
        assert_eq!(
            store
                .with_history(&child_a.id, |history| Ok(history.snapshot().len()))
                .unwrap(),
            0
        );
    }

    #[test]
    fn sequential_turns_retain_verbatim_responses_items_in_order() {
        let store = ConversationStore::default();
        let encrypted = "gAAAAABopaque-reasoning";
        let first_input = item(
            "message",
            serde_json::json!({
                "role": "user", "content": [{"type": "input_text", "text": "weather?"}]
            }),
        );
        let assistant = item(
            "message",
            serde_json::json!({
                "id": "msg_server_1", "role": "assistant",
                "content": [{"type": "output_text", "text": "checking"}]
            }),
        );
        let call = item(
            "function_call",
            serde_json::json!({
                "id": "fc_server_1", "call_id": "call_1", "name": "weather", "arguments": "{}"
            }),
        );
        let tool_output = item(
            "function_call_output",
            serde_json::json!({
                "call_id": "call_1", "output": "18C"
            }),
        );
        let reasoning = item(
            "reasoning",
            serde_json::json!({
                "id": "rs_server_1", "encrypted_content": encrypted, "summary": []
            }),
        );

        store
            .with_history(BRIDGE_SESSION_CONVERSATION, |history| {
                history.append_successful_turn(
                    std::slice::from_ref(&first_input),
                    &[assistant.clone(), call.clone()],
                )?;
                history.append_successful_turn(
                    std::slice::from_ref(&tool_output),
                    std::slice::from_ref(&reasoning),
                )
            })
            .unwrap();

        let history = store
            .with_history(
                BRIDGE_SESSION_CONVERSATION,
                |history| Ok(history.snapshot()),
            )
            .unwrap();
        assert_eq!(
            history,
            vec![first_input, assistant, call, tool_output, reasoning]
        );
        assert_eq!(history[4]["encrypted_content"], encrypted);
        assert_eq!(history[1]["id"], "msg_server_1");
    }

    #[test]
    fn provider_boundary_starts_a_fresh_route_epoch() {
        let store = ConversationStore::default();
        let key = ConversationKey::from_headers(Some("session-route"), None);
        store
            .with_history(&key.id, |history| {
                history.enter_route(ConversationRoute::Codex);
                history.append_successful_turn(
                    &[item("message", serde_json::json!({"text": "input"}))],
                    &[item(
                        "reasoning",
                        serde_json::json!({"encrypted_content": "opaque"}),
                    )],
                )
            })
            .unwrap();
        store
            .with_history(&key.id, |history| {
                history.enter_route(ConversationRoute::Claude);
                assert!(history.snapshot().is_empty());
                history.enter_route(ConversationRoute::DeepSeek);
                assert!(history.snapshot().is_empty());
                history.enter_route(ConversationRoute::Codex);
                assert!(history.snapshot().is_empty());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn compaction_replacement_is_atomic_before_pending_input_is_appended() {
        let store = ConversationStore::default();
        let old = item("message", serde_json::json!({"id": "old"}));
        let compact = item(
            "compaction",
            serde_json::json!({
                "id": "cmp_1", "encrypted_content": "opaque-summary"
            }),
        );
        let pending = item(
            "function_call_output",
            serde_json::json!({
                "call_id": "call_1", "output": "result"
            }),
        );
        store
            .with_history(BRIDGE_SESSION_CONVERSATION, |history| {
                history.append_successful_turn(std::slice::from_ref(&old), &[])?;
                history.replace_history(std::slice::from_ref(&compact))?;
                history.append_successful_turn(std::slice::from_ref(&pending), &[])
            })
            .unwrap();
        assert_eq!(
            store
                .with_history(
                    BRIDGE_SESSION_CONVERSATION,
                    |history| Ok(history.snapshot())
                )
                .unwrap(),
            vec![compact, pending]
        );
    }

    #[test]
    fn harness_compaction_fallback_clears_the_temporary_replay_once() {
        let mut history = ConversationHistory::new(HistoryLimits::default());
        history
            .append_successful_turn(&[item("message", serde_json::json!({"id": "old"}))], &[])
            .unwrap();
        history.begin_harness_compaction_fallback(true);
        assert!(history.snapshot().is_empty());
        history
            .append_successful_turn(
                &[item(
                    "message",
                    serde_json::json!({"id": "temporary-replay"}),
                )],
                &[],
            )
            .unwrap();

        assert!(history.finish_harness_compaction_fallback());
        assert!(history.snapshot().is_empty());
        assert!(!history.finish_harness_compaction_fallback());
    }

    #[test]
    fn harness_compaction_fallback_defers_valid_history_clear_until_finished() {
        let mut history = ConversationHistory::new(HistoryLimits::default());
        let prior = item("message", serde_json::json!({"id": "canonical"}));
        history
            .append_successful_turn(std::slice::from_ref(&prior), &[])
            .unwrap();
        history.begin_harness_compaction_fallback(false);
        assert_eq!(history.snapshot(), vec![prior]);
        assert!(history.finish_harness_compaction_fallback());
        assert!(history.snapshot().is_empty());
        assert!(!history.finish_harness_compaction_fallback());
    }

    #[test]
    fn provider_compaction_cancels_a_stale_harness_fallback_reset() {
        let mut history = ConversationHistory::new(HistoryLimits::default());
        history.begin_harness_compaction_fallback(true);
        history
            .append_successful_turn(
                &[item(
                    "message",
                    serde_json::json!({"id": "temporary-replay"}),
                )],
                &[],
            )
            .unwrap();
        let compact = item(
            "compaction",
            serde_json::json!({"encrypted_content": "opaque"}),
        );

        history
            .install_provider_compaction(std::slice::from_ref(&compact))
            .unwrap();
        assert!(!history.finish_harness_compaction_fallback());
        assert_eq!(history.snapshot(), vec![compact]);
    }

    #[test]
    fn concurrent_operations_cannot_observe_half_replaced_history() {
        let store = Arc::new(ConversationStore::default());
        let barrier = Arc::new(Barrier::new(2));
        let old = item("message", serde_json::json!({"id": "old"}));
        store
            .with_history(BRIDGE_SESSION_CONVERSATION, |history| {
                history.append_successful_turn(&[old], &[])
            })
            .unwrap();

        let replacing_store = Arc::clone(&store);
        let replacing_barrier = Arc::clone(&barrier);
        let replace = thread::spawn(move || {
            replacing_store
                .with_history(BRIDGE_SESSION_CONVERSATION, |history| {
                    history.replace_history(&[item(
                        "compaction",
                        serde_json::json!({"id": "compact"}),
                    )])?;
                    replacing_barrier.wait();
                    history.append_successful_turn(
                        &[item("message", serde_json::json!({"id": "pending"}))],
                        &[],
                    )
                })
                .unwrap();
        });
        let reading_store = Arc::clone(&store);
        let reading = thread::spawn(move || {
            barrier.wait();
            reading_store
                .with_history(
                    BRIDGE_SESSION_CONVERSATION,
                    |history| Ok(history.snapshot()),
                )
                .unwrap()
        });
        replace.join().unwrap();
        let observed = reading.join().unwrap();
        assert_eq!(
            observed,
            vec![
                item("compaction", serde_json::json!({"id": "compact"})),
                item("message", serde_json::json!({"id": "pending"})),
            ]
        );
    }

    #[test]
    fn failed_attempt_is_not_recorded_and_session_end_evicts_history() {
        let store = ConversationStore::default();
        // Failed attempts deliberately do not call append_successful_turn.
        store
            .with_history(
                BRIDGE_SESSION_CONVERSATION,
                |history| Ok(history.snapshot()),
            )
            .unwrap();
        assert_eq!(store.conversation_count(), 1);
        store.clear();
        assert_eq!(store.conversation_count(), 0);
    }

    #[test]
    fn capacity_rejection_can_clear_a_conversation_before_the_next_replay() {
        let store = ConversationStore::new(HistoryLimits {
            max_conversations: 1,
            max_items_per_conversation: 1,
            max_bytes_per_conversation: 1024,
        });
        store
            .with_history(BRIDGE_SESSION_CONVERSATION, |history| {
                history.append_successful_turn(&[item("message", serde_json::json!({}))], &[])
            })
            .unwrap();
        store
            .with_history(BRIDGE_SESSION_CONVERSATION, |history| {
                assert_eq!(
                    history
                        .append_successful_turn(
                            &[item("message", serde_json::json!({"id": "too-many"}))],
                            &[]
                        )
                        .unwrap_err(),
                    HistoryError::HistoryLimit
                );
                history.clear();
                Ok(())
            })
            .unwrap();
        assert!(store
            .with_history(
                BRIDGE_SESSION_CONVERSATION,
                |history| Ok(history.snapshot())
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn conversation_and_history_limits_are_explicit() {
        let store = ConversationStore::new(HistoryLimits {
            max_conversations: 1,
            max_items_per_conversation: 1,
            max_bytes_per_conversation: 1024,
        });
        store
            .with_history("one", |history| {
                history.append_successful_turn(&[item("message", serde_json::json!({}))], &[])
            })
            .unwrap();
        assert_eq!(
            store.with_history("two", |_| Ok(())).unwrap_err(),
            HistoryError::ConversationLimit
        );
        assert_eq!(
            store
                .with_history("one", |history| {
                    history.append_successful_turn(
                        &[item("message", serde_json::json!({"id": "two"}))],
                        &[],
                    )
                })
                .unwrap_err(),
            HistoryError::HistoryLimit
        );
    }
}

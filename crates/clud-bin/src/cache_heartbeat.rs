//! Issue #2046: keep the provider prefix cache warm across idle gaps.
//!
//! DeepSeek, OpenRouter and Kimi cache a request's prefix for about five
//! minutes, refreshed only by use. A `/loop` iteration that waits longer
//! (a build, a test run) re-enters cold: full re-read, slow first token,
//! cache-miss pricing. The unified gateway sees every request, so it keeps
//! the cache warm itself, out of band:
//!
//! - After each successful compat-route `/v1/messages` call it records the
//!   request body per conversation ([`Heartbeats::finish`]).
//! - A background thread sends a heartbeat when a conversation has been idle
//!   for `ttl - 15 s` with nothing in flight ([`due`]). The heartbeat replays
//!   the recorded prefix unchanged (system, tools, messages) and appends one
//!   user turn, `just respond with ok`, with a tiny `max_tokens`, no
//!   streaming and no thinking ([`heartbeat_body`]). Its reply is discarded.
//! - It never enters the conversation: Claude Code, the loop, its markers
//!   and its counters never see it, so the next real turn resumes exactly
//!   where it left off. A real request marks the conversation in flight
//!   ([`Heartbeats::begin`]), which stops new heartbeats until it finishes
//!   and restarts the idle clock; time in flight never counts as idle.
//! - At most [`MAX_BEATS_PER_IDLE`] heartbeats per idle stretch, so an
//!   abandoned session stops paying after about half an hour.
//! - A failed or timed-out heartbeat is a lost cache opportunity, never an
//!   error: it is logged and dropped.
//!
//! Enabled by default. `NO_CACHE_HEARTBEAT` set to a truthy value disables
//! it; `CLUD_CACHE_TTL_SECS` overrides the 300 s default TTL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Kill switch: truthy disables the heartbeat.
pub const ENV_DISABLE: &str = "NO_CACHE_HEARTBEAT";
/// TTL override in seconds.
pub const ENV_TTL_SECS: &str = "CLUD_CACHE_TTL_SECS";
/// Default provider cache TTL.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);
/// Fire this long before the TTL runs out.
pub const MARGIN: Duration = Duration::from_secs(15);
/// Heartbeats per idle stretch before giving up on an abandoned session.
pub const MAX_BEATS_PER_IDLE: u32 = 6;
/// The appended turn.
pub const HEARTBEAT_PROMPT: &str = "just respond with ok";
/// Reply budget for a heartbeat.
const HEARTBEAT_MAX_TOKENS: u64 = 8;
/// A heartbeat that takes longer than this is abandoned.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether the heartbeat is on, from the environment.
pub fn enabled() -> bool {
    !std::env::var(ENV_DISABLE).is_ok_and(|v| crate::process_scan::is_truthy_marker(&v))
}

/// Provider TTL from the environment, else [`DEFAULT_TTL`]. A value at or
/// under [`MARGIN`] is ignored.
pub fn ttl() -> Duration {
    std::env::var(ENV_TTL_SECS)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .filter(|ttl| *ttl > MARGIN)
        .unwrap_or(DEFAULT_TTL)
}

/// Pure decision: should a heartbeat go out now?
pub fn due(idle_for: Duration, in_flight: bool, beats_sent: u32, ttl: Duration) -> bool {
    !in_flight && beats_sent < MAX_BEATS_PER_IDLE && idle_for >= ttl.saturating_sub(MARGIN)
}

/// The heartbeat request: the recorded body with one appended user turn.
/// Everything that forms the cached prefix is left byte-for-byte as sent;
/// only the reply shape changes. `None` when the body is not a messages
/// request.
pub fn heartbeat_body(recorded: &[u8]) -> Option<Vec<u8>> {
    let mut request: serde_json::Value = serde_json::from_slice(recorded).ok()?;
    let object = request.as_object_mut()?;
    object
        .get_mut("messages")?
        .as_array_mut()?
        .push(serde_json::json!({"role": "user", "content": HEARTBEAT_PROMPT}));
    // #2553: keep `thinking` as recorded. Providers cache the message prefix
    // under the thinking parameters, so removing them warmed an entry the
    // next real turn never read. `max_tokens` must exceed the thinking
    // budget for the request to be valid; a one-word reply spends little of it.
    let budget = object
        .get("thinking")
        .and_then(|thinking| thinking.get("budget_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    object.insert(
        "max_tokens".into(),
        budget.saturating_add(HEARTBEAT_MAX_TOKENS).into(),
    );
    object.insert("stream".into(), false.into());
    serde_json::to_vec(&request).ok()
}

/// Where and how to replay one conversation's prefix.
#[derive(Clone)]
pub struct Target {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

struct Slot {
    target: Option<Target>,
    in_flight: u32,
    last_activity: Instant,
    beats_sent: u32,
}

/// Per-conversation heartbeat state, shared by the gateway's request
/// threads and the heartbeat thread.
#[derive(Default)]
pub struct Heartbeats {
    slots: Mutex<HashMap<String, Slot>>,
}

impl Heartbeats {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A real request for `conversation` started.
    pub fn begin(&self, conversation: &str, now: Instant) {
        let mut slots = self.lock();
        let slot = slots.entry(conversation.to_string()).or_insert(Slot {
            target: None,
            in_flight: 0,
            last_activity: now,
            beats_sent: 0,
        });
        slot.in_flight += 1;
        slot.last_activity = now;
        slot.beats_sent = 0;
    }

    /// A real request finished. `target` is `Some` for a success whose
    /// prefix is worth keeping warm.
    pub fn finish(&self, conversation: &str, target: Option<Target>, now: Instant) {
        let mut slots = self.lock();
        if let Some(slot) = slots.get_mut(conversation) {
            slot.in_flight = slot.in_flight.saturating_sub(1);
            slot.last_activity = now;
            slot.beats_sent = 0;
            if target.is_some() {
                slot.target = target;
            }
        }
    }

    /// Conversations due a heartbeat now; each is counted as sent.
    pub fn take_due(&self, now: Instant, ttl: Duration) -> Vec<Target> {
        let mut slots = self.lock();
        let mut out = Vec::new();
        for slot in slots.values_mut() {
            let idle = now.saturating_duration_since(slot.last_activity);
            if slot.target.is_some() && due(idle, slot.in_flight > 0, slot.beats_sent, ttl) {
                slot.beats_sent += 1;
                // The next beat is measured from this one.
                slot.last_activity = now;
                out.extend(slot.target.clone());
            }
        }
        out
    }
}

/// The process-wide registry, with its heartbeat thread started on first
/// use. `None` when disabled.
pub fn shared() -> Option<&'static Arc<Heartbeats>> {
    static SHARED: OnceLock<Option<Arc<Heartbeats>>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            if !enabled() {
                return None;
            }
            let beats = Arc::new(Heartbeats::default());
            let worker = Arc::clone(&beats);
            let ttl = ttl();
            std::thread::Builder::new()
                .name("clud-cache-heartbeat".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(1));
                    for target in worker.take_due(Instant::now(), ttl) {
                        send(&target);
                    }
                })
                .ok()?;
            Some(beats)
        })
        .as_ref()
}

fn send(target: &Target) {
    let Some(body) = heartbeat_body(&target.body) else {
        return;
    };
    let agent = ureq::AgentBuilder::new().timeout(HEARTBEAT_TIMEOUT).build();
    let mut request = agent
        .post(&target.url)
        .set("Content-Type", "application/json");
    for (name, value) in &target.headers {
        request = request.set(name, value);
    }
    if let Err(error) = request.send_bytes(&body) {
        eprintln!("[clud] cache heartbeat failed, next turn may be cold: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(300);

    #[test]
    fn fires_at_ttl_minus_margin_only_when_idle() {
        assert!(!due(Duration::from_secs(284), false, 0, TTL));
        assert!(due(Duration::from_secs(285), false, 0, TTL));
        assert!(
            !due(Duration::from_secs(900), true, 0, TTL),
            "never during a request"
        );
        assert!(!due(
            Duration::from_secs(900),
            false,
            MAX_BEATS_PER_IDLE,
            TTL
        ));
    }

    #[test]
    fn heartbeat_body_keeps_the_prefix_and_appends_one_turn() {
        let recorded = serde_json::json!({
            "model": "deepseek-chat",
            "system": "sys",
            "tools": [{"name": "Bash"}],
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 32000,
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 1000}
        });
        let body = heartbeat_body(&serde_json::to_vec(&recorded).unwrap()).unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(sent["system"], recorded["system"]);
        assert_eq!(sent["tools"], recorded["tools"]);
        assert_eq!(sent["messages"][0], recorded["messages"][0]);
        assert_eq!(sent["messages"][1]["content"], HEARTBEAT_PROMPT);
        assert_eq!(sent["messages"].as_array().unwrap().len(), 2);
        assert_eq!(sent["stream"], false);
        // #2553: thinking parameters are part of what a provider caches the
        // message prefix under; dropping them warmed a different entry.
        assert_eq!(sent["thinking"], recorded["thinking"]);
        assert_eq!(sent["max_tokens"], 1000 + HEARTBEAT_MAX_TOKENS);
        assert!(heartbeat_body(b"not json").is_none());
    }

    fn target() -> Target {
        Target {
            url: "http://127.0.0.1:1/v1/messages".into(),
            headers: Vec::new(),
            body: b"{}".to_vec(),
        }
    }

    #[test]
    fn real_requests_reset_the_clock_and_block_heartbeats_while_in_flight() {
        let beats = Heartbeats::default();
        let t0 = Instant::now();
        beats.begin("c", t0);
        beats.finish("c", Some(target()), t0);
        assert!(beats
            .take_due(t0 + Duration::from_secs(200), TTL)
            .is_empty());
        // A request starts before the window: in flight blocks the beat, and
        // its duration does not count as idle.
        beats.begin("c", t0 + Duration::from_secs(250));
        assert!(beats
            .take_due(t0 + Duration::from_secs(900), TTL)
            .is_empty());
        let done = t0 + Duration::from_secs(1000);
        beats.finish("c", None, done);
        assert!(beats
            .take_due(done + Duration::from_secs(284), TTL)
            .is_empty());
        assert_eq!(
            beats.take_due(done + Duration::from_secs(285), TTL).len(),
            1
        );
        // The next beat is one window after this one, and they stop after
        // the per-idle cap.
        let mut at = done + Duration::from_secs(285);
        for _ in 1..MAX_BEATS_PER_IDLE {
            at += Duration::from_secs(285);
            assert_eq!(beats.take_due(at, TTL).len(), 1);
        }
        assert!(beats
            .take_due(at + Duration::from_secs(285), TTL)
            .is_empty());
    }

    #[test]
    fn a_conversation_without_a_successful_request_is_never_beaten() {
        let beats = Heartbeats::default();
        let t0 = Instant::now();
        beats.begin("c", t0);
        beats.finish("c", None, t0);
        assert!(beats
            .take_due(t0 + Duration::from_secs(600), TTL)
            .is_empty());
    }
}

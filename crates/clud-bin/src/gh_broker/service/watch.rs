//! Subscriptions (#1743, phase 3): `POST /gh/watch`.
//!
//! A waiter (`pr_merge_watch.py`, a script) names up to
//! [`MAX_WATCH_ENDPOINTS`] REST reads and the digests of the bodies it last
//! saw, then blocks. The broker re-reads those endpoints through its own
//! read path, so the refresh cadence is the TTL and every subscriber, and
//! every ordinary read of the same key, shares one upstream request
//! (single-flight, free `304`s). The call returns as soon as a body's digest
//! differs from the one the waiter sent, or when its wait runs out. Below
//! the rate-limit floor the refresh is deferred: the waiter keeps blocking
//! and the reply says until when, so a deferral never reads as "nothing
//! changed" by mistake and never as a change either.

use std::path::{Path, PathBuf};

use kernal_api::hash::Sha256Hasher as Sha256;
use serde::{Deserialize, Serialize};

use super::{error_json, BrokerRead, Caller, GhBroker, ReadError};

/// Endpoints one subscription may name.
pub const MAX_WATCH_ENDPOINTS: usize = 16;
/// The longest one call blocks; a waiter calls again to keep waiting.
pub const MAX_WATCH_MS: u64 = 55_000;
/// How often a blocked call looks at its keys. Cache hits within the TTL
/// cost nothing and are not ledgered; only an expired TTL reaches GitHub.
pub const WATCH_TICK_MS: u64 = 1_000;

/// Waiter -> daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchRequest {
    /// The session's real `gh`, revalidated like `/gh/read`'s.
    pub gh: String,
    pub hostname: Option<String>,
    /// [`super::super::FORWARDED_ENV`] values: the same identity, and so the
    /// same cache keys, as the waiter's own brokered reads.
    pub env: Vec<(String, String)>,
    pub session_id: Option<String>,
    pub endpoints: Vec<String>,
    /// The digest the waiter last saw per endpoint, from the previous
    /// reply. Empty (or shorter than `endpoints`) asks for a baseline: the
    /// call returns at once with every digest.
    #[serde(default)]
    pub seen: Vec<Option<String>>,
    pub wait_ms: u64,
}

/// Daemon -> waiter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchReply {
    /// Per endpoint: a digest of the body served, `status:<code>` for an
    /// upstream error, or `null` when the read failed (no change is
    /// claimed for it).
    pub digests: Vec<Option<String>>,
    /// Indices whose digest differs from `seen`.
    pub changed: Vec<usize>,
    /// Set when a refresh was deferred below the rate-limit floor: the
    /// Unix second the window resets. The digests are of stored copies.
    pub deferred_until_s: Option<u64>,
}

fn digest(body: &[u8]) -> String {
    Sha256::digest(body)
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl GhBroker {
    /// The daemon's `/gh/watch` handler body. Blocks the calling thread for
    /// up to the request's `wait_ms` (capped at [`MAX_WATCH_MS`]).
    pub fn handle_watch(
        &self,
        body: &[u8],
        self_exe: &Path,
        home: Option<&Path>,
    ) -> (u16, Vec<u8>) {
        let request: WatchRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(error) => return (400, error_json(&format!("invalid request: {error}"))),
        };
        if request.endpoints.is_empty() || request.endpoints.len() > MAX_WATCH_ENDPOINTS {
            return (400, error_json("watch 1 to 16 endpoints"));
        }
        let gh = PathBuf::from(&request.gh);
        let endpoints: Vec<&str> = request.endpoints.iter().map(String::as_str).collect();
        let caller = Caller {
            gh: &gh,
            hostname: request.hostname.as_deref(),
            env: &request.env,
        };
        if let Err(message) = caller.validate(&endpoints, self_exe, home) {
            return (400, error_json(message));
        }
        let reply = self.watch(&gh, &request);
        match serde_json::to_vec(&reply) {
            Ok(bytes) => (200, bytes),
            Err(error) => (502, error_json(&error.to_string())),
        }
    }

    /// Read every endpoint until one changes or the wait runs out.
    pub fn watch(&self, gh: &Path, request: &WatchRequest) -> WatchReply {
        let deadline = (self.clock)().saturating_add(request.wait_ms.min(MAX_WATCH_MS));
        let baseline = request.seen.len() < request.endpoints.len();
        let count = request.endpoints.len();
        let mut digests: Vec<Option<String>> = vec![None; count];
        // A read that failed (an upstream error, nothing stored) is not
        // retried every tick: it waits one TTL, like a cached key would.
        let mut retry_at: Vec<u64> = vec![0; count];
        loop {
            let mut deferred_until_s = None;
            for (index, endpoint) in request.endpoints.iter().enumerate() {
                if (self.clock)() < retry_at[index] {
                    continue;
                }
                let read = BrokerRead {
                    gh,
                    endpoint,
                    hostname: request.hostname.as_deref(),
                    env: &request.env,
                    session_id: request.session_id.as_deref(),
                    fresh: false,
                    interactive: false,
                    quiet: true,
                };
                let (result, stale) = self.read_marked(&read);
                if let Some(stale) = stale {
                    deferred_until_s = Some(stale.reset_s);
                }
                // Errors are not cached, so they back off for one TTL.
                let backoff = (self.clock)().saturating_add(super::ttl_ms(endpoint));
                digests[index] = match result {
                    Ok(response) => Some(digest(&response.body)),
                    Err(ReadError::Passthrough(status)) => {
                        retry_at[index] = backoff;
                        Some(format!("status:{status}"))
                    }
                    Err(ReadError::Deferred | ReadError::Failed(_)) => {
                        retry_at[index] = backoff;
                        None
                    }
                };
            }
            let changed: Vec<usize> = if baseline {
                (0..digests.len()).collect()
            } else {
                digests
                    .iter()
                    .zip(&request.seen)
                    .enumerate()
                    .filter(|(_, (now, seen))| now.is_some() && now != seen)
                    .map(|(index, _)| index)
                    .collect()
            };
            let now = (self.clock)();
            if !changed.is_empty() || now >= deadline {
                return WatchReply {
                    digests,
                    changed,
                    deferred_until_s,
                };
            }
            // Below the floor nothing refreshes before the window resets:
            // sleep through to it (or the deadline) instead of ticking.
            let next = match deferred_until_s {
                Some(reset_s) => reset_s.saturating_mul(1000).max(now + WATCH_TICK_MS),
                None => now + WATCH_TICK_MS,
            };
            (self.sleep)(next.min(deadline).saturating_sub(now));
            if (self.clock)() >= deadline {
                // Slept to the deadline: nothing changed in this call, and
                // a last round of reads would only re-defer (or re-hit the
                // cache) for no answer.
                return WatchReply {
                    digests,
                    changed: Vec::new(),
                    deferred_until_s,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests;

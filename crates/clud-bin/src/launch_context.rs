//! Per-session launch-context record (#1675, parent #1276).
//!
//! A transcript does not record the child environment, so nobody could tell
//! which `CLAUDE_CODE_MAX_CONTEXT_TOKENS` a failing session inherited. At
//! launch clud writes one small record of the values the child actually
//! receives; `diagnostics/transcript_report.py` joins it by a hashed session
//! id. Contract: `docs/architecture/launch-plan.md#launch-context-record-1675`.
//!
//! Two steps, because an interactive Claude launch does not know its session
//! id yet:
//!
//! 1. [`ForegroundRuntime::start`](crate::foreground_runtime::ForegroundRuntime)
//!    builds the record from the **final child env** and writes it as
//!    `<state>/launch-context/pending-<token>.json`, putting the random token
//!    in the child env as [`TOKEN_ENV`].
//! 2. `clud session-hook --event SessionStart`, which inherits that env and
//!    receives the session id on stdin, copies it to `<hash>.json` with the
//!    `session` field set ([`bind`]).
//!
//! Both steps are failure-silent: every error is swallowed, and nothing here
//! walks a tree or talks to the daemon. Retention is pruned at write time
//! ([`prune`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use kernal_api::hash::Sha256Hasher as Sha256;
use serde::{Deserialize, Serialize};

use crate::server_settings::ContextWindowSource;

/// Child env var carrying the pending record's random token to the hook.
pub const TOKEN_ENV: &str = "CLUD_LAUNCH_CONTEXT_TOKEN";
/// Directory under the clud state dir.
pub const DIR_NAME: &str = "launch-context";
/// Domain-separation prefix of [`session_hash`]. The Python reader
/// (`transcript_report.py::launch_context_key`) must use the same bytes.
pub const HASH_DOMAIN: &str = "clud-launch-context-v1\0";
/// Records older than this are deleted at write time.
pub const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// At most this many record files are kept (newest first) after a write.
pub const MAX_RECORDS: usize = 200;

pub const MAX_CONTEXT_ENV: &str = "CLAUDE_CODE_MAX_CONTEXT_TOKENS";
pub const AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
pub const AUTOCOMPACT_PCT_ENV: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";

/// The join key: first 16 hex chars of
/// `sha256("clud-launch-context-v1\0" + session_id)`. A Claude session id is
/// a random UUID, so the truncated hash cannot be reversed; the constant
/// domain prefix keeps it from matching any other clud hash of the same id.
pub fn session_hash(session_id: &str) -> String {
    let digest = Sha256::digest(format!("{HASH_DOMAIN}{session_id}").as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// How the child reaches its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// An Anthropic-compatible provider (OpenRouter, DeepSeek, Kimi) the
    /// harness talks to directly; clud is not in the request path.
    Direct,
    UnifiedGateway,
    CodexBridge,
    /// Native Claude or any other harness clud does not overlay.
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Set by the user's environment and passed through.
    Ambient,
    /// A reviewed catalog row.
    Catalog,
    /// The served `model_contexts` datasheet (#1258).
    Served,
    /// Nobody set it; the harness uses its own default.
    Unset,
    /// The child got a value the decision table does not explain.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    pub value: Option<u64>,
    pub source: Source,
}

impl Setting {
    const fn new(value: Option<u64>, source: Source) -> Self {
        Self { value, source }
    }
}

/// Injected facts the decision table runs on. No env, no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub route: Route,
    /// `Some(None)`: the user set the key to a non-number.
    pub ambient_max_context: Option<Option<u64>>,
    pub ambient_compact_window: Option<Option<u64>>,
    /// `server_settings::effective_context_window_with_source(model)`.
    pub model_window: Option<(u32, ContextWindowSource)>,
    /// The catalog row's `claude_compact_window` for the wire model.
    pub catalog_compact_window: Option<u32>,
    /// `provider_catalog::common_claude_context_tokens(Codex)`.
    pub codex_common_window: Option<u32>,
}

/// The `CLAUDE_CODE_MAX_CONTEXT_TOKENS` each route's overlay gives the child.
/// Mirrors `foreground_runtime`: the direct overlay `push_default`s the model
/// window, the Codex bridge `set_env`s the common Codex ceiling, and the
/// unified gateway and native launches never touch the key.
pub fn decide_max_context(facts: &Facts) -> Setting {
    let ambient = facts
        .ambient_max_context
        .map(|value| Setting::new(value, Source::Ambient));
    let unset = Setting::new(None, Source::Unset);
    match facts.route {
        Route::Direct => ambient.unwrap_or_else(|| match facts.model_window {
            Some((tokens, ContextWindowSource::Catalog)) => {
                Setting::new(Some(u64::from(tokens)), Source::Catalog)
            }
            Some((tokens, ContextWindowSource::Served)) => {
                Setting::new(Some(u64::from(tokens)), Source::Served)
            }
            None => unset,
        }),
        Route::CodexBridge => facts
            .codex_common_window
            .map_or(Setting::new(None, Source::Unknown), |tokens| {
                Setting::new(Some(u64::from(tokens)), Source::Catalog)
            }),
        Route::UnifiedGateway | Route::Native => ambient.unwrap_or(unset),
    }
}

/// The `CLAUDE_CODE_AUTO_COMPACT_WINDOW` override. The direct overlay scrubs
/// an ambient value and pushes the catalog's; every other route passes the
/// ambient value through.
pub fn decide_compact_window(facts: &Facts) -> Setting {
    let ambient = facts
        .ambient_compact_window
        .map(|value| Setting::new(value, Source::Ambient));
    match facts.route {
        Route::Direct => facts
            .catalog_compact_window
            .map_or(Setting::new(None, Source::Unset), |tokens| {
                Setting::new(Some(u64::from(tokens)), Source::Catalog)
            }),
        _ => ambient.unwrap_or(Setting::new(None, Source::Unset)),
    }
}

/// Reconcile a decision against what the child env actually holds. The value
/// always comes from the child env; a value the decision did not predict is
/// reported with source `unknown` rather than a guessed one.
pub fn reconcile(decided: Setting, child: Option<Option<u64>>) -> Setting {
    let child_value = child.flatten();
    match (child, decided.source) {
        (None, Source::Unset) => decided,
        (None, _) => Setting::new(None, Source::Unknown),
        (Some(_), _) if child_value == decided.value && decided.source != Source::Unset => decided,
        (Some(_), _) => Setting::new(child_value, Source::Unknown),
    }
}

/// The record. Every field is a fixed vocabulary word, a version, a model id,
/// a number or a hash: no prompt, path, command text or credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    /// [`session_hash`] of the Claude session id; `None` until bound.
    pub session: Option<String>,
    /// Unix seconds at launch.
    pub launched_at: u64,
    pub clud_version: String,
    pub harness: String,
    /// Not cheaply known at launch (it would cost a `claude --version`
    /// spawn), so always `None` today.
    pub harness_version: Option<String>,
    pub route: Route,
    pub provider: String,
    pub wire_model: Option<String>,
    pub max_context_tokens: Setting,
    pub auto_compact_window: Setting,
    pub autocompact_pct_override: Option<u64>,
}

/// Parse one env lookup into the `Option<Option<u64>>` shape: absent, set
/// but not a number, or a number.
pub fn numeric(value: Option<&str>) -> Option<Option<u64>> {
    value.map(|raw| raw.trim().parse::<u64>().ok())
}

/// Last value of `key` in an env list (the one the child sees).
pub fn env_lookup<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(candidate, _)| {
            if crate::platform::host::WINDOWS {
                candidate.eq_ignore_ascii_case(key)
            } else {
                candidate == key
            }
        })
        .map(|(_, value)| value.as_str())
}

/// Plan-derived inputs common to the dry-run preview and the launch record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFacts {
    pub route: Route,
    pub harness: String,
    pub provider: String,
    pub wire_model: Option<String>,
}

impl PlanFacts {
    /// Facts for the decision table, read from the env the launch started
    /// with (before any overlay).
    pub fn facts(&self, ambient: &[(String, String)]) -> Facts {
        let model = self.wire_model.as_deref();
        Facts {
            route: self.route,
            ambient_max_context: numeric(env_lookup(ambient, MAX_CONTEXT_ENV)),
            ambient_compact_window: numeric(env_lookup(ambient, AUTO_COMPACT_WINDOW_ENV)),
            model_window: model
                .and_then(crate::server_settings::effective_context_window_with_source),
            catalog_compact_window: model
                .and_then(crate::provider_catalog::model_by_wire_id)
                .and_then(|row| row.claude_compact_window),
            codex_common_window: crate::provider_catalog::common_claude_context_tokens(
                crate::backend::ModelProvider::Codex,
            ),
        }
    }

    fn record(&self, max: Setting, compact: Setting, pct: Option<u64>, now: u64) -> Record {
        Record {
            v: 1,
            session: None,
            launched_at: now,
            clud_version: env!("CARGO_PKG_VERSION").to_string(),
            harness: self.harness.clone(),
            harness_version: None,
            route: self.route,
            provider: self.provider.clone(),
            wire_model: self.wire_model.clone(),
            max_context_tokens: max,
            auto_compact_window: compact,
            autocompact_pct_override: pct,
        }
    }

    /// What `--dry-run` shows: the decision table's prediction.
    pub fn preview(&self, ambient: &[(String, String)]) -> Record {
        let facts = self.facts(ambient);
        self.record(
            decide_max_context(&facts),
            decide_compact_window(&facts),
            numeric(env_lookup(ambient, AUTOCOMPACT_PCT_ENV)).flatten(),
            0,
        )
    }

    /// What the launch writes: values from the final child env, sources from
    /// the decision table, reconciled.
    pub fn at_launch(
        &self,
        ambient: &[(String, String)],
        child: &[(String, String)],
        now: u64,
    ) -> Record {
        let facts = self.facts(ambient);
        self.record(
            reconcile(
                decide_max_context(&facts),
                numeric(env_lookup(child, MAX_CONTEXT_ENV)),
            ),
            reconcile(
                decide_compact_window(&facts),
                numeric(env_lookup(child, AUTO_COMPACT_WINDOW_ENV)),
            ),
            numeric(env_lookup(child, AUTOCOMPACT_PCT_ENV)).flatten(),
            now,
        )
    }
}

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR_NAME)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn random_token() -> Option<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn valid_token(token: &str) -> bool {
    token.len() == 16 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Launch step: write the pending record and return the token for the child
/// env. `None` on any failure; the launch proceeds unchanged.
pub fn write_pending(state_dir: &Path, record: &Record) -> Option<String> {
    let token = random_token()?;
    let bytes = serde_json::to_vec(record).ok()?;
    let dir = dir(state_dir);
    crate::fs_private::write_private_atomic(&dir.join(format!("pending-{token}.json")), &bytes)
        .ok()?;
    prune(&dir, SystemTime::now());
    Some(token)
}

/// Hook step: copy the pending record to `<session_hash>.json` with the
/// session field set. Silent on any failure. The pending file is left for
/// [`prune`] so a `/clear` (a new session id in the same launch) binds too.
pub fn bind(state_dir: &Path, token: &str, session_id: &str) {
    if !valid_token(token) || session_id.is_empty() {
        return;
    }
    let dir = dir(state_dir);
    let Ok(bytes) = std::fs::read(dir.join(format!("pending-{token}.json"))) else {
        return;
    };
    let Ok(mut record) = serde_json::from_slice::<Record>(&bytes) else {
        return;
    };
    let hash = session_hash(session_id);
    record.session = Some(hash.clone());
    if let Ok(bytes) = serde_json::to_vec(&record) {
        let _ = crate::fs_private::write_private_atomic(&dir.join(format!("{hash}.json")), &bytes);
    }
    prune(&dir, SystemTime::now());
}

/// Retention: delete records older than [`MAX_AGE`], then keep only the
/// newest [`MAX_RECORDS`]. One `read_dir` of a directory this bound keeps
/// small; no recursion.
pub fn prune(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kept: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if now.duration_since(modified).is_ok_and(|age| age > MAX_AGE) {
            let _ = std::fs::remove_file(&path);
        } else {
            kept.push((modified, path));
        }
    }
    if kept.len() > MAX_RECORDS {
        kept.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        for (_, path) in kept.drain(MAX_RECORDS..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Launch-path entry: build from the final child env, write, and add the
/// token to the child env. Failure-silent.
pub fn record_launch(
    plan: &PlanFacts,
    ambient: &[(String, String)],
    child: &mut Vec<(String, String)>,
) {
    let Ok(state_dir) = crate::daemon::default_state_dir() else {
        return;
    };
    let record = plan.at_launch(ambient, child, now_secs());
    child.retain(|(key, _)| !key.eq_ignore_ascii_case(TOKEN_ENV));
    if let Some(token) = write_pending(&state_dir, &record) {
        child.push((TOKEN_ENV.to_string(), token));
    }
}

#[cfg(test)]
#[path = "launch_context_tests.rs"]
mod tests;

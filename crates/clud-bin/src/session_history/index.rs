//! clud's per-cwd index of Claude-harness sessions (#922).
//!
//! One small JSON file per canonical working directory, under
//! `<state>/session-history/<sha256(cwd)>.json`. It holds what the picker and
//! the recovery planner need — the session id, the transcript path, the
//! authoritative clud route, title, activity time, checkpoint count, lineage —
//! and never copies transcript content. Claude's JSONL stays the source of
//! truth and is never modified.
//!
//! Every read-modify-write holds an exclusive lock on a sibling `.lock` file
//! and replaces the index atomically, owner-only (`fs_private`). A corrupt
//! index is treated as empty rather than fatal: it is a cache, rebuilt by the
//! next hook and the lazy transcript import.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::file_lock::FileExt;
use kernal_api::hash::Sha256Hasher as Sha256;
use serde::{Deserialize, Serialize};

pub const INDEX_VERSION: u32 = 1;

/// Which clud route produced a session. Recorded from the launch's resolved
/// `LaunchPlan`; inferred from the model name only for legacy imports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "provider")]
pub enum Route {
    /// Anthropic's own models, native Claude Code.
    Claude,
    /// Another provider driving the Claude harness (Codex, DeepSeek, Kimi,
    /// OpenRouter, unified gateway, ...), by provider name.
    ViaClaude(String),
}

impl Route {
    /// Picker label: `Claude`, `Codex via Claude`, `DeepSeek via Claude`.
    pub fn label(&self) -> String {
        match self {
            Route::Claude => "Claude".to_string(),
            Route::ViaClaude(provider) => format!("{provider} via Claude"),
        }
    }
}

/// Recovery lineage for a session started by portable recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    pub recovered_from: String,
    /// The checkpoint the recovery started from, when one was used.
    pub checkpoint: Option<String>,
    /// Estimated tokens of history left out.
    pub truncated_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub route: Route,
    /// True when `route` was guessed from a model name during legacy import.
    #[serde(default)]
    pub route_inferred: bool,
    pub model: Option<String>,
    pub title: Option<String>,
    /// RFC 3339, for sorting and display.
    pub last_activity: Option<String>,
    #[serde(default)]
    pub compact_checkpoints: usize,
    #[serde(default)]
    pub lineage: Option<Lineage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CwdIndex {
    pub version: u32,
    pub cwd: String,
    /// Set once the lazy transcript import has run for this cwd.
    #[serde(default)]
    pub imported: bool,
    #[serde(default)]
    pub sessions: Vec<SessionEntry>,
}

impl CwdIndex {
    /// Sessions, newest activity first.
    pub fn newest_first(&self) -> Vec<&SessionEntry> {
        let mut sessions: Vec<&SessionEntry> = self.sessions.iter().collect();
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        sessions
    }

    pub fn find(&self, session_id: &str) -> Option<&SessionEntry> {
        self.sessions.iter().find(|s| s.session_id == session_id)
    }

    /// Insert or merge by session id. An authoritative route (from a hook)
    /// always replaces an inferred one; an inferred route never overwrites an
    /// authoritative one. Other fields take the newer value when present.
    pub fn upsert(&mut self, entry: SessionEntry) {
        match self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == entry.session_id)
        {
            None => self.sessions.push(entry),
            Some(existing) => {
                if existing.route_inferred || !entry.route_inferred {
                    existing.route = entry.route;
                    existing.route_inferred = entry.route_inferred;
                }
                existing.transcript_path = entry.transcript_path;
                existing.model = entry.model.or(existing.model.take());
                existing.title = entry.title.or(existing.title.take());
                if entry.last_activity > existing.last_activity {
                    existing.last_activity = entry.last_activity;
                }
                existing.compact_checkpoints =
                    existing.compact_checkpoints.max(entry.compact_checkpoints);
                existing.lineage = entry.lineage.or(existing.lineage.take());
            }
        }
    }

    /// Drop entries whose transcript no longer exists.
    pub fn prune_missing(&mut self) {
        self.sessions.retain(|s| s.transcript_path.is_file());
    }
}

/// The canonical form of `cwd` used as the index key: symlinks resolved,
/// Windows verbatim prefix removed, and case folded on Windows, where paths
/// are case-insensitive.
pub fn canonical_cwd(cwd: &Path) -> String {
    let resolved = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let text = resolved.to_string_lossy().into_owned();
    let text = text
        .strip_prefix(r"\\?\")
        .map(str::to_string)
        .unwrap_or(text);
    if crate::platform::host::WINDOWS {
        text.to_lowercase()
    } else {
        text
    }
}

pub fn index_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("session-history")
}

pub fn index_path(state_dir: &Path, canonical_cwd: &str) -> PathBuf {
    let digest = Sha256::digest(canonical_cwd.as_bytes());
    let name: String = digest
        .as_bytes()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    index_dir(state_dir).join(format!("{name}.json"))
}

/// Read the index for `canonical_cwd` without locking. Missing or corrupt
/// means empty.
pub fn read(state_dir: &Path, canonical_cwd: &str) -> CwdIndex {
    fs::read(index_path(state_dir, canonical_cwd))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<CwdIndex>(&bytes).ok())
        .filter(|index| index.cwd == canonical_cwd)
        .unwrap_or_else(|| empty(canonical_cwd))
}

fn empty(canonical_cwd: &str) -> CwdIndex {
    CwdIndex {
        version: INDEX_VERSION,
        cwd: canonical_cwd.to_string(),
        ..CwdIndex::default()
    }
}

/// Read-modify-write the index under an exclusive inter-process lock.
pub fn update<R>(
    state_dir: &Path,
    canonical_cwd: &str,
    change: impl FnOnce(&mut CwdIndex) -> R,
) -> io::Result<R> {
    let path = index_path(state_dir, canonical_cwd);
    fs::create_dir_all(index_dir(state_dir))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.lock_exclusive()?;
    let mut index = read(state_dir, canonical_cwd);
    let result = change(&mut index);
    index.version = INDEX_VERSION;
    let bytes = serde_json::to_vec_pretty(&index).map_err(io::Error::other)?;
    let written = crate::fs_private::write_private_atomic(&path, &bytes);
    let _ = FileExt::unlock(&lock);
    written.map(|()| result)
}

#[cfg(test)]
#[path = "index_tests.rs"]
pub(crate) mod tests;

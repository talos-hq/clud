//! Issue #2103 item 5: a rare filesystem crawl for repos nothing else saw.
//!
//! Shim hints (#2103 items 1-2) miss a repo made by an absolute `git` path
//! or copied with `cp -a`. This crawl walks the home directory for `.git`
//! entries and resolves each one with the shim's file-only resolver
//! ([`super::shim_hints::repo_root_for`]), so found repos join the scan
//! through the same `RegistryMsg::RepoHints` path.
//!
//! It is bounded and resumable: one probe pass visits at most
//! [`ENTRIES_PER_PASS`] directory entries, and the pending directory stack
//! persists in `<state>/gc/fs-crawl.json`, so a long sweep spans many
//! passes and survives a restart. A finished sweep waits [`SWEEP_INTERVAL_SECS`]
//! before the next. Symlinks are never followed, and build, cache and
//! package trees ([`SKIP_DIR_NAMES`]) are never entered. Read-only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directory entries one pass may read.
pub(super) const ENTRIES_PER_PASS: usize = 50_000;

/// A completed sweep is repeated at most this often.
pub(super) const SWEEP_INTERVAL_SECS: i64 = 7 * 24 * 60 * 60;

/// Never entered: regenerable or package trees that hold no user repos
/// worth reclaiming, and whose size would starve the budget.
pub(super) const SKIP_DIR_NAMES: &[&str] = &[
    "node_modules",
    "target",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".venv",
    "venv",
    "__pycache__",
    ".gradle",
    ".m2",
    "Library",
    "AppData",
    ".Trash",
    "tmp",
];

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CrawlState {
    /// Directories still to read; empty between sweeps.
    #[serde(default)]
    pub(super) stack: Vec<String>,
    #[serde(default)]
    pub(super) last_complete_unix: i64,
}

/// `<state>/gc/fs-crawl.json`.
pub(super) fn crawl_path(state: &Path) -> PathBuf {
    state.join("gc").join("fs-crawl.json")
}

pub(super) fn load(path: &Path) -> CrawlState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub(super) fn save(path: &Path, state: &CrawlState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string(state).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Advance the crawl by at most `budget` entries. Returns repo root → the
/// directory it was found from, in the `RepoHints` shape.
pub(super) fn step(
    state: &mut CrawlState,
    home: &Path,
    budget: usize,
    now: i64,
) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    if state.stack.is_empty() {
        if now - state.last_complete_unix < SWEEP_INTERVAL_SECS {
            return found;
        }
        state.stack.push(home.to_string_lossy().into_owned());
    }
    let mut budget = budget;
    while budget > 0 {
        let Some(dir) = state.stack.pop() else {
            state.last_complete_unix = now;
            break;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            budget = budget.saturating_sub(1);
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            if name == ".git" {
                if let Some(root) = super::shim_hints::repo_root_for(Path::new(&dir)) {
                    found.insert(root.to_string_lossy().into_owned(), dir.clone());
                }
                continue;
            }
            if kind.is_dir() && !SKIP_DIR_NAMES.iter().any(|skip| name == *skip) {
                state
                    .stack
                    .push(entry.path().to_string_lossy().into_owned());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(at: &Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
    }

    #[test]
    fn finds_repos_skips_package_trees_and_resumes_across_passes() {
        let home = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let h = home.path();
        repo(&h.join("code/a"));
        repo(&h.join("deep/x/y/b"));
        repo(&h.join("proj/node_modules/dep"));
        let mut state = CrawlState::default();
        let mut found = BTreeMap::new();
        let mut passes = 0;
        loop {
            found.extend(step(&mut state, h, 2, 1_000_000));
            passes += 1;
            if state.stack.is_empty() {
                break;
            }
            assert!(passes < 100, "crawl never finished");
        }
        assert!(passes > 1, "a small budget spans several passes");
        let names: Vec<_> = found
            .keys()
            .map(|k| {
                Path::new(k)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"a".to_string()) && names.contains(&"b".to_string()));
        assert_eq!(state.last_complete_unix, 1_000_000);
        // Within the interval, a finished sweep does nothing.
        assert!(step(&mut state, h, 100, 1_000_001).is_empty());
        assert!(state.stack.is_empty());
    }

    #[test]
    fn crawl_state_round_trips_and_corrupt_reads_as_new() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = crawl_path(dir.path());
        let state = CrawlState {
            stack: vec!["/a".into()],
            last_complete_unix: 5,
        };
        save(&path, &state).unwrap();
        assert_eq!(load(&path), state);
        std::fs::write(&path, "{x").unwrap();
        assert_eq!(load(&path), CrawlState::default());
    }
}

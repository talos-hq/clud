//! Issue #2101 item 2: `clud gc keep <path>` pins a worktree against the
//! daemon's automatic reclaim.
//!
//! The list is `~/.clud/gc-keep.json` (a sibling of `worktrees`), written by
//! the CLI and read by the daemon on every repo-worktree tick, again on the
//! purge-pool thread immediately before a deletion, and by `clud gc list`.
//! A path keeps itself and everything under it. An unreadable or corrupt
//! file is doubt, and doubt never deletes: it keeps everything.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name of the keep list, a sibling of `worktrees` under `~/.clud`.
pub const KEEP_FILE_NAME: &str = "gc-keep.json";

/// Reason shown for a kept row.
pub const KEPT_REASON: &str = "pinned: kept by user (clud gc keep)";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KeepFile {
    #[serde(default)]
    paths: BTreeSet<String>,
}

/// What the daemon learned from the keep file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepList {
    Paths(BTreeSet<String>),
    /// The file exists but could not be read or parsed.
    Unreadable,
}

impl KeepList {
    /// Is `entry` (or a directory above it) kept?
    pub fn keeps(&self, entry: &str) -> bool {
        match self {
            Self::Unreadable => true,
            Self::Paths(paths) => {
                if paths.is_empty() {
                    return false;
                }
                let entry = Path::new(entry);
                let entry_key = keep_key(entry);
                paths.iter().any(|kept| {
                    entry.starts_with(kept)
                        || Path::new(&entry_key).starts_with(keep_key(Path::new(kept)))
                })
            }
        }
    }
}

fn keep_key(path: &Path) -> String {
    use crate::path_norm::{canonicalize_lenient, normalize_for_key};
    normalize_for_key(&canonicalize_lenient(path))
}

/// `<wt_root>/../gc-keep.json`.
pub fn keep_path_for(wt_root: &Path) -> PathBuf {
    wt_root.parent().unwrap_or(wt_root).join(KEEP_FILE_NAME)
}

/// Production location under the user's home.
pub fn production_keep_path() -> Option<PathBuf> {
    crate::home::user_home()
        .map(|home| keep_path_for(&crate::gc::worktree_root::worktree_root_for(&home)))
}

/// Read the keep list. A missing file keeps nothing.
pub fn load(path: &Path) -> KeepList {
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<KeepFile>(&text) {
            Ok(file) => KeepList::Paths(file.paths),
            Err(_) => KeepList::Unreadable,
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => KeepList::Paths(BTreeSet::new()),
        Err(_) => KeepList::Unreadable,
    }
}

/// Add (`keep == true`) or remove `entry`. Returns whether the list changed.
/// Refuses to rewrite a corrupt file rather than silently dropping its pins.
pub fn update(path: &Path, entry: &str, keep: bool) -> io::Result<bool> {
    let mut paths = match load(path) {
        KeepList::Paths(paths) => paths,
        KeepList::Unreadable => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is unreadable; fix or remove it first", path.display()),
            ))
        }
    };
    let changed = if keep {
        paths.insert(entry.to_string())
    } else {
        paths.remove(entry)
    };
    if changed {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(&KeepFile { paths })?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)?;
    }
    Ok(changed)
}

/// `clud gc keep [--remove] [--list] [PATH]`. Returns the exit code.
pub fn run_cli(path: Option<&Path>, remove: bool, list: bool) -> i32 {
    let Some(file) = production_keep_path() else {
        eprintln!("clud gc keep: cannot resolve the home directory");
        return 1;
    };
    if list || path.is_none() {
        return match load(&file) {
            KeepList::Paths(paths) => {
                for p in &paths {
                    println!("{p}");
                }
                0
            }
            KeepList::Unreadable => {
                eprintln!("clud gc keep: {} is unreadable", file.display());
                1
            }
        };
    }
    let raw = path.unwrap_or(Path::new("."));
    let entry = crate::path_norm::canonicalize_plain(raw).unwrap_or_else(|_| raw.to_path_buf());
    let entry = entry.to_string_lossy().into_owned();
    match update(&file, &entry, !remove) {
        Ok(true) if remove => {
            println!("clud gc keep: released {entry}");
            0
        }
        Ok(true) => {
            println!("clud gc keep: kept {entry}; the daemon will not reclaim it");
            0
        }
        Ok(false) => {
            println!("clud gc keep: no change for {entry}");
            0
        }
        Err(e) => {
            eprintln!("clud gc keep: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_keeps_nothing_and_corrupt_file_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(KEEP_FILE_NAME);
        assert!(!load(&file).keeps("/a/b"));
        fs::write(&file, "{not json").unwrap();
        assert!(load(&file).keeps("/a/b"));
        assert!(
            update(&file, "/a", true).is_err(),
            "corrupt file is not rewritten"
        );
    }

    #[test]
    fn a_kept_path_covers_itself_and_its_children_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(KEEP_FILE_NAME);
        assert!(update(&file, "/wt/repo-a", true).unwrap());
        assert!(!update(&file, "/wt/repo-a", true).unwrap(), "idempotent");
        let list = load(&file);
        assert!(list.keeps("/wt/repo-a"));
        assert!(list.keeps("/wt/repo-a/sub"));
        assert!(!list.keeps("/wt/repo-ab"), "component match, not prefix");
        assert!(update(&file, "/wt/repo-a", false).unwrap());
        assert!(!load(&file).keeps("/wt/repo-a"));
    }

    #[test]
    fn a_pin_matches_the_canonical_filesystem_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("worktree");
        fs::create_dir(&root).unwrap();
        let stored = root.to_string_lossy().into_owned();
        let canonical = fs::canonicalize(&root).unwrap();
        let pins = KeepList::Paths(BTreeSet::from([stored]));
        assert!(pins.keeps(&canonical.join("src").to_string_lossy()));
        assert!(!pins.keeps(&canonical.with_file_name("worktree-other").to_string_lossy()));
    }
}

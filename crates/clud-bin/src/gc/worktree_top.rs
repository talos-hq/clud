//! Issue #2101 item 3: name the largest worktrees in the size banner.
//!
//! When the root is over `worktrees.warn_bytes`, the daemon's probe thread
//! sizes each direct child of `~/.clud/worktrees` (one bounded walk sharing
//! one entry budget) and writes the largest few to
//! `~/.clud/worktrees-top.json`, beside `worktrees-size.json`. The launch
//! banner reads that file, never the tree. A child the budget cut short is
//! reported as a lower bound (`≥`). Missing, corrupt or out-of-date data
//! just drops the list; the banner line itself is unchanged.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name of the per-child sizes, a sibling of `worktrees`.
pub const TOP_FILE_NAME: &str = "worktrees-top.json";

/// How many entries the banner names.
pub const TOP_N: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopEntry {
    pub path: String,
    pub bytes: u64,
    /// False when the entry budget ran out inside this child.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Wire {
    checked_unix: i64,
    entries: Vec<TopEntry>,
}

/// `<wt_root>/../worktrees-top.json`.
pub fn top_path_for(wt_root: &Path) -> PathBuf {
    wt_root.parent().unwrap_or(wt_root).join(TOP_FILE_NAME)
}

/// Bytes under `dir`, spending from the shared `budget`. Symlinks are not
/// followed. Returns `(bytes, complete)`.
fn walk_bytes(dir: &Path, budget: &mut usize) -> (u64, bool) {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return (total, false);
            }
            *budget -= 1;
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    (total, true)
}

/// The `TOP_N` largest direct children of `root`, largest first.
pub fn largest_children(root: &Path, entry_budget: usize) -> Vec<TopEntry> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut budget = entry_budget;
    let mut sized: Vec<TopEntry> = entries
        .flatten()
        .filter(|e| e.path().symlink_metadata().is_ok_and(|m| m.is_dir()))
        .map(|e| {
            let (bytes, complete) = walk_bytes(&e.path(), &mut budget);
            TopEntry {
                path: e.path().to_string_lossy().into_owned(),
                bytes,
                complete,
            }
        })
        .collect();
    sized.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    sized.truncate(TOP_N);
    sized
}

/// Daemon side: size the children and record them atomically.
pub fn refresh(wt_root: &Path, entry_budget: usize, now_unix: i64) -> std::io::Result<()> {
    let wire = Wire {
        checked_unix: now_unix,
        entries: largest_children(wt_root, entry_budget),
    };
    let path = top_path_for(wt_root);
    let text = serde_json::to_string(&wire).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)
}

/// Daemon side when the root is under the limit: drop a leftover list.
pub fn clear(wt_root: &Path) {
    let _ = fs::remove_file(top_path_for(wt_root));
}

/// Launch side: the recorded entries if they were sized within
/// `max_skew_secs` of the size check at `checked_unix`.
pub fn read_for(wt_root: &Path, checked_unix: i64, max_skew_secs: i64) -> Vec<TopEntry> {
    let Ok(text) = fs::read_to_string(top_path_for(wt_root)) else {
        return Vec::new();
    };
    match serde_json::from_str::<Wire>(&text) {
        Ok(w) if (w.checked_unix - checked_unix).abs() <= max_skew_secs => w.entries,
        _ => Vec::new(),
    }
}

/// `; largest: a (12.3 GiB), b (≥4.0 GiB)`, or empty.
pub fn suffix(entries: &[TopEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let names: Vec<String> = entries
        .iter()
        .map(|e| {
            let name = Path::new(&e.path)
                .file_name()
                .map_or_else(|| e.path.clone(), |n| n.to_string_lossy().into_owned());
            let bound = if e.complete { "" } else { "≥" };
            format!(
                "{name} ({bound}{:.1} GiB)",
                e.bytes as f64 / (1u64 << 30) as f64
            )
        })
        .collect();
    format!("; largest: {}", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn names_the_largest_children_first_and_round_trips() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("worktrees");
        write(&root.join("small/a"), 10);
        write(&root.join("big/x/y"), 300);
        write(&root.join("mid/b"), 100);
        write(&root.join("stray.log"), 5000);
        let top = largest_children(&root, 1000);
        let names: Vec<_> = top
            .iter()
            .map(|e| Path::new(&e.path).file_name().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            ["big", "mid", "small"],
            "files at the root are not entries"
        );
        assert!(top.iter().all(|e| e.complete));
        refresh(&root, 1000, 100).unwrap();
        assert_eq!(read_for(&root, 100, 60), top);
        assert!(
            read_for(&root, 1000, 60).is_empty(),
            "out of date list is dropped"
        );
        clear(&root);
        assert!(read_for(&root, 100, 60).is_empty());
    }

    #[test]
    fn an_exhausted_budget_marks_a_lower_bound() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("worktrees");
        for i in 0..5 {
            // `walk_bytes` only reads the metadata length, so set the size
            // rather than writing a real GiB of data per file.
            let path = root.join(format!("only/f{i}"));
            write(&path, 0);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(1 << 30)
                .unwrap();
        }
        let top = largest_children(&root, 2);
        assert_eq!(top.len(), 1);
        assert!(!top[0].complete);
        assert_eq!(suffix(&top), "; largest: only (≥2.0 GiB)");
    }
}

//! Exact changed-line accounting from tool patches and Codex's own session log.
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Context and file headers are not changed lines. Once inside a hunk, even
/// `+++`/`---` are content if preceded by the patch's change prefix.
pub fn unified_counts(patch: &str) -> (usize, usize) {
    let mut counts = (0, 0);
    let mut hunk = false;
    for line in patch.lines() {
        if line.starts_with("@@") {
            hunk = true;
            continue;
        }
        if !hunk {
            continue;
        }
        if line.starts_with('+') {
            counts.0 += 1;
        }
        if line.starts_with('-') {
            counts.1 += 1;
        }
    }
    counts
}

/// Myers line diffs exclude unchanged context in replacement blocks.
pub fn replacement_counts(old: &str, new: &str) -> (usize, usize) {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut counts = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => counts.0 += 1,
            similar::ChangeTag::Delete => counts.1 += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    counts
}

type Patch = Vec<(String, (usize, usize))>;

/// Session records retain the patch that `codex exec --json` omits. Read only
/// the current thread's log, never unrelated sessions or repository contents.
#[derive(Default)]
pub struct CodexPatches {
    root: Option<PathBuf>,
    thread: String,
    cwd: PathBuf,
    pending: VecDeque<Patch>,
}
impl CodexPatches {
    pub fn new(root: Option<PathBuf>, cwd: PathBuf) -> Self {
        Self {
            root,
            cwd,
            ..Self::default()
        }
    }
    pub fn set_thread(&mut self, id: &str) {
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            self.thread = id.to_string();
        }
    }
    pub fn load(&mut self, expected_calls: usize) {
        let Some(root) = self.root.as_ref() else {
            return;
        };
        if self.thread.is_empty() {
            return;
        }
        let Some(path) = find_log(&root.join("sessions"), &self.thread, 4) else {
            return;
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        // Bound both old resumed history and a pathological single record.
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(32 * 1024 * 1024);
        if file.seek(SeekFrom::Start(start)).is_err() {
            return;
        }
        let mut reader = BufReader::new(file.take(32 * 1024 * 1024));
        let mut line = String::new();
        if start > 0 {
            let _ = reader.read_line(&mut line);
            line.clear();
        }
        self.pending.clear();
        let mut saw_turn = false;
        while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                saw_turn |= value["type"] == "event_msg"
                    && matches!(
                        value.pointer("/payload/type").and_then(Value::as_str),
                        Some("task_started" | "turn_started")
                    );
                if saw_turn {
                    self.record(&value);
                }
            }
            line.clear();
        }
        self.align(expected_calls);
    }
    fn align(&mut self, expected_calls: usize) {
        if self.pending.len() != expected_calls {
            self.pending.clear();
        }
    }
    fn record(&mut self, value: &Value) {
        let payload = &value["payload"];
        if value["type"] != "event_msg" {
            return;
        }
        match payload["type"].as_str().unwrap_or("") {
            "task_started" | "turn_started" => self.pending.clear(),
            "patch_apply_end" if payload["success"].as_bool() == Some(true) => {
                self.push_changes(&payload["changes"])
            }
            "item_completed"
                if payload.pointer("/item/type").and_then(Value::as_str) == Some("FileChange")
                    && payload.pointer("/item/status").and_then(Value::as_str)
                        == Some("completed") =>
            {
                self.push_changes(&payload["item"]["changes"])
            }
            _ => {}
        }
    }
    fn push_changes(&mut self, changes: &Value) {
        let Some(changes) = changes.as_object() else {
            return;
        };
        let patch = changes
            .iter()
            .map(|(path, change)| {
                let counts = match change["type"].as_str()? {
                    "add" => (change["content"].as_str()?.lines().count(), 0),
                    "delete" => (0, change["content"].as_str()?.lines().count()),
                    "update" => unified_counts(change["unified_diff"].as_str()?),
                    _ => return None,
                };
                Some((path.clone(), counts))
            })
            .collect::<Option<Vec<_>>>();
        if let Some(patch) = patch.filter(|p| !p.is_empty()) {
            self.pending.push_back(patch);
        }
    }
    pub fn take(&mut self, paths: &[String]) -> Option<Vec<(usize, usize)>> {
        let patch = self.pending.pop_front()?;
        let mut used = std::collections::HashSet::new();
        let counts = paths
            .iter()
            .map(|path| {
                let key = crate::path_norm::normalize_for_key(&self.cwd.join(path));
                let (index, (_, counts)) = patch.iter().enumerate().find(|(index, (p, _))| {
                    !used.contains(index)
                        && crate::path_norm::normalize_for_key(&self.cwd.join(p)) == key
                })?;
                used.insert(index);
                Some(*counts)
            })
            .collect::<Option<Vec<_>>>();
        match counts.filter(|counts| counts.len() == patch.len()) {
            Some(counts) => Some(counts),
            None => {
                self.pending.clear();
                None
            }
        }
    }
}
fn find_log(root: &Path, id: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let kind = entry.file_type().ok()?;
        if kind.is_dir() {
            if let Some(path) = find_log(&entry.path(), id, depth - 1) {
                return Some(path);
            }
        } else if kind.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&format!("-{id}.jsonl")))
        {
            return Some(entry.path());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_context_and_headers_do_not_count() {
        assert_eq!(
            replacement_counts("same\nold\nkeep\nremove\n", "same\nnew\nextra\nkeep\n"),
            (2, 2)
        );
        assert_eq!(
            unified_counts("--- a\n+++ b\n@@ -1,3 +1,3 @@\n same\n-old\n+new\n+++content\n keep\n"),
            (2, 1)
        );
        assert_eq!(replacement_counts("x\r\ny\r\n", "x\r\nz\r\n"), (1, 1));
        assert_eq!(replacement_counts("", "a\nb"), (2, 0));
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn resumed_turns_and_repeated_paths_match_current_call_order() {
        let mut source = CodexPatches::new(None, PathBuf::from("/repo"));
        source.record(&json!({"type":"event_msg","payload":{"type":"patch_apply_end","success":true,"changes":{"/repo/a":{"type":"add","content":"stale\nstale"}}}}));
        source.record(&json!({"type":"event_msg","payload":{"type":"task_started"}}));
        source.record(&json!({"type":"event_msg","payload":{"type":"patch_apply_end","success":true,"changes":{"/repo/a":{"type":"add","content":"first"}}}}));
        source.record(&json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","status":"completed","changes":{"/repo/a":{"type":"delete","content":"first"}}}}}));
        assert_eq!(source.take(&["a".into()]), Some(vec![(1, 0)]));
        assert_eq!(source.take(&["a".into()]), Some(vec![(0, 1)]));
        assert!(source.take(&["a".into()]).is_none());
    }
}

#[cfg(test)]
mod incomplete_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn incomplete_logs_cannot_assign_a_later_patch_to_an_earlier_call() {
        let mut source = CodexPatches::new(None, PathBuf::from("/repo"));
        source.record(&json!({"type":"event_msg","payload":{"type":"patch_apply_end","success":true,"changes":{"/repo/a":{"type":"add","content":"later\npatch"}}}}));
        source.align(2);
        assert!(source.take(&["a".into()]).is_none());
    }
}

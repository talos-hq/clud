//! One-time import of existing Claude transcripts for a cwd (#922).
//!
//! Sessions launched before the index existed, or with plain `claude`, are
//! discovered once per cwd from Claude's own project directory and recorded
//! with a route *inferred* from the model name. After that the index is kept
//! current by clud's lifecycle hooks (`hook.rs`), so a launch never rescans
//! the whole `~/.claude/projects` tree.

use std::path::{Path, PathBuf};

use super::index::{self, canonical_cwd, Route, SessionEntry};
use super::transcript::Transcript;

/// Claude's config root: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
}

/// Claude Code's project-directory name for a cwd: every UTF-16 code unit
/// that is not ASCII alphanumeric becomes `-` (`/work/my.proj` ->
/// `-work-my-proj`). Claude is JavaScript (`replace(/[^a-zA-Z0-9]/g, '-')`),
/// so a character outside the BMP is a surrogate pair and yields two dashes
/// (#2400). Long paths may also be truncated with a hash suffix by newer
/// Claude versions; that is handled by the gc liveness matcher, not here.
pub fn project_slug(cwd: &str) -> String {
    cwd.chars()
        .flat_map(|c| {
            let dashes = if c.is_ascii_alphanumeric() {
                0
            } else {
                c.len_utf16()
            };
            let kept = (dashes == 0).then_some(c);
            kept.into_iter().chain(std::iter::repeat_n('-', dashes))
        })
        .collect()
}

/// Guess a legacy session's route from its model name. Only used when no
/// launch recorded the route; the result is flagged `route_inferred`.
pub fn infer_route(model: Option<&str>) -> Route {
    let Some(model) = model.map(str::to_ascii_lowercase) else {
        return Route::Claude;
    };
    if model.contains("deepseek") {
        Route::ViaClaude("DeepSeek".into())
    } else if model.contains("gpt") || model.contains("codex") || model.starts_with("clud-claude-")
    {
        Route::ViaClaude("Codex".into())
    } else if model.contains("kimi") || model.contains("moonshot") {
        Route::ViaClaude("Kimi".into())
    } else {
        Route::Claude
    }
}

/// Build an index entry from a transcript file, if it belongs to `cwd`.
pub fn entry_from_transcript(path: &Path, cwd: &str) -> Option<SessionEntry> {
    let transcript = Transcript::load(path).ok()?;
    let transcript_cwd = transcript.cwd()?;
    if canonical_cwd(Path::new(transcript_cwd)) != cwd {
        return None;
    }
    let session_id = transcript
        .session_id()
        .map(str::to_string)
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
    let model = transcript.model();
    Some(SessionEntry {
        session_id,
        transcript_path: path.to_path_buf(),
        route: infer_route(model.as_deref()),
        route_inferred: true,
        model,
        title: transcript.title().or_else(|| transcript.preview(80)),
        last_activity: transcript.last_timestamp().map(str::to_string),
        compact_checkpoints: transcript.checkpoints().len(),
        lineage: None,
    })
}

/// Import this cwd's transcripts into the index unless that already happened.
/// Returns how many sessions were added or refreshed.
pub fn import_once(state_dir: &Path, claude_dir: &Path, raw_cwd: &Path) -> std::io::Result<usize> {
    let cwd = canonical_cwd(raw_cwd);
    if index::read(state_dir, &cwd).imported {
        return Ok(0);
    }
    // Claude names the project from the cwd it was launched in. Try both the
    // path as given and its canonical form, since symlinks change the slug.
    let mut dirs = vec![claude_dir
        .join("projects")
        .join(project_slug(&raw_cwd.to_string_lossy()))];
    let canonical_dir = claude_dir.join("projects").join(project_slug(
        &std::fs::canonicalize(raw_cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    ));
    if !dirs.contains(&canonical_dir) {
        dirs.push(canonical_dir);
    }
    let entries: Vec<SessionEntry> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|p| entry_from_transcript(&p, &cwd))
        .collect();
    let count = entries.len();
    index::update(state_dir, &cwd, |index| {
        for entry in entries {
            index.upsert(entry);
        }
        index.imported = true;
    })?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_history::transcript::tests::{assistant, compact_summary, user};
    use serde_json::Value;

    fn write_transcript(dir: &Path, name: &str, cwd: &Path, records: &[Value]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}.jsonl"));
        let lines: Vec<String> = records
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r["cwd"] = Value::String(cwd.to_string_lossy().into_owned());
                r["sessionId"] = Value::String(name.to_string());
                r.to_string()
            })
            .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn slug_matches_claude_codes_rule() {
        assert_eq!(project_slug("/work/my.proj"), "-work-my-proj");
        assert_eq!(project_slug(r"C:\Users\me\repo"), "C--Users-me-repo");
        // JS counts UTF-16 units: a non-BMP char is two dashes (#2400).
        assert_eq!(project_slug("/a/\u{1F600}b"), "-a---b");
        assert_eq!(project_slug("/a/\u{e9}b"), "-a--b");
    }

    #[test]
    fn route_inference_covers_the_claude_harness_providers() {
        assert_eq!(infer_route(Some("claude-sonnet-4-5")), Route::Claude);
        assert_eq!(
            infer_route(Some("gpt-5.6-terra")),
            Route::ViaClaude("Codex".into())
        );
        assert_eq!(
            infer_route(Some("clud-claude-codex-sol")),
            Route::ViaClaude("Codex".into())
        );
        assert_eq!(
            infer_route(Some("deepseek-v4-pro")),
            Route::ViaClaude("DeepSeek".into())
        );
        assert_eq!(infer_route(None), Route::Claude);
    }

    /// Only transcripts whose recorded cwd matches exactly are imported, and
    /// the import runs once: a second call touches nothing.
    #[test]
    fn imports_only_this_cwd_and_only_once() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cwd = root.path().join("proj");
        let other = root.path().join("other");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let claude = root.path().join("claude");
        let project = claude
            .join("projects")
            .join(project_slug(&cwd.to_string_lossy()));
        write_transcript(
            &project,
            "keep",
            &cwd,
            &[
                user("01", None, "hello"),
                assistant("02", "01", "hi"),
                compact_summary("03", "02", "summary"),
                assistant("04", "03", "after"),
            ],
        );
        // Same project dir (slug collision), different recorded cwd: skipped.
        write_transcript(&project, "stray", &other, &[user("01", None, "x")]);
        let state = root.path().join("state");

        assert_eq!(import_once(&state, &claude, &cwd).unwrap(), 1);
        let index = index::read(&state, &canonical_cwd(&cwd));
        assert!(index.imported);
        let entry = index.find("keep").unwrap();
        assert!(entry.route_inferred);
        assert_eq!(entry.compact_checkpoints, 1);
        assert_eq!(entry.title.as_deref(), Some("hello"));

        assert_eq!(import_once(&state, &claude, &cwd).unwrap(), 0);
    }
}

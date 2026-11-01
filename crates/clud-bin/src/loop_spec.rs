//! Task-spec resolution and done-signal plumbing for `clud loop`.
//!
//! Responsibilities:
//! - Classify the positional argument (GH URL, short-form `#42`, local file,
//!   literal prompt).
//! - Fetch a GH issue/PR body via `gh` (with `curl` fallback) and cache it
//!   under `<git-root>/.clud/loop/`.
//! - Locate the marker directory and detect DONE/BLOCKED terminal files.
//!
//! The actual iteration-control loop lives in `main.rs`; this module is
//! side-effectful only at loop-start (fetching/caching) and loop-iter-end
//! (marker polling).

use std::path::{Path, PathBuf};
use std::time::Duration;

use running_process::{NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode};

use crate::subprocess;
use crate::win_creation_flags::invisible_helper_creationflags;

/// Display string for the marker directory (forward slashes for the
/// user-facing prompt). Always join the segments via `Path::join` when
/// constructing on-disk paths so the separators stay platform-native —
/// otherwise mixed separators (`.clud/loop\DONE` on Windows) leak into
/// the prompt text and confuse the agent.
pub const LOOP_DIR: &str = ".clud/loop";
const LOOP_DIR_PARENT: &str = ".clud";
const LOOP_DIR_LEAF: &str = "loop";
pub const DONE_MARKER: &str = "DONE";
pub const BLOCKED_MARKER: &str = "BLOCKED";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerPaths {
    pub done: PathBuf,
    pub blocked: PathBuf,
}

/// How to interpret the positional argument passed to `clud loop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskSpec {
    GhIssue {
        owner: String,
        repo: String,
        kind: GhKind,
        number: u32,
    },
    ShortForm(u32),
    File(PathBuf),
    Literal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhKind {
    Issue,
    Pr,
}

impl GhKind {
    fn as_gh_subcommand(self) -> &'static str {
        match self {
            GhKind::Issue => "issue",
            GhKind::Pr => "pr",
        }
    }

    fn label(self) -> &'static str {
        match self {
            GhKind::Issue => "issue",
            GhKind::Pr => "pull",
        }
    }
}

/// Classify a positional task argument. Input detection order:
///   1. GH issue/PR URL
///   2. Short-form `#42` or `42`
///   3. Local file path
///   4. Literal prompt
pub fn classify(input: &str) -> TaskSpec {
    if let Some(parsed) = parse_gh_url(input) {
        return parsed;
    }
    if let Some(n) = parse_short_form(input) {
        return TaskSpec::ShortForm(n);
    }
    let path = Path::new(input);
    if path.is_file() {
        return TaskSpec::File(path.to_path_buf());
    }
    TaskSpec::Literal(input.to_string())
}

fn parse_gh_url(input: &str) -> Option<TaskSpec> {
    // Keep dependency count minimal — manual parse instead of pulling regex.
    let s = input.strip_suffix('/').unwrap_or(input);
    let rest = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))?;
    let mut parts = rest.splitn(4, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    let kind_seg = parts.next()?;
    let number_seg = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let kind = match kind_seg {
        "issues" => GhKind::Issue,
        "pull" => GhKind::Pr,
        _ => return None,
    };
    let number: u32 = number_seg.parse().ok()?;
    Some(TaskSpec::GhIssue {
        owner,
        repo,
        kind,
        number,
    })
}

fn parse_short_form(input: &str) -> Option<u32> {
    let s = input.strip_prefix('#').unwrap_or(input);
    if s.is_empty() {
        return None;
    }
    s.parse().ok()
}

/// Find the git-root by walking upward from `start` looking for `.git`.
/// Falls back to `start` if no git root is found.
pub fn git_root_from(start: &Path) -> PathBuf {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".git").exists() {
            return cur;
        }
        if !cur.pop() {
            return start.to_path_buf();
        }
    }
}

/// Resolve the `<git-root>/.clud/loop/` directory.
///
/// Joins segment-by-segment so the separators stay platform-native
/// (`\` on Windows, `/` elsewhere). Using `git_root.join(".clud/loop")`
/// would leak a stray `/` into the middle of an otherwise-Windows path.
pub fn loop_dir(git_root: &Path) -> PathBuf {
    git_root.join(LOOP_DIR_PARENT).join(LOOP_DIR_LEAF)
}

/// Path to the DONE marker.
pub fn done_path(git_root: &Path) -> PathBuf {
    loop_dir(git_root).join(DONE_MARKER)
}

/// Path to the BLOCKED marker.
pub fn blocked_path(git_root: &Path) -> PathBuf {
    loop_dir(git_root).join(BLOCKED_MARKER)
}

/// Derive the BLOCKED marker path from a custom DONE marker path.
///
/// `DONE.md` becomes `BLOCKED.md`; `DONE` becomes `BLOCKED`.
pub fn blocked_path_from_done(done: &Path) -> PathBuf {
    let parent = done.parent().unwrap_or_else(|| Path::new("."));
    let ext = done.extension().and_then(|s| s.to_str());
    let file_name = match ext {
        Some(ext) if !ext.is_empty() => format!("{BLOCKED_MARKER}.{ext}"),
        _ => BLOCKED_MARKER.to_string(),
    };
    parent.join(file_name)
}

pub fn default_marker_paths(git_root: &Path) -> MarkerPaths {
    MarkerPaths {
        done: done_path(git_root),
        blocked: blocked_path(git_root),
    }
}

/// Marker state observed after an iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerState {
    Done(String),
    Blocked(String),
    None,
}

/// Read the marker state from `<git-root>/.clud/loop/`. DONE wins if both
/// exist (the agent's last word that the task resolved).
pub fn read_markers(git_root: &Path) -> MarkerState {
    let paths = default_marker_paths(git_root);
    read_markers_at(&paths)
}

pub fn read_markers_at(paths: &MarkerPaths) -> MarkerState {
    if paths.done.is_file() {
        let body = std::fs::read_to_string(&paths.done).unwrap_or_default();
        return MarkerState::Done(body.trim().to_string());
    }
    if paths.blocked.is_file() {
        let body = std::fs::read_to_string(&paths.blocked).unwrap_or_default();
        return MarkerState::Blocked(body.trim().to_string());
    }
    MarkerState::None
}

/// Remove stale DONE/BLOCKED markers so we start from a clean slate.
pub fn clear_markers(git_root: &Path) {
    let paths = default_marker_paths(git_root);
    clear_markers_at(&paths);
}

pub fn clear_markers_at(paths: &MarkerPaths) {
    let _ = std::fs::remove_file(&paths.done);
    let _ = std::fs::remove_file(&paths.blocked);
}

/// Ensure the `.clud/loop/` dir exists under the given git root.
pub fn ensure_loop_dir(git_root: &Path) -> std::io::Result<PathBuf> {
    let dir = loop_dir(git_root);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Cache file path for a GH issue/PR.
pub fn cache_path(git_root: &Path, owner: &str, repo: &str, kind: GhKind, number: u32) -> PathBuf {
    loop_dir(git_root).join(format!(
        "{}__{}__{}-{}.md",
        sanitize(owner),
        sanitize(repo),
        kind.label(),
        number
    ))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve a short-form `#42` input by asking `gh` for the current repo.
/// Returns `(owner, repo)` or an error string suitable for the user.
pub fn resolve_current_repo() -> Result<(String, String), String> {
    let (exit_code, stdout) = run_gh_capture(&["repo", "view", "--json", "owner,name"])?;
    if exit_code != 0 {
        return Err(format!("`gh repo view` failed with exit {exit_code}"));
    }
    // Two-field JSON: minimal parse without pulling serde_json dep here.
    let v: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("gh JSON parse: {e}"))?;
    let owner = v
        .get("owner")
        .and_then(|o| o.get("login"))
        .and_then(|s| s.as_str())
        .ok_or_else(|| "gh response missing owner.login".to_string())?
        .to_string();
    let name = v
        .get("name")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "gh response missing name".to_string())?
        .to_string();
    Ok((owner, name))
}

/// Fetched issue/PR data, minimal shape needed for prompt assembly.
#[derive(Debug, Clone)]
pub struct IssueDoc {
    pub url: String,
    pub title: String,
    pub body: String,
    pub state: String,
    pub author: String,
    pub labels: Vec<String>,
    pub comments: Vec<IssueComment>,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct IssueComment {
    pub author: String,
    pub created_at: String,
    pub body: String,
}

/// Fetch a GH issue or PR via `gh` CLI. Callers should pass the same `kind`
/// that produced the input URL (issue vs pull).
pub fn fetch_via_gh(
    owner: &str,
    repo: &str,
    kind: GhKind,
    number: u32,
) -> Result<IssueDoc, String> {
    let slug = format!("{owner}/{repo}");
    let number_str = number.to_string();
    let args: &[&str] = &[
        kind.as_gh_subcommand(),
        "view",
        &number_str,
        "--repo",
        &slug,
        "--json",
        "number,title,body,state,author,labels,comments,updatedAt,url",
    ];
    let (exit_code, stdout) = run_gh_capture(args)?;
    if exit_code != 0 {
        return Err(format!(
            "`gh {} view` failed with exit {exit_code}",
            kind.as_gh_subcommand()
        ));
    }
    parse_gh_json(&stdout)
}

/// Run `gh` with `args`, capturing combined stdout/stderr. Returns
/// `(exit_code, captured_output)`. Uses `running-process-core` per the
/// repo's subprocess policy (see ci/check-banned-imports).
pub(crate) fn run_gh_capture(args: &[&str]) -> Result<(i32, String), String> {
    let mut argv = vec!["gh".to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    run_capture(argv, None)
}

/// Run `argv` (in `cwd`, when given), capturing combined stdout/stderr.
/// Returns `(exit_code, captured_output)`. Shared by the `gh` and `git`
/// helpers so every piped helper goes through one running-process path.
pub(crate) fn run_capture(
    argv: Vec<String>,
    cwd: Option<&std::path::Path>,
) -> Result<(i32, String), String> {
    let program = argv.first().cloned().unwrap_or_default();
    let config = ProcessConfig {
        command: subprocess::command_spec_for_subprocess(argv),
        cwd: cwd.map(std::path::Path::to_path_buf),
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        // Issue #55: `gh` invocation is a piped helper — output is
        // captured and parsed; the user never interacts with this child's
        // console. Suppress the conhost popup on Windows. No-op
        // elsewhere.
        creationflags: invisible_helper_creationflags(),
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new(config);
    process
        .start()
        .map_err(|e| format!("failed to start `{program}`: {e}"))?;

    let mut buf = Vec::<u8>::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => {
                buf.extend_from_slice(&event.line);
                buf.push(b'\n');
            }
            ReadStatus::Timeout => {
                if process.returncode().is_some() {
                    break;
                }
            }
            ReadStatus::Eof => break,
        }
    }
    // Exit is not end of output: the last lines can reach the capture queue
    // after the exit code on a loaded machine. `wait` runs running-process's
    // bounded capture drain, so the queue is complete once it returns (#2159).
    let exit_code = process
        .wait(Some(Duration::from_secs(30)))
        .map_err(|e| format!("waiting for {program}: {e}"))?;
    for event in process.drain_combined() {
        buf.extend_from_slice(&event.line);
        buf.push(b'\n');
    }
    Ok((exit_code, String::from_utf8_lossy(&buf).to_string()))
}

fn parse_gh_json(stdout: &str) -> Result<IssueDoc, String> {
    let v: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| format!("gh JSON parse: {e}"))?;
    let url = v
        .get("url")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let title = v
        .get("title")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let body = v
        .get("body")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let state = v
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let author = v
        .get("author")
        .and_then(|o| o.get("login"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let updated_at = v
        .get("updatedAt")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let labels = v
        .get("labels")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("name").and_then(|s| s.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let comments = v
        .get("comments")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .map(|c| IssueComment {
                    author: c
                        .get("author")
                        .and_then(|o| o.get("login"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    created_at: c
                        .get("createdAt")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    body: c
                        .get("body")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(IssueDoc {
        url,
        title,
        body,
        state,
        author,
        labels,
        comments,
        updated_at,
    })
}

/// Render a fetched issue into the cache file body (with YAML frontmatter).
pub fn render_cache(doc: &IssueDoc, fetched_at: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("url: {}\n", doc.url));
    out.push_str(&format!("fetched_at: {}\n", fetched_at));
    out.push_str(&format!("updated_at: {}\n", doc.updated_at));
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n\n", doc.title));
    if !doc.state.is_empty() {
        out.push_str(&format!("State: {}\n", doc.state));
    }
    if !doc.author.is_empty() {
        out.push_str(&format!("Author: @{}\n", doc.author));
    }
    if !doc.labels.is_empty() {
        out.push_str(&format!("Labels: {}\n", doc.labels.join(", ")));
    }
    out.push_str("\n## Body\n\n");
    out.push_str(&doc.body);
    out.push('\n');
    if !doc.comments.is_empty() {
        out.push_str("\n## Comments\n");
        for c in &doc.comments {
            out.push_str(&format!("\n### @{} ({})\n\n", c.author, c.created_at));
            out.push_str(&c.body);
            out.push('\n');
        }
    }
    out
}

/// Default prompt instructions appended to every loop-driven task when the
/// DONE/BLOCKED marker contract is active.
///
/// `done_abs` and `blocked_abs` are passed as absolute paths so the model
/// cannot reinterpret a display-relative form (e.g. `.clud/loop/DONE`) as a
/// generic "write a completion file somewhere" instruction. See issue #95
/// for the original failure mode (agent writing to `~/.loop/LOOP.md`).
///
/// The contract also documents the `<<<CLUD_LOOP_DONE: ...>>>` /
/// `<<<CLUD_LOOP_BLOCKED: ...>>>` token fallback (see
/// `scan_completion_token`) for the case where the agent cannot write a
/// file — the loop runner scans its captured output for those tokens.
pub fn done_marker_contract(done_abs: &Path, blocked_abs: &Path) -> String {
    let done_display = done_abs.display();
    let blocked_display = blocked_abs.display();
    format!(
        "\n\n---\n\
You are running in a ralph loop. The loop will re-invoke you up to N times \
with the same task until you complete it.\n\
\n\
When the task is fully resolved and verified (tests pass, lint clean where \
applicable), write the file at this EXACT absolute path with a one-line \
summary of what you did:\n\
\n\
    {done_display}\n\
\n\
If you cannot make progress (missing info, external dependency, needs \
human input), write the file at this EXACT absolute path with a one-line \
reason and stop:\n\
\n\
    {blocked_display}\n\
\n\
Do not create any other completion files. Only writing to the exact \
absolute path above terminates the loop. Do not write to ~/.loop/, \
./loop.md, LOOP.md, or any other location.\n\
\n\
If you cannot write the marker file for any reason, you may instead emit \
the literal token `<<<CLUD_LOOP_DONE: <one-line summary>>>>` on a line by \
itself as the very last thing you output, and clud will treat that as a \
DONE signal. Use `<<<CLUD_LOOP_BLOCKED: <reason>>>>` for the BLOCKED \
equivalent. Writing the marker file is still strongly preferred.\n\
\n\
Do not write DONE prematurely — only after you are confident the work is \
complete. Otherwise, continue working — you will be re-invoked.\n\
\n\
If the work lands through a pull request and the base branch uses a merge \
queue, a PR accepted into the merge queue counts as complete: write DONE \
with the PR URL as soon as the enqueue succeeds and stop. Do not wait for \
the merge itself; the queue (or a chaperone) owns it from there, and the \
loop moves on to the next item.\n"
    )
}

/// Prompt paragraph telling the agent where operator messages for the
/// current iteration appear (#2471). Appended after the DONE/BLOCKED
/// contract whenever marker injection is active.
pub fn next_message_contract(next_message_abs: &Path) -> String {
    format!(
        "\nAt the start of every iteration, check whether this file exists:\n\n    {}\n\n\
If it does, it holds messages the operator queued for this iteration. \
Read it first and follow its instructions; they take priority over the \
original task text where they conflict. Do not edit or delete it; clud \
manages it.\n",
        next_message_abs.display()
    )
}

/// Result of scanning captured output for a `<<<CLUD_LOOP_...>>>` token.
///
/// Mirrors `MarkerState` but is produced by parsing stdout/stderr text
/// rather than by reading marker files. This is a fallback for agents that
/// summarize "task complete" without writing the marker file (issue #95).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenState {
    Done(String),
    Blocked(String),
    None,
}

/// Scan captured output for completion tokens.
///
/// Recognizes lines that START with `<<<CLUD_LOOP_DONE:` or
/// `<<<CLUD_LOOP_BLOCKED:` and end with `>>>`. Tokens embedded mid-line are
/// ignored — the contract documents that the token must be on a line by
/// itself. If multiple tokens appear, the LAST one wins (the agent's
/// final word).
///
/// Used only in subprocess launch mode where we capture stdout. In PTY
/// mode the child writes directly to the user's terminal and we never see
/// the bytes — for now, only the marker-file path is supported there.
pub fn scan_completion_token(captured: &str) -> TokenState {
    const DONE_PREFIX: &str = "<<<CLUD_LOOP_DONE:";
    const BLOCKED_PREFIX: &str = "<<<CLUD_LOOP_BLOCKED:";
    const SUFFIX: &str = ">>>";

    let mut last: TokenState = TokenState::None;
    for raw_line in captured.lines() {
        let line = raw_line.trim();
        let (state, payload) = if let Some(rest) = line.strip_prefix(DONE_PREFIX) {
            let Some(inner) = rest.strip_suffix(SUFFIX) else {
                continue;
            };
            (
                TokenState::Done(String::new()),
                inner.trim().trim_end_matches('>').trim().to_string(),
            )
        } else if let Some(rest) = line.strip_prefix(BLOCKED_PREFIX) {
            let Some(inner) = rest.strip_suffix(SUFFIX) else {
                continue;
            };
            (
                TokenState::Blocked(String::new()),
                inner.trim().trim_end_matches('>').trim().to_string(),
            )
        } else {
            continue;
        };
        last = match state {
            TokenState::Done(_) => TokenState::Done(payload),
            TokenState::Blocked(_) => TokenState::Blocked(payload),
            TokenState::None => TokenState::None,
        };
    }
    last
}

/// Read marker state from disk OR from a token in `captured` output, with
/// the marker file taking precedence. This is the loop-runner entry point
/// for subprocess mode where stdout is captured into a buffer.
pub fn read_markers_or_token(paths: &MarkerPaths, captured: &str) -> MarkerState {
    match read_markers_at(paths) {
        MarkerState::None => match scan_completion_token(captured) {
            TokenState::Done(s) => MarkerState::Done(s),
            TokenState::Blocked(s) => MarkerState::Blocked(s),
            TokenState::None => MarkerState::None,
        },
        state => state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_issue_url() {
        match classify("https://github.com/acme/widgets/issues/42") {
            TaskSpec::GhIssue {
                owner,
                repo,
                kind,
                number,
            } => {
                assert_eq!(owner, "acme");
                assert_eq!(repo, "widgets");
                assert_eq!(kind, GhKind::Issue);
                assert_eq!(number, 42);
            }
            other => panic!("expected GhIssue, got {other:?}"),
        }
    }

    #[test]
    fn classify_pr_url_trailing_slash() {
        match classify("https://github.com/acme/widgets/pull/7/") {
            TaskSpec::GhIssue { kind, number, .. } => {
                assert_eq!(kind, GhKind::Pr);
                assert_eq!(number, 7);
            }
            other => panic!("expected GhIssue(Pr), got {other:?}"),
        }
    }

    #[test]
    fn classify_short_form() {
        assert_eq!(classify("#42"), TaskSpec::ShortForm(42));
        assert_eq!(classify("1"), TaskSpec::ShortForm(1));
    }

    #[test]
    fn classify_literal() {
        match classify("do the task") {
            TaskSpec::Literal(s) => assert_eq!(s, "do the task"),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn classify_file() {
        let tmp = tempfile::NamedTempFile::new().expect("tmp");
        std::fs::write(tmp.path(), "task body").unwrap();
        let path_str = tmp.path().to_string_lossy().to_string();
        match classify(&path_str) {
            TaskSpec::File(p) => assert_eq!(p, tmp.path()),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn classify_rejects_wrong_host() {
        match classify("https://gitlab.com/acme/widgets/issues/1") {
            TaskSpec::Literal(_) => {}
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn cache_path_sanitizes_segments() {
        let p = cache_path(
            Path::new("/tmp/repo"),
            "acme-co",
            "wid.gets",
            GhKind::Issue,
            42,
        );
        assert!(p
            .to_string_lossy()
            .ends_with("acme-co__wid_gets__issue-42.md"));
    }

    #[test]
    fn read_markers_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_markers(tmp.path()), MarkerState::None);
    }

    #[test]
    fn read_markers_done() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_loop_dir(tmp.path()).unwrap();
        std::fs::write(done_path(tmp.path()), "all good\n").unwrap();
        assert_eq!(
            read_markers(tmp.path()),
            MarkerState::Done("all good".to_string())
        );
    }

    #[test]
    fn read_markers_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_loop_dir(tmp.path()).unwrap();
        std::fs::write(blocked_path(tmp.path()), "need secret\n").unwrap();
        assert_eq!(
            read_markers(tmp.path()),
            MarkerState::Blocked("need secret".to_string())
        );
    }

    #[test]
    fn read_markers_done_wins_over_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_loop_dir(tmp.path()).unwrap();
        std::fs::write(done_path(tmp.path()), "done").unwrap();
        std::fs::write(blocked_path(tmp.path()), "ignored").unwrap();
        assert_eq!(
            read_markers(tmp.path()),
            MarkerState::Done("done".to_string())
        );
    }

    #[test]
    fn clear_markers_removes_both() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_loop_dir(tmp.path()).unwrap();
        std::fs::write(done_path(tmp.path()), "x").unwrap();
        std::fs::write(blocked_path(tmp.path()), "y").unwrap();
        clear_markers(tmp.path());
        assert_eq!(read_markers(tmp.path()), MarkerState::None);
    }

    #[test]
    fn parse_gh_json_happy_path() {
        let raw = r#"{
            "url": "https://github.com/acme/widgets/issues/42",
            "title": "Bug: things break",
            "body": "when I do X, Y happens",
            "state": "OPEN",
            "author": {"login": "alice"},
            "labels": [{"name": "bug"}, {"name": "prio-high"}],
            "comments": [
                {"author": {"login": "bob"}, "createdAt": "2026-04-10T00:00:00Z", "body": "repro?"}
            ],
            "updatedAt": "2026-04-11T00:00:00Z"
        }"#;
        let doc = parse_gh_json(raw).expect("parse");
        assert_eq!(doc.title, "Bug: things break");
        assert_eq!(doc.author, "alice");
        assert_eq!(doc.labels, vec!["bug", "prio-high"]);
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].author, "bob");
    }

    // ---- Issue #95: tightened DONE marker contract ----

    #[test]
    fn done_marker_contract_uses_absolute_paths() {
        let done = Path::new("/tmp/proj/.clud/loop/DONE");
        let blocked = Path::new("/tmp/proj/.clud/loop/BLOCKED");
        let text = done_marker_contract(done, blocked);
        // Absolute paths must appear verbatim in the contract.
        assert!(
            text.contains(&done.display().to_string()),
            "contract must reference absolute DONE path: {text}"
        );
        assert!(
            text.contains(&blocked.display().to_string()),
            "contract must reference absolute BLOCKED path: {text}"
        );
        // Explicit "do not write elsewhere" rule must be present.
        assert!(
            text.contains("Do not create any other completion files"),
            "contract must forbid other completion file locations"
        );
        // Token fallback must be documented.
        assert!(
            text.contains("<<<CLUD_LOOP_DONE:"),
            "contract must document the DONE token fallback"
        );
        assert!(
            text.contains("<<<CLUD_LOOP_BLOCKED:"),
            "contract must document the BLOCKED token fallback"
        );
    }

    // ---- Issue #95: token-based completion fallback ----

    #[test]
    fn scan_token_matches_done() {
        let s = "work work work\n<<<CLUD_LOOP_DONE: fixed the bug>>>\n";
        assert_eq!(
            scan_completion_token(s),
            TokenState::Done("fixed the bug".to_string())
        );
    }

    #[test]
    fn scan_token_matches_blocked() {
        let s = "<<<CLUD_LOOP_BLOCKED: missing API key>>>";
        assert_eq!(
            scan_completion_token(s),
            TokenState::Blocked("missing API key".to_string())
        );
    }

    #[test]
    fn scan_token_ignores_unclosed() {
        let s = "<<<CLUD_LOOP_DONE: oops no terminator\nnext line";
        assert_eq!(scan_completion_token(s), TokenState::None);
    }

    #[test]
    fn scan_token_ignores_midline_occurrence() {
        // Token must START the line (after trimming whitespace).
        let s = "blah blah <<<CLUD_LOOP_DONE: nope>>> trailing";
        assert_eq!(scan_completion_token(s), TokenState::None);
    }

    #[test]
    fn scan_token_prefers_later_token() {
        let s =
            "<<<CLUD_LOOP_DONE: first attempt>>>\nmore work\n<<<CLUD_LOOP_DONE: actually done>>>";
        assert_eq!(
            scan_completion_token(s),
            TokenState::Done("actually done".to_string())
        );
    }

    #[test]
    fn scan_token_none_for_empty() {
        assert_eq!(scan_completion_token(""), TokenState::None);
        assert_eq!(
            scan_completion_token("just some output\n"),
            TokenState::None
        );
    }

    #[test]
    fn scan_token_strips_extra_trailing_angle_brackets() {
        // The contract template uses `>>>>` at the end (matched template
        // syntax `<<<...>>>>`). We accept stray trailing `>` in the payload
        // so the model isn't punished for the visually-confusing example.
        let s = "<<<CLUD_LOOP_DONE: payload>>>>";
        assert_eq!(
            scan_completion_token(s),
            TokenState::Done("payload".to_string())
        );
    }

    #[test]
    fn read_markers_or_token_prefers_marker_file() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_loop_dir(tmp.path()).unwrap();
        std::fs::write(done_path(tmp.path()), "via file\n").unwrap();
        let paths = default_marker_paths(tmp.path());
        let captured = "<<<CLUD_LOOP_BLOCKED: via token>>>\n";
        assert_eq!(
            read_markers_or_token(&paths, captured),
            MarkerState::Done("via file".to_string())
        );
    }

    #[test]
    fn read_markers_or_token_falls_back_to_token() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = default_marker_paths(tmp.path());
        let captured = "<<<CLUD_LOOP_DONE: token-only completion>>>\n";
        assert_eq!(
            read_markers_or_token(&paths, captured),
            MarkerState::Done("token-only completion".to_string())
        );
    }

    #[test]
    fn read_markers_or_token_none_when_neither() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = default_marker_paths(tmp.path());
        assert_eq!(
            read_markers_or_token(&paths, "nothing relevant\n"),
            MarkerState::None
        );
    }

    #[test]
    fn render_cache_includes_frontmatter_and_sections() {
        let doc = IssueDoc {
            url: "https://github.com/a/b/issues/1".into(),
            title: "t".into(),
            body: "b".into(),
            state: "OPEN".into(),
            author: "al".into(),
            labels: vec!["bug".into()],
            comments: vec![IssueComment {
                author: "bo".into(),
                created_at: "2026-04-10T00:00:00Z".into(),
                body: "note".into(),
            }],
            updated_at: "2026-04-11T00:00:00Z".into(),
        };
        let rendered = render_cache(&doc, "2026-04-16T10:00:00Z");
        assert!(rendered.starts_with("---\n"));
        assert!(rendered.contains("url: https://github.com/a/b/issues/1"));
        assert!(rendered.contains("fetched_at: 2026-04-16T10:00:00Z"));
        assert!(rendered.contains("# t"));
        assert!(rendered.contains("## Body"));
        assert!(rendered.contains("## Comments"));
        assert!(rendered.contains("@bo"));
    }

    #[test]
    fn next_message_contract_names_exact_path() {
        let p = Path::new("/repo/.clud/loop/NEXT_MESSAGE.md");
        let text = next_message_contract(p);
        assert!(
            text.contains("    /repo/.clud/loop/NEXT_MESSAGE.md\n"),
            "{text}"
        );
        assert!(text.contains("Do not edit or delete it"));
    }

    #[test]
    fn done_contract_treats_merge_queue_enqueue_as_complete() {
        let text = done_marker_contract(Path::new("/r/DONE"), Path::new("/r/BLOCKED"));
        assert!(
            text.contains("accepted into the merge queue counts as complete"),
            "{text}"
        );
        assert!(text.contains("Do not wait for"), "{text}");
    }
}

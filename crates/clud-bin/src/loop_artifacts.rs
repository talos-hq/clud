//! Durable artifacts emitted under `<git-root>/.clud/loop/` during a
//! `clud loop` run. Ported from the Python implementation
//! (`src/clud/agent/loop_executor.py` and `loop_logger.py` on the
//! `python-legacy` branch). Issue #96.
//!
//! Responsibilities (all best-effort; an artifact failure must never
//! abort an iteration):
//!
//! - `info.json` — `TaskInfo` JSON tracking iteration count, start/end
//!   ISO-8601 timestamps, per-iteration return codes, completion status,
//!   and an optional error string. Updated at iteration start/end, on
//!   loop completion, and on interrupt.
//! - `log.txt` — append-mode UTF-8 file the runner appends iteration
//!   headers/footers and end-of-loop summaries to. The actual per-line
//!   piping of child output is deliberately left to the live console;
//!   this captures the loop driver's own narrative.
//! - `.gitignore` auto-injection — if `<git-root>/.gitignore` exists
//!   and does not yet contain `.clud/loop`, `.clud`, or `.clud/`, append
//!   `.clud/loop` and print a yellow warning to stderr.
//! - `motivation.md` — fixed prompt fragment written on iteration 2+.
//! - Loop-spec working copy — a literal string prompt is normalized into
//!   `<git-root>/.clud/loop/LOOP.md`; a user-supplied loop file is
//!   copied into `<git-root>/.clud/loop/<original-filename>`. Skipped if
//!   the destination already exists so user edits survive.
//!
//! Side effects are confined to `<git-root>/.clud/loop/` plus an
//! optional `.gitignore` append at `<git-root>/.gitignore`. The
//! DONE/BLOCKED marker contract and its directory location are
//! untouched.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::loop_spec::{ensure_loop_dir, loop_dir, TaskSpec};

const YELLOW: &str = "\x1b[93m";
const RESET: &str = "\x1b[0m";

const MOTIVATION_BODY: &str = "# Motivation\n\
\n\
You are continuing a multi-iteration task. Build on previous progress \
and stay focused on completing it. Do not start over. Re-read any \
artifacts written in earlier iterations before deciding what to do next.\n";

/// JSON shape persisted to `<git-root>/.clud/loop/info.json`. Kept
/// intentionally close to the python-legacy schema (subset) so a future
/// resume-from-crash implementation can read either format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskInfo {
    /// ISO-8601 UTC timestamp at which the loop driver started this
    /// session.
    pub start_time: String,
    /// ISO-8601 UTC timestamp at which the loop driver finished, or
    /// `None` while still running.
    #[serde(default)]
    pub end_time: Option<String>,
    /// Configured iteration budget for the run.
    pub total_iterations: u32,
    /// The number of the iteration currently in flight (1-indexed) or
    /// the last completed iteration if the loop has finished. `0` until
    /// the first iteration starts.
    #[serde(default)]
    pub current_iteration: u32,
    /// True once the loop has reached a terminal state (DONE, BLOCKED,
    /// iteration cap, or interrupt).
    #[serde(default)]
    pub completed: bool,
    /// Optional error message — e.g. "Interrupted by user" — written
    /// when the loop ends abnormally.
    #[serde(default)]
    pub error: Option<String>,
    /// Per-iteration audit trail.
    #[serde(default)]
    pub iterations: Vec<IterationInfo>,
}

/// One iteration's lifecycle record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IterationInfo {
    /// 1-indexed iteration number.
    pub iteration: u32,
    /// ISO-8601 UTC timestamp at iteration start.
    pub start_time: String,
    /// ISO-8601 UTC timestamp at iteration end (None if still running).
    #[serde(default)]
    pub end_time: Option<String>,
    /// Process exit code; only set once `end_iteration` runs.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Optional per-iteration error string.
    #[serde(default)]
    pub error: Option<String>,
}

impl TaskInfo {
    /// Fresh `TaskInfo` for a new loop session.
    pub fn new(total_iterations: u32) -> Self {
        Self {
            start_time: now_iso8601(),
            end_time: None,
            total_iterations,
            current_iteration: 0,
            completed: false,
            error: None,
            iterations: Vec::new(),
        }
    }

    /// Mark the start of `iteration` (1-indexed). Appends a new
    /// `IterationInfo` record.
    pub fn start_iteration(&mut self, iteration: u32) {
        self.current_iteration = iteration;
        self.iterations.push(IterationInfo {
            iteration,
            start_time: now_iso8601(),
            end_time: None,
            exit_code: None,
            error: None,
        });
    }

    /// Close out the most-recently-started iteration with `exit_code`
    /// and an optional `error` string.
    pub fn end_iteration(&mut self, exit_code: i32, error: Option<String>) {
        if let Some(last) = self.iterations.last_mut() {
            last.end_time = Some(now_iso8601());
            last.exit_code = Some(exit_code);
            last.error = error;
        }
    }

    /// Mark the whole loop as completed (success or terminal failure).
    pub fn mark_completed(&mut self, error: Option<String>) {
        self.completed = true;
        self.end_time = Some(now_iso8601());
        if error.is_some() {
            self.error = error;
        }
    }

    /// Persist to `<dir>/info.json`. Errors are non-fatal — caller
    /// decides whether to log.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Path to `info.json` under the loop dir.
    pub fn info_path(git_root: &Path) -> PathBuf {
        loop_dir(git_root).join("info.json")
    }
}

/// Path to `log.txt` under the loop dir.
pub fn log_path(git_root: &Path) -> PathBuf {
    loop_dir(git_root).join("log.txt")
}

/// Stateful wrapper used by `main.rs` to drive iteration-boundary
/// bookkeeping for a single `clud loop` run. Holds the git root plus a
/// `TaskInfo` accumulator that's flushed to `info.json` after each
/// state change. All methods are best-effort and never panic; IO
/// failures are silently swallowed (the loop must keep running).
pub struct LoopSession {
    git_root: PathBuf,
    info: TaskInfo,
    info_path: PathBuf,
}

impl LoopSession {
    /// Build a new session for a loop with `total_iterations` budget.
    /// Side-effects on construction:
    ///   - `ensure_loop_dir(git_root)` (so artifacts have somewhere to land)
    ///   - initial save of `info.json`
    ///   - `=== loop start ... ===` line appended to `log.txt`
    ///
    /// The caller is responsible for `ensure_loop_in_gitignore` and
    /// `materialize_working_copy` if desired — they're orthogonal to
    /// the per-iteration accounting tracked here.
    pub fn start(git_root: &Path, total_iterations: u32) -> Self {
        let _ = ensure_loop_dir(git_root);
        let info = TaskInfo::new(total_iterations);
        let info_path = TaskInfo::info_path(git_root);
        let session = Self {
            git_root: git_root.to_path_buf(),
            info,
            info_path,
        };
        let _ = session.info.save(&session.info_path);
        append_log_line(
            &session.git_root,
            &format!(
                "=== loop start {} total_iterations={} ===",
                now_iso8601(),
                total_iterations
            ),
        );
        session
    }

    /// Hook at the top of iteration `iteration` (1-indexed). Writes
    /// `motivation.md` on iter ≥ 2, updates info.json, and appends an
    /// iteration-start line to `log.txt`.
    pub fn on_iteration_start(&mut self, iteration: u32) {
        if iteration >= 2 {
            let _ = write_motivation_file(&self.git_root);
        }
        self.info.start_iteration(iteration);
        let _ = self.info.save(&self.info_path);
        log_iteration_start(&self.git_root, iteration);
        if let Ok(Some(n)) = drain_inbox(&self.git_root, iteration) {
            append_log_line(
                &self.git_root,
                &format!("=== iteration {iteration} delivered {n} queued message(s) ==="),
            );
        }
    }

    /// Hook after iteration `iteration` finishes with exit code `rc`.
    /// `error` is an optional one-line summary persisted into the
    /// per-iteration record (e.g. "Interrupted by user").
    pub fn on_iteration_end(&mut self, iteration: u32, rc: i32, error: Option<String>) {
        self.info.end_iteration(rc, error);
        let _ = self.info.save(&self.info_path);
        log_iteration_end(&self.git_root, iteration, rc);
    }

    /// Hook at the end of the loop. `summary` is a free-form one-line
    /// description ("DONE", "BLOCKED: ...", "iteration cap exhausted",
    /// "Interrupted by user", etc.). When `error` is `Some` it is also
    /// stored on the top-level info record.
    pub fn on_loop_end(&mut self, summary: &str, error: Option<String>) {
        self.info.mark_completed(error);
        let _ = self.info.save(&self.info_path);
        log_loop_end(&self.git_root, summary);
        let _ = write_root_summary(&self.git_root, &self.info, summary);
    }

    /// Borrow the current task info — exposed for tests that want to
    /// peek at iteration history without going through the JSON file.
    #[cfg(test)]
    pub fn info(&self) -> &TaskInfo {
        &self.info
    }
}

/// Path to the visible `LOOP_DONE.md` summary at the project root.
pub fn root_summary_path(git_root: &Path) -> PathBuf {
    git_root.join("LOOP_DONE.md")
}

/// Render the root `LOOP_DONE.md` body: outcome, an ASCII iteration
/// table, and pointers into the hidden `.clud/loop/` artifact dir.
pub fn render_root_summary(info: &TaskInfo, summary: &str) -> String {
    let mut out = String::new();
    out.push_str("# LOOP_DONE\n\n");
    out.push_str(&format!("- Outcome: {summary}\n"));
    out.push_str(&format!("- Started: {}\n", info.start_time));
    if let Some(end) = &info.end_time {
        out.push_str(&format!("- Ended: {end}\n"));
    }
    out.push_str(&format!(
        "- Iterations: {}/{}\n",
        info.iterations.len(),
        info.total_iterations
    ));
    if let Some(err) = &info.error {
        out.push_str(&format!("- Error: {err}\n"));
    }
    out.push_str("\n```\n+------+------+\n| iter |  rc  |\n+------+------+\n");
    for it in &info.iterations {
        let rc = it
            .exit_code
            .map_or_else(|| "-".to_string(), |c| c.to_string());
        out.push_str(&format!("| {:>4} | {:>4} |\n", it.iteration, rc));
    }
    out.push_str("+------+------+\n```\n\n");
    out.push_str("Artifacts (hidden directory):\n\n");
    for name in [
        "loop.json",
        "info.json",
        "log.txt",
        "LOOP.md",
        "NEXT_MESSAGE.md",
        "DONE",
        "BLOCKED",
    ] {
        out.push_str(&format!("- `.clud/loop/{name}`\n"));
    }
    out
}

/// Write `<git-root>/LOOP_DONE.md` so the loop result is visible without
/// opening the hidden `.clud/loop/` directory.
pub fn write_root_summary(git_root: &Path, info: &TaskInfo, summary: &str) -> std::io::Result<()> {
    std::fs::write(
        root_summary_path(git_root),
        render_root_summary(info, summary),
    )
}

/// Directory where operator messages for the next iteration are queued.
pub fn inbox_dir(git_root: &Path) -> PathBuf {
    loop_dir(git_root).join("inbox")
}

/// File the agent is told to read at the start of each iteration.
pub fn next_message_path(git_root: &Path) -> PathBuf {
    loop_dir(git_root).join("NEXT_MESSAGE.md")
}

/// Queue `text` for delivery at the start of the next iteration.
/// Returns the path of the queued file.
pub fn queue_message(git_root: &Path, text: &str) -> std::io::Result<PathBuf> {
    let dir = inbox_dir(git_root);
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut n = 0u32;
    loop {
        let path = dir.join(format!("{stamp:024}-{n:03}.md"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Move queued inbox messages into `NEXT_MESSAGE.md` for `iteration`,
/// archiving the consumed files under `inbox/delivered/`. With an empty
/// inbox, a stale `NEXT_MESSAGE.md` from an earlier iteration is removed
/// so messages are delivered exactly once. Returns how many messages were
/// delivered (`None` when the inbox was empty).
pub fn drain_inbox(git_root: &Path, iteration: u32) -> std::io::Result<Option<usize>> {
    let next = next_message_path(git_root);
    let dir = inbox_dir(git_root);
    let mut queued: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md"))
            .collect(),
        Err(_) => Vec::new(),
    };
    if queued.is_empty() {
        if next.exists() {
            std::fs::remove_file(&next)?;
        }
        return Ok(None);
    }
    queued.sort();
    let mut body = format!("# Operator messages for iteration {iteration}\n");
    for (i, path) in queued.iter().enumerate() {
        let text = std::fs::read_to_string(path)?;
        body.push_str(&format!("\n## Message {}\n\n{}\n", i + 1, text.trim_end()));
    }
    std::fs::write(&next, body)?;
    let delivered = dir.join("delivered");
    std::fs::create_dir_all(&delivered)?;
    for path in &queued {
        if let Some(name) = path.file_name() {
            std::fs::rename(path, delivered.join(name))?;
        }
    }
    Ok(Some(queued.len()))
}

/// Path to `motivation.md` under the loop dir.
pub fn motivation_path(git_root: &Path) -> PathBuf {
    loop_dir(git_root).join("motivation.md")
}

/// Append `line` (newline added) to `<git-root>/.clud/loop/log.txt`.
/// Best-effort; failure is silently ignored (the loop must never abort
/// on a log-write hiccup).
pub fn append_log_line(git_root: &Path, line: &str) {
    let _ = ensure_loop_dir(git_root);
    let path = log_path(git_root);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

/// Convenience: write the iteration-start header to log.txt.
pub fn log_iteration_start(git_root: &Path, iteration: u32) {
    append_log_line(
        git_root,
        &format!("=== iteration {iteration} start {} ===", now_iso8601()),
    );
}

/// Convenience: write the iteration-end footer to log.txt.
pub fn log_iteration_end(git_root: &Path, iteration: u32, rc: i32) {
    append_log_line(
        git_root,
        &format!(
            "=== iteration {iteration} end rc={rc} {} ===",
            now_iso8601()
        ),
    );
}

/// Convenience: write a one-line terminal-state summary at loop end.
pub fn log_loop_end(git_root: &Path, summary: &str) {
    append_log_line(
        git_root,
        &format!("=== loop end {} {summary} ===", now_iso8601()),
    );
}

/// Write the `motivation.md` snippet under the loop dir. Called from
/// iteration 2 onward. Safe to call repeatedly — overwrites with the
/// same content. Errors are non-fatal.
pub fn write_motivation_file(git_root: &Path) -> std::io::Result<()> {
    let _ = ensure_loop_dir(git_root);
    std::fs::write(motivation_path(git_root), MOTIVATION_BODY)
}

/// Materialize a working copy of the loop spec under the loop dir.
///
/// - `TaskSpec::Literal(s)` → write `s` to `<loop>/LOOP.md`
/// - `TaskSpec::File(p)`    → copy `p` to `<loop>/<filename>`
/// - any other variant      → no-op (GH cache files are handled elsewhere)
///
/// In both cases the destination is left alone if it already exists, so
/// edits made by the agent across iterations survive subsequent calls.
/// Returns the destination path on a successful materialize, `None`
/// otherwise (including the "already-present" case and any IO error).
pub fn materialize_working_copy(git_root: &Path, spec: &TaskSpec) -> Option<PathBuf> {
    let _ = ensure_loop_dir(git_root);
    match spec {
        TaskSpec::Literal(s) => {
            let dest = loop_dir(git_root).join("LOOP.md");
            if dest.exists() {
                return None;
            }
            std::fs::write(&dest, s).ok().map(|()| dest)
        }
        TaskSpec::File(p) => {
            let filename = p.file_name()?;
            let dest = loop_dir(git_root).join(filename);
            if dest.exists() {
                return None;
            }
            std::fs::copy(p, &dest).ok().map(|_| dest)
        }
        TaskSpec::GhIssue { .. } | TaskSpec::ShortForm(_) => None,
    }
}

/// Check `<git-root>/.gitignore` and append `.clud/loop` if it isn't
/// already covered by one of `{".clud/loop", ".clud", ".clud/"}`. Prints
/// a yellow warning to stderr when the append actually happens. No-op
/// (silent) when:
///
/// - `.gitignore` does not exist
/// - the file already contains a covering entry
/// - the file is unreadable or unwritable (permission error)
///
/// The narrow set of accepted ignore entries mirrors what the
/// python-legacy implementation matched on, plus the two `.clud`
/// prefixes that cover the loop dir transitively.
pub fn ensure_loop_in_gitignore(git_root: &Path) {
    let gitignore = git_root.join(".gitignore");
    let content = match std::fs::read_to_string(&gitignore) {
        Ok(s) => s,
        Err(_) => return,
    };

    if gitignore_covers_loop_dir(&content) {
        return;
    }

    let new = if content.is_empty() || content.ends_with('\n') {
        format!("{content}.clud/loop\n")
    } else {
        format!("{content}\n.clud/loop\n")
    };

    if std::fs::write(&gitignore, new).is_err() {
        // Permission errors are silent — the loop must not fail over
        // an unwritable .gitignore.
        return;
    }

    eprintln!("{YELLOW}Warning: .clud/loop was added to .gitignore{RESET}");
}

/// Pure check: does `content` already include a line that covers the
/// `.clud/loop/` directory? Exposed for unit tests; not part of the
/// public side-effecting API.
fn gitignore_covers_loop_dir(content: &str) -> bool {
    for raw in content.lines() {
        let line = raw.trim();
        if matches!(
            line,
            ".clud/loop"
                | "./.clud/loop"
                | "/.clud/loop"
                | ".clud/loop/"
                | ".clud"
                | "./.clud"
                | "/.clud"
                | ".clud/"
        ) {
            return true;
        }
    }
    false
}

/// ISO-8601 UTC timestamp via `SystemTime` — avoids adding a chrono dep.
/// The calendar arithmetic lives in `crate::civil_time` (#1206); this
/// function owns only the format string, which is already baked into
/// persisted `info.json` and `log.txt` and must stay byte-for-byte the
/// same.
fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, se) = crate::civil_time::civil_from_unix_secs(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ---- TaskInfo ----

    #[test]
    fn task_info_roundtrip_through_json() {
        let mut info = TaskInfo::new(10);
        info.start_iteration(1);
        info.end_iteration(0, None);
        info.start_iteration(2);
        info.end_iteration(2, Some("blocked".to_string()));
        info.mark_completed(None);

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("info.json");
        info.save(&path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let parsed: TaskInfo = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, info);
        assert_eq!(parsed.iterations.len(), 2);
        assert_eq!(parsed.iterations[0].exit_code, Some(0));
        assert_eq!(parsed.iterations[1].exit_code, Some(2));
        assert_eq!(parsed.iterations[1].error.as_deref(), Some("blocked"));
        assert!(parsed.completed);
        assert!(parsed.end_time.is_some());
    }

    #[test]
    fn task_info_start_iteration_bumps_current() {
        let mut info = TaskInfo::new(3);
        assert_eq!(info.current_iteration, 0);
        info.start_iteration(1);
        assert_eq!(info.current_iteration, 1);
        info.start_iteration(2);
        assert_eq!(info.current_iteration, 2);
        assert_eq!(info.iterations.len(), 2);
    }

    #[test]
    fn task_info_end_iteration_records_on_last() {
        let mut info = TaskInfo::new(3);
        info.start_iteration(1);
        info.end_iteration(7, Some("oops".into()));
        assert_eq!(info.iterations[0].exit_code, Some(7));
        assert_eq!(info.iterations[0].error.as_deref(), Some("oops"));
        assert!(info.iterations[0].end_time.is_some());
    }

    #[test]
    fn task_info_end_iteration_noop_without_started_iter() {
        let mut info = TaskInfo::new(3);
        // No start_iteration → end_iteration must not panic.
        info.end_iteration(0, None);
        assert!(info.iterations.is_empty());
    }

    #[test]
    fn task_info_mark_completed_sets_error_when_supplied() {
        let mut info = TaskInfo::new(1);
        info.mark_completed(Some("Interrupted by user".into()));
        assert!(info.completed);
        assert_eq!(info.error.as_deref(), Some("Interrupted by user"));
        assert!(info.end_time.is_some());
    }

    // ---- gitignore injection ----

    #[test]
    fn gitignore_already_has_loop_entry_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let gi = tmp.path().join(".gitignore");
        let original = "target/\n.clud/loop\nfoo.txt\n";
        fs::write(&gi, original).unwrap();

        ensure_loop_in_gitignore(tmp.path());

        let after = fs::read_to_string(&gi).unwrap();
        assert_eq!(after, original);
    }

    #[test]
    fn gitignore_clud_dir_covers_loop_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let gi = tmp.path().join(".gitignore");
        let original = ".clud\n";
        fs::write(&gi, original).unwrap();

        ensure_loop_in_gitignore(tmp.path());
        assert_eq!(fs::read_to_string(&gi).unwrap(), original);
    }

    #[test]
    fn gitignore_clud_slash_covers_loop_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let gi = tmp.path().join(".gitignore");
        let original = ".clud/\n";
        fs::write(&gi, original).unwrap();

        ensure_loop_in_gitignore(tmp.path());
        assert_eq!(fs::read_to_string(&gi).unwrap(), original);
    }

    #[test]
    fn gitignore_appends_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let gi = tmp.path().join(".gitignore");
        fs::write(&gi, "target/\nfoo.txt\n").unwrap();

        ensure_loop_in_gitignore(tmp.path());

        let after = fs::read_to_string(&gi).unwrap();
        assert!(after.contains(".clud/loop"));
        assert!(after.ends_with(".clud/loop\n"));
    }

    #[test]
    fn gitignore_appends_with_newline_when_no_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let gi = tmp.path().join(".gitignore");
        fs::write(&gi, "target/").unwrap();

        ensure_loop_in_gitignore(tmp.path());

        let after = fs::read_to_string(&gi).unwrap();
        assert_eq!(after, "target/\n.clud/loop\n");
    }

    #[test]
    fn gitignore_no_file_is_silent_noop() {
        let tmp = tempfile::tempdir().unwrap();
        // No .gitignore — must not create one and must not panic.
        ensure_loop_in_gitignore(tmp.path());
        assert!(!tmp.path().join(".gitignore").exists());
    }

    #[test]
    fn gitignore_covers_loop_dir_recognizes_all_variants() {
        for v in &[
            ".clud/loop",
            "./.clud/loop",
            "/.clud/loop",
            ".clud/loop/",
            ".clud",
            "./.clud",
            "/.clud",
            ".clud/",
        ] {
            let body = format!("foo\n{v}\nbar\n");
            assert!(
                gitignore_covers_loop_dir(&body),
                "expected `{v}` to be recognized as covering .clud/loop"
            );
        }
    }

    #[test]
    fn gitignore_covers_loop_dir_rejects_unrelated_entries() {
        let body = "foo\n.clud-other\nclud\n.cludloop\n";
        assert!(!gitignore_covers_loop_dir(body));
    }

    // ---- motivation ----

    #[test]
    fn write_motivation_file_creates_dir_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_motivation_file(tmp.path()).unwrap();
        let content = fs::read_to_string(motivation_path(tmp.path())).unwrap();
        assert!(content.contains("multi-iteration"));
        assert!(content.starts_with("# Motivation"));
    }

    // ---- working copy ----

    #[test]
    fn working_copy_literal_writes_loop_md() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = TaskSpec::Literal("do the thing".to_string());
        let dest = materialize_working_copy(tmp.path(), &spec).unwrap();
        assert_eq!(dest, loop_dir(tmp.path()).join("LOOP.md"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "do the thing");
    }

    #[test]
    fn working_copy_literal_skips_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_loop_dir(tmp.path()).unwrap();
        let dest = loop_dir(tmp.path()).join("LOOP.md");
        fs::write(&dest, "existing").unwrap();

        let spec = TaskSpec::Literal("new content".to_string());
        let result = materialize_working_copy(tmp.path(), &spec);
        assert!(result.is_none(), "expected skip when LOOP.md exists");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "existing");
    }

    #[test]
    fn working_copy_file_copies_to_loop_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("task.md");
        fs::write(&src, "task body").unwrap();

        let spec = TaskSpec::File(src.clone());
        let dest = materialize_working_copy(tmp.path(), &spec).unwrap();
        assert_eq!(dest, loop_dir(tmp.path()).join("task.md"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "task body");
    }

    #[test]
    fn working_copy_file_skips_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_loop_dir(tmp.path()).unwrap();
        let src = tmp.path().join("task.md");
        fs::write(&src, "fresh").unwrap();
        let preexisting = loop_dir(tmp.path()).join("task.md");
        fs::write(&preexisting, "existing").unwrap();

        let spec = TaskSpec::File(src.clone());
        let result = materialize_working_copy(tmp.path(), &spec);
        assert!(result.is_none());
        assert_eq!(fs::read_to_string(&preexisting).unwrap(), "existing");
    }

    #[test]
    fn working_copy_ghissue_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = TaskSpec::GhIssue {
            owner: "a".into(),
            repo: "b".into(),
            kind: crate::loop_spec::GhKind::Issue,
            number: 42,
        };
        assert!(materialize_working_copy(tmp.path(), &spec).is_none());
        // Loop dir is created by `ensure_loop_dir`, but no LOOP.md
        // should appear.
        assert!(!loop_dir(tmp.path()).join("LOOP.md").exists());
    }

    // ---- log + iteration helpers ----

    #[test]
    fn log_iteration_helpers_append_to_log_txt() {
        let tmp = tempfile::tempdir().unwrap();
        log_iteration_start(tmp.path(), 1);
        log_iteration_end(tmp.path(), 1, 0);
        log_iteration_start(tmp.path(), 2);
        log_loop_end(tmp.path(), "DONE");

        let body = fs::read_to_string(log_path(tmp.path())).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4, "got: {body:?}");
        assert!(lines[0].contains("iteration 1 start"));
        assert!(lines[1].contains("iteration 1 end rc=0"));
        assert!(lines[2].contains("iteration 2 start"));
        assert!(lines[3].contains("loop end") && lines[3].contains("DONE"));
    }

    #[test]
    fn append_log_line_creates_dir_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Loop dir does not exist yet — helper must create it.
        assert!(!loop_dir(tmp.path()).exists());
        append_log_line(tmp.path(), "first line");
        assert!(log_path(tmp.path()).is_file());
        let body = fs::read_to_string(log_path(tmp.path())).unwrap();
        assert_eq!(body, "first line\n");
    }

    // ---- LoopSession driver ----

    #[test]
    fn loop_session_start_writes_info_and_log() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = LoopSession::start(tmp.path(), 5);

        let info: TaskInfo =
            serde_json::from_str(&fs::read_to_string(TaskInfo::info_path(tmp.path())).unwrap())
                .unwrap();
        assert_eq!(info.total_iterations, 5);
        assert_eq!(info.current_iteration, 0);
        assert!(!info.completed);

        let log = fs::read_to_string(log_path(tmp.path())).unwrap();
        assert!(log.contains("loop start"));
        assert!(log.contains("total_iterations=5"));
    }

    #[test]
    fn loop_session_iteration_lifecycle_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = LoopSession::start(tmp.path(), 3);
        s.on_iteration_start(1);
        s.on_iteration_end(1, 0, None);
        s.on_iteration_start(2);
        s.on_iteration_end(2, 1, Some("nonzero".into()));
        s.on_loop_end("BLOCKED", Some("Interrupted by user".into()));
        let root = std::fs::read_to_string(root_summary_path(tmp.path())).unwrap();
        assert!(root.contains("Outcome: BLOCKED"), "{root}");
        assert!(root.contains(".clud/loop/info.json"), "{root}");

        let info: TaskInfo =
            serde_json::from_str(&fs::read_to_string(TaskInfo::info_path(tmp.path())).unwrap())
                .unwrap();
        assert_eq!(info.iterations.len(), 2);
        assert_eq!(info.iterations[0].exit_code, Some(0));
        assert_eq!(info.iterations[1].exit_code, Some(1));
        assert_eq!(info.iterations[1].error.as_deref(), Some("nonzero"));
        assert!(info.completed);
        assert_eq!(info.error.as_deref(), Some("Interrupted by user"));

        let log = fs::read_to_string(log_path(tmp.path())).unwrap();
        assert!(log.contains("iteration 1 start"));
        assert!(log.contains("iteration 1 end rc=0"));
        assert!(log.contains("iteration 2 start"));
        assert!(log.contains("iteration 2 end rc=1"));
        assert!(log.contains("loop end") && log.contains("BLOCKED"));
    }

    #[test]
    fn loop_session_skips_motivation_on_iter_1() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = LoopSession::start(tmp.path(), 3);
        s.on_iteration_start(1);
        assert!(!motivation_path(tmp.path()).exists());
        s.on_iteration_end(1, 0, None);

        s.on_iteration_start(2);
        assert!(motivation_path(tmp.path()).exists());
    }

    // ---- timestamp sanity ----

    #[test]
    fn now_iso8601_has_expected_shape() {
        let ts = now_iso8601();
        // YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "got: {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    #[test]
    fn queued_messages_are_delivered_once_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert_eq!(drain_inbox(root, 1).unwrap(), None);
        queue_message(root, "first").unwrap();
        queue_message(root, "second\n").unwrap();
        assert_eq!(drain_inbox(root, 2).unwrap(), Some(2));
        let body = fs::read_to_string(next_message_path(root)).unwrap();
        assert!(
            body.starts_with("# Operator messages for iteration 2"),
            "{body}"
        );
        let (a, b) = (body.find("first").unwrap(), body.find("second").unwrap());
        assert!(a < b, "{body}");
        assert_eq!(
            fs::read_dir(inbox_dir(root).join("delivered"))
                .unwrap()
                .count(),
            2
        );
        // Next iteration with nothing queued clears the stale file.
        assert_eq!(drain_inbox(root, 3).unwrap(), None);
        assert!(!next_message_path(root).exists());
    }

    #[test]
    fn loop_session_delivers_inbox_at_iteration_start() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = LoopSession::start(tmp.path(), 3);
        queue_message(tmp.path(), "focus on tests").unwrap();
        s.on_iteration_start(1);
        let body = fs::read_to_string(next_message_path(tmp.path())).unwrap();
        assert!(body.contains("focus on tests"));
        let log = fs::read_to_string(log_path(tmp.path())).unwrap();
        assert!(log.contains("delivered 1 queued message(s)"), "{log}");
    }
}

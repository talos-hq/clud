//! Startup-time nudge: warn when the project contains source files large
//! enough to cause AI agents to get stuck. See zackees/clud#132.
//!
//! On launch we walk the project's git root, honoring `.gitignore`,
//! `.ignore`, hidden-file rules, and global git excludes via the `ignore`
//! crate's parallel walker. Files whose size is ≥ [`SIZE_THRESHOLD`] and
//! whose extension is on a small whitelist of source-code languages are
//! reported to stderr — top [`REPORT_LIMIT`] by size, with a `(N more)`
//! tail when more qualify.
//!
//! The walk hard-stops at [`DEADLINE`] (1 s wall) regardless of progress
//! so that startup is never blocked by a pathological repo: if the
//! deadline trips, stderr reports a partial-scan note instead of stalling.

use ignore::{DirEntry, WalkBuilder, WalkState};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Issue #556: pass 1 — the in-process git-index reader (fast path for tracked
/// files). Wired into [`run`] ahead of the walker for git repos.
#[cfg(test)]
mod fallback_tests;
mod index_pass;
mod ls_files_pass;

/// 40 kB ≈ 1000 LOC at clud's measured ~37 bytes/line (see issue #132).
///
/// Doubles as the yellow tier's floor (issue #557): anything worth listing is
/// worth colouring, so there is no uncoloured band.
pub const SIZE_THRESHOLD: u64 = 40 * 1024;

/// At or above this a file is "very degenerate" (issue #557) and renders red
/// rather than yellow. Inclusive — 300 kB exactly is red — chosen for
/// predictability and pinned by test.
pub const SEVERE_SIZE_THRESHOLD: u64 = 300 * 1024;

/// Whether [`format_report`] emits ANSI colour.
///
/// A parameter rather than ambient state, so the byte-exact report tests (the
/// issue #515 regression guard) stay deterministic and keep asserting plain
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Plain,
    Ansi,
}

impl ColorMode {
    /// Colour only when stderr is a terminal and `NO_COLOR` is unset.
    ///
    /// Separated from the environment probe so the decision is testable
    /// without a terminal.
    pub fn decide(stderr_is_terminal: bool, no_color_set: bool) -> Self {
        if stderr_is_terminal && !no_color_set {
            Self::Ansi
        } else {
            Self::Plain
        }
    }

    fn for_stderr() -> Self {
        use std::io::IsTerminal;
        Self::decide(
            std::io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        )
    }

    /// Escape pair wrapping one entry, empty in [`ColorMode::Plain`].
    fn entry_wrap(self, size: u64) -> (&'static str, &'static str) {
        match self {
            ColorMode::Plain => ("", ""),
            ColorMode::Ansi if size >= SEVERE_SIZE_THRESHOLD => (ANSI_RED, ANSI_RESET),
            ColorMode::Ansi => (ANSI_YELLOW, ANSI_RESET),
        }
    }
}

const ANSI_RED: &str = "\u{1b}[31m";
const ANSI_YELLOW: &str = "\u{1b}[33m";
const ANSI_RESET: &str = "\u{1b}[0m";

/// Hard wall-clock deadline for the entire walk. Per user requirement
/// the check stops at 1 s even if files remain unvisited.
pub const DEADLINE: Duration = Duration::from_secs(1);

/// Top-N largest files reported.
pub const REPORT_LIMIT: usize = 4;

/// Source-file extensions worth checking (no leading dot).
const SOURCE_EXTS: &[&str] = &[
    "rs", "py", "js", "jsx", "ts", "tsx", "mjs", "cjs", "go", "c", "cc", "cpp", "cxx", "h", "hpp",
    "hxx", "java", "kt", "scala", "rb", "swift", "cs", "php", "lua", "m", "mm", "r", "jl", "ex",
    "exs", "erl", "clj", "cljs", "ml", "fs", "vb", "dart", "sh", "ps1",
];

/// Conventional directory names that hold third-party / vendored source we
/// never want to flag. Most are also gitignored in practice, but some repos
/// commit them (cargo vendor, go vendor, git-subrepo trees), so pruning by
/// name is a belt-and-suspenders complement to gitignore + nested-`.git`.
const VENDOR_DIRS: &[&str] = &[
    "vendor",
    "third_party",
    "third-party",
    "external",
    "deps",
    "subprojects",
    "node_modules",
];

/// Conventional directory names holding generated build output (issue #846).
///
/// Distinct from [`VENDOR_DIRS`]: that list is third-party source we did not
/// write, whereas a bundle is *our* source after a bundler ate it. Same
/// treatment — unrefactorable, so never worth flagging — for a different
/// reason, so the lists stay separate rather than one growing a second meaning.
///
/// Normally build output is gitignored and never reaches us. A JS GitHub
/// Action breaks that assumption structurally: the platform loads the action
/// from a committed bundle, so `dist/` is tracked and multiple MB of `ncc`
/// output otherwise dominates a report whose whole point is hand-written code.
///
/// Deliberately narrow. `build`, `out`, and `target` are *not* here: `build/`
/// in particular holds hand-written scripts in some repos, and pruning a name
/// means never warning about anything beneath it again. That trade wants its
/// own decision rather than riding along with the unambiguous case.
const BUILD_OUTPUT_DIRS: &[&str] = &["dist"];

/// Whether a single directory name should be pruned from the scan.
///
/// One leading dot is ignored so `.dist` needs no separate entry. The walker
/// already skips hidden directories via `standard_filters`, but the index pass
/// (the fast path, and the one that runs first on git repos) has no
/// hidden-file check at all — a tracked `.dist/main.js` reaches
/// `classify_entries` directly, so the dot handling is load-bearing there.
fn is_pruned_dir_name(name: &str) -> bool {
    let base = name.strip_prefix('.').unwrap_or(name);
    VENDOR_DIRS.contains(&base) || BUILD_OUTPUT_DIRS.contains(&base)
}

/// A single file that crossed the size threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFile {
    pub rel_path: PathBuf,
    pub size: u64,
}

/// Run the startup check from `project_root`. Silent on non-git roots and
/// when no files qualify. Errors are swallowed — a failed scan must never
/// block a launch.
pub fn run(project_root: &Path) {
    // Silent if not in a git repo (the warning is project-scoped).
    if !project_root.join(".git").exists() {
        return;
    }

    // Issue #556: prefer the in-process index pass (tracked files + cached
    // sizes, ~1–5 ms, no tree walk). Fall back to the full parallel walker only
    // when there is no usable index (missing/corrupt), so behavior never
    // regresses on non-standard or damaged repos.
    let report = match index_pass::index_pass(project_root) {
        Ok(output) => report_from_index_pass(project_root, output),
        // #556: a *corrupt* index is not the same as no index. The escape
        // hatch is one killable `git ls-files --debug`, which still reads the
        // index's cached sizes and still touches no object database — falling
        // straight to the full tree walk here would give up the entire win on
        // exactly the repos most likely to be large. Only when that also comes
        // up empty do we walk.
        Err(index_pass::IndexPassError::Parse) => {
            match ls_files_pass::ls_files_pass(project_root) {
                Some(entries) => report_from_index_pass(
                    project_root,
                    index_pass::classify_entries(entries.into_iter()),
                ),
                None => collect(project_root, DEADLINE),
            }
        }
        Err(index_pass::IndexPassError::NoIndex) => collect(project_root, DEADLINE),
    };
    emit(&report.files, report.total_qualifying, report.timed_out);
}

/// Build the startup report from the index pass. Tracked files already over the
/// threshold come straight from the index's cached sizes; the few entries whose
/// cached size is untrustworthy (racily-clean, recorded as 0) are re-measured
/// with a single targeted `stat` each — still no tree walk. Untracked files are
/// not covered on the launch path; the daemon's background pass 2 (#551) owns
/// that, consistent with the "crude, fast, not comprehensive" design.
fn report_from_index_pass(root: &Path, output: index_pass::IndexPassOutput) -> Report {
    let mut files = output.qualifying;
    for rel in output.needs_verification {
        if let Ok(metadata) = std::fs::metadata(root.join(&rel)) {
            if metadata.len() >= SIZE_THRESHOLD {
                files.push(LargeFile {
                    rel_path: rel,
                    size: metadata.len(),
                });
            }
        }
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.size));
    let total_qualifying = files.len();
    files.truncate(REPORT_LIMIT);
    Report {
        files,
        total_qualifying,
        // The index pass has no wall-clock walk to time out; verification stats
        // are a bounded, tiny set.
        timed_out: false,
    }
}

struct Report {
    files: Vec<LargeFile>,
    total_qualifying: usize,
    timed_out: bool,
}

fn collect(root: &Path, deadline: Duration) -> Report {
    let start = Instant::now();
    let stop = AtomicBool::new(false);
    let hits: Mutex<Vec<LargeFile>> = Mutex::new(Vec::new());

    let walker = WalkBuilder::new(root)
        .standard_filters(true) // .gitignore + .ignore + hidden
        .same_file_system(true)
        .filter_entry({
            let root = root.to_path_buf();
            move |e| !is_pruned_nested_git(&root, e)
        })
        .build_parallel();

    walker.run(|| {
        let stop = &stop;
        let hits = &hits;
        let root = root.to_path_buf();
        Box::new(move |result| {
            if start.elapsed() >= deadline {
                stop.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            if stop.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            if !is_whitelisted_source(path) {
                return WalkState::Continue;
            }
            let Ok(md) = entry.metadata() else {
                return WalkState::Continue;
            };
            if md.len() < SIZE_THRESHOLD {
                return WalkState::Continue;
            }
            let rel = path.strip_prefix(&root).unwrap_or(path).to_path_buf();
            hits.lock().unwrap().push(LargeFile {
                rel_path: rel,
                size: md.len(),
            });
            WalkState::Continue
        })
    });

    let timed_out = start.elapsed() >= deadline;
    let mut files = hits.into_inner().unwrap();
    files.sort_by_key(|file| std::cmp::Reverse(file.size));
    let total = files.len();
    files.truncate(REPORT_LIMIT);
    Report {
        files,
        total_qualifying: total,
        timed_out,
    }
}

fn is_whitelisted_source(path: &Path) -> bool {
    // Exclude *.min.* bundles (multi-component check, not a single extension).
    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return false,
    };
    if name.contains(".min.") {
        return false;
    }
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    SOURCE_EXTS.contains(&ext.as_str())
}

fn is_pruned_nested_git(root: &Path, e: &DirEntry) -> bool {
    if !e.file_type().is_some_and(|t| t.is_dir()) {
        return false;
    }
    if e.path() == root {
        return false;
    }
    // Pruned if the directory has its own `.git` (vendored sub-repo).
    if e.path().join(".git").exists() {
        return true;
    }
    // Pruned if the directory's basename is a conventional vendor / deps
    // directory, or generated build output. Repos that commit `cargo vendor` /
    // `go vendor` trees, or a bundled action's `dist/`, would otherwise
    // dominate the report with code nobody can refactor.
    if let Some(name) = e.path().file_name().and_then(|s| s.to_str()) {
        if is_pruned_dir_name(name) {
            return true;
        }
    }
    false
}

/// Write the warning to stderr. Issue #515: the header and every file entry
/// are composed into a single buffer and emitted with one locked, flushed
/// `write_all`, rather than a sequence of `eprintln!` calls. On Windows,
/// `clud --codex` launches the backend through a ConPTY that repaints the
/// viewport when the child attaches; detail lines emitted as separate writes
/// just before takeover were being wiped, leaving only the header. One
/// flushed block commits the whole warning up front so the entries survive —
/// and matches the content `--dry-run` prints (which never starts a PTY).
fn emit(files: &[LargeFile], total: usize, timed_out: bool) {
    // Issue #557: colour is inline bytes inside the same composed string, so
    // the single-write guarantee above is unaffected.
    let Some(message) = format_report(files, total, timed_out, ColorMode::for_stderr()) else {
        return;
    };
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = handle.write_all(message.as_bytes());
    let _ = handle.flush();
}

/// Compose the complete warning block (header + every file entry + optional
/// `(N more)` tail and partial-scan note) as one string, or `None` when there
/// is nothing to report. Pure so the exact bytes emitted at startup can be
/// asserted in tests — the regression guard for issue #515, where the entries
/// must never be dropped from or split out of the emitted warning.
///
/// Issue #557: entries carry a severity tier — red at or above
/// [`SEVERE_SIZE_THRESHOLD`], yellow for everything else listed. The header
/// and `(N more)` tail stay uncoloured so the block reads as a list with
/// emphasis rather than a wall of colour.
fn format_report(
    files: &[LargeFile],
    total: usize,
    timed_out: bool,
    color: ColorMode,
) -> Option<String> {
    if files.is_empty() {
        if timed_out {
            return Some(
                "[clud] note: large-file scan exceeded 1s budget — skipping\n".to_string(),
            );
        }
        return None;
    }
    let mut out = String::new();
    out.push_str(
        "[clud] warning: large source files (\u{2265}40 kB) detected — AI may get stuck on these; recommend refactoring:\n",
    );
    for f in files {
        let (open, close) = color.entry_wrap(f.size);
        out.push_str(&format!(
            "  {open}{} ({}){close}\n",
            f.rel_path.display(),
            human_kb(f.size)
        ));
    }
    let extra = total.saturating_sub(files.len());
    if extra > 0 {
        out.push_str(&format!("  ({extra} more)\n"));
    }
    if timed_out {
        out.push_str("[clud] note: scan stopped at 1s — report may be partial\n");
    }
    Some(out)
}

fn human_kb(bytes: u64) -> String {
    let kb = (bytes as f64) / 1024.0;
    if kb >= 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{} kB", kb.round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;
    use std::fs;

    /// Helper: make a fixture dir with a `.git` marker so `run()` won't bail.
    fn fixture() -> TemporaryDirectory {
        let dir = TemporaryDirectory::new().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        dir
    }

    fn write_file(dir: &Path, rel: &str, size: usize) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        // Write `size` bytes of ASCII content. Use 'a' so the file is a
        // valid source-text payload for any tool that might inspect it.
        let payload = vec![b'a'; size];
        fs::write(path, payload).unwrap();
    }

    #[test]
    fn threshold_constant_is_documented() {
        // Sanity-check that the public constant matches the issue spec —
        // future drift will flag here before the doc comment ages out.
        assert_eq!(SIZE_THRESHOLD, 40 * 1024);
    }

    #[test]
    fn whitelist_excludes_json() {
        let dir = fixture();
        write_file(dir.path(), "big.json", 200 * 1024);
        let report = collect(dir.path(), DEADLINE);
        assert!(report.files.is_empty());
        assert_eq!(report.total_qualifying, 0);
    }

    #[test]
    fn whitelist_excludes_min_files() {
        let dir = fixture();
        write_file(dir.path(), "bundle.min.js", 200 * 1024);
        let report = collect(dir.path(), DEADLINE);
        assert!(report.files.is_empty());
        assert_eq!(report.total_qualifying, 0);
    }

    #[test]
    fn whitelist_includes_known_extensions() {
        let dir = fixture();
        for ext in &["rs", "py", "ts", "go", "cpp"] {
            write_file(dir.path(), &format!("file.{ext}"), 50 * 1024);
        }
        let report = collect(dir.path(), DEADLINE);
        assert_eq!(report.total_qualifying, 5);
        // truncated to REPORT_LIMIT
        assert_eq!(report.files.len(), REPORT_LIMIT);
    }

    #[test]
    fn under_threshold_silent() {
        let dir = fixture();
        write_file(dir.path(), "small.rs", 10 * 1024);
        let report = collect(dir.path(), DEADLINE);
        assert!(report.files.is_empty());
        assert_eq!(report.total_qualifying, 0);
    }

    #[test]
    fn gitignore_pruning() {
        let dir = fixture();
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        write_file(dir.path(), "target/foo.rs", 100 * 1024);
        let report = collect(dir.path(), DEADLINE);
        assert!(report.files.is_empty(), "got: {:?}", report.files);
        assert_eq!(report.total_qualifying, 0);
    }

    #[test]
    fn nested_git_pruned() {
        let dir = fixture();
        fs::create_dir_all(dir.path().join("vendored-dep/.git")).unwrap();
        write_file(dir.path(), "vendored-dep/huge.rs", 100 * 1024);
        let report = collect(dir.path(), DEADLINE);
        assert!(report.files.is_empty(), "got: {:?}", report.files);
        assert_eq!(report.total_qualifying, 0);
    }

    #[test]
    fn vendor_dirs_pruned_by_name() {
        // No nested `.git` here — pruning is purely by directory name.
        // Mirrors the real-world case in zackees/clud where `vendor/...`
        // contains committed C++ third-party source without its own .git.
        let dir = fixture();
        for d in &[
            "vendor",
            "third_party",
            "third-party",
            "external",
            "deps",
            "subprojects",
            "node_modules",
        ] {
            write_file(dir.path(), &format!("{d}/big.cpp"), 100 * 1024);
        }
        let report = collect(dir.path(), DEADLINE);
        assert!(report.files.is_empty(), "got: {:?}", report.files);
        assert_eq!(report.total_qualifying, 0);
    }

    /// Issue #846: the walker half of the dist prune. Mirrors the reported
    /// case in zackees/setup-soldr, where a bundled GitHub Action commits
    /// `dist/main.js` and `cook/dist/main.js` because the platform loads the
    /// action from the bundle — so gitignore never hides them.
    #[test]
    fn dist_dirs_pruned_by_name() {
        let dir = fixture();
        for f in &["dist/main.js", "cook/dist/main.js", "a/b/dist/deep.js"] {
            write_file(dir.path(), f, 100 * 1024);
        }
        let report = collect(dir.path(), DEADLINE);
        assert!(report.files.is_empty(), "got: {:?}", report.files);
        assert_eq!(report.total_qualifying, 0);
    }

    /// Names that merely resemble `dist` are ordinary source. Without this the
    /// prune could silently widen and suppress the findings it exists to show.
    #[test]
    fn dist_lookalike_dirs_are_not_pruned() {
        let dir = fixture();
        for f in &["distributed/node.rs", "src/dist_helper.js"] {
            write_file(dir.path(), f, 100 * 1024);
        }
        let report = collect(dir.path(), DEADLINE);
        assert_eq!(report.total_qualifying, 2, "got: {:?}", report.files);
    }

    #[test]
    fn root_git_does_not_prune_self() {
        let dir = fixture();
        write_file(dir.path(), "src.rs", 50 * 1024);
        let report = collect(dir.path(), DEADLINE);
        assert_eq!(report.total_qualifying, 1);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].rel_path, PathBuf::from("src.rs"));
    }

    #[test]
    fn report_caps_at_four() {
        let dir = fixture();
        for i in 0..10 {
            write_file(dir.path(), &format!("f{i}.rs"), 100 * 1024);
        }
        let report = collect(dir.path(), DEADLINE);
        assert_eq!(report.files.len(), 4);
        assert_eq!(report.total_qualifying, 10);
    }

    #[test]
    fn report_sorted_descending() {
        let dir = fixture();
        write_file(dir.path(), "a.rs", 120 * 1024);
        write_file(dir.path(), "b.rs", 60 * 1024);
        write_file(dir.path(), "c.rs", 80 * 1024);
        let report = collect(dir.path(), DEADLINE);
        assert_eq!(report.files.len(), 3);
        let sizes: Vec<u64> = report.files.iter().map(|f| f.size).collect();
        assert_eq!(sizes, vec![120 * 1024, 80 * 1024, 60 * 1024]);
    }

    #[test]
    fn report_sizes_human_readable() {
        assert_eq!(human_kb(112 * 1024), "112 kB");
        assert_eq!(human_kb(2 * 1024 * 1024), "2.0 MB");
    }

    // --- Issue #515: the emitted warning must carry every file entry as one
    // atomic block, identical to what `--dry-run` shows. `format_report` is the
    // single source both the normal-startup and dry-run paths format through,
    // so asserting its output guards against entries being dropped or split.

    fn lf(path: &str, size: u64) -> LargeFile {
        LargeFile {
            rel_path: PathBuf::from(path),
            size,
        }
    }

    #[test]
    fn format_report_includes_header_and_every_entry() {
        let files = vec![
            lf("crates/running-process/src/lib.rs", 52 * 1024),
            lf("crates/running-process/src/daemon/services.rs", 50 * 1024),
        ];
        let out =
            format_report(&files, files.len(), false, ColorMode::Plain).expect("should report");
        // One atomic block: header + both entries, each on its own line.
        assert!(
            out.contains("large source files"),
            "missing header: {out:?}"
        );
        for f in &files {
            let line = format!("  {} ({})", f.rel_path.display(), human_kb(f.size));
            assert!(out.contains(&line), "missing entry {line:?} in {out:?}");
        }
        // Entries are not dropped: header line + 2 entry lines.
        assert_eq!(out.lines().count(), 3, "unexpected line count: {out:?}");
        assert!(out.ends_with('\n'), "block must end with a newline");
    }

    // Issue #557: severity tiers. Asserted as exact bytes, because the point
    // of the feature is the escape sequences a terminal receives.

    #[test]
    fn ansi_mode_paints_severe_entries_red_and_the_rest_yellow() {
        let files = vec![lf("huge.rs", 320 * 1024), lf("big.rs", 45 * 1024)];
        let out =
            format_report(&files, files.len(), false, ColorMode::Ansi).expect("should report");
        assert!(
            out.contains("\u{1b}[31mhuge.rs (320 kB)\u{1b}[0m"),
            "320 kB must be red: {out:?}"
        );
        assert!(
            out.contains("\u{1b}[33mbig.rs (45 kB)\u{1b}[0m"),
            "45 kB must be yellow: {out:?}"
        );
    }

    #[test]
    fn the_red_boundary_is_inclusive_at_300_kb() {
        // Pinned deliberately: "300 kB exactly" is the kind of edge that
        // silently drifts when someone rewrites the comparison.
        let at = vec![lf("at.rs", SEVERE_SIZE_THRESHOLD)];
        let below = vec![lf("below.rs", SEVERE_SIZE_THRESHOLD - 1)];
        let at_out = format_report(&at, 1, false, ColorMode::Ansi).expect("report");
        let below_out = format_report(&below, 1, false, ColorMode::Ansi).expect("report");
        assert!(
            at_out.contains(ANSI_RED),
            "300 kB exactly must be red: {at_out:?}"
        );
        assert!(
            below_out.contains(ANSI_YELLOW) && !below_out.contains(ANSI_RED),
            "one byte under 300 kB must be yellow: {below_out:?}"
        );
    }

    #[test]
    fn no_listed_entry_is_ever_uncolored_in_ansi_mode() {
        // The user's clarification: there is no uncoloured band — if a file is
        // worth listing it is at least yellow. A future tier added between the
        // floor and 300 kB must not reintroduce one.
        let files = vec![
            lf("a.rs", SIZE_THRESHOLD),
            lf("b.rs", 100 * 1024),
            lf("c.rs", SEVERE_SIZE_THRESHOLD),
            lf("d.rs", 5 * 1024 * 1024),
        ];
        let out =
            format_report(&files, files.len(), false, ColorMode::Ansi).expect("should report");
        for line in out.lines().filter(|l| l.starts_with("  ")) {
            assert!(
                line.contains(ANSI_RED) || line.contains(ANSI_YELLOW),
                "entry line is uncoloured: {line:?}"
            );
        }
    }

    #[test]
    fn plain_mode_emits_no_escapes_at_any_size() {
        let files = vec![lf("huge.rs", 900 * 1024), lf("big.rs", 45 * 1024)];
        let out =
            format_report(&files, files.len(), false, ColorMode::Plain).expect("should report");
        assert!(
            !out.contains('\u{1b}'),
            "plain mode must stay byte-identical to the pre-#557 format: {out:?}"
        );
    }

    #[test]
    fn header_and_more_tail_stay_uncolored() {
        // Per-entry severity only: colouring the header would make the block
        // read as one alarm rather than a scannable list.
        let files: Vec<LargeFile> = (0..REPORT_LIMIT)
            .map(|i| lf(&format!("f{i}.rs"), 400 * 1024))
            .collect();
        let out =
            format_report(&files, REPORT_LIMIT + 3, false, ColorMode::Ansi).expect("should report");
        let header = out.lines().next().expect("header");
        assert!(!header.contains('\u{1b}'), "header coloured: {header:?}");
        let tail = out
            .lines()
            .find(|l| l.contains("more)"))
            .expect("more-tail");
        assert!(!tail.contains('\u{1b}'), "tail coloured: {tail:?}");
    }

    #[test]
    fn color_is_off_unless_stderr_is_a_terminal_and_no_color_is_unset() {
        assert_eq!(ColorMode::decide(true, false), ColorMode::Ansi);
        // Piped output — the `--dry-run` case that must stay byte-identical.
        assert_eq!(ColorMode::decide(false, false), ColorMode::Plain);
        // NO_COLOR wins even on a terminal.
        assert_eq!(ColorMode::decide(true, true), ColorMode::Plain);
        assert_eq!(ColorMode::decide(false, true), ColorMode::Plain);
    }

    #[test]
    fn format_report_truncated_shows_more_tail() {
        let files: Vec<LargeFile> = (0..REPORT_LIMIT)
            .map(|i| lf(&format!("f{i}.rs"), 60 * 1024))
            .collect();
        // total exceeds shown → a "(N more)" tail must appear.
        let out = format_report(&files, REPORT_LIMIT + 3, false, ColorMode::Plain)
            .expect("should report");
        assert!(out.contains("(3 more)"), "missing more-tail: {out:?}");
    }

    #[test]
    fn format_report_empty_is_none_unless_timed_out() {
        assert_eq!(format_report(&[], 0, false, ColorMode::Plain), None);
        let note = format_report(&[], 0, true, ColorMode::Plain).expect("timeout note");
        assert!(note.contains("exceeded 1s budget"), "got: {note:?}");
    }

    #[test]
    fn format_report_partial_scan_note_follows_entries() {
        let files = vec![lf("big.rs", 80 * 1024)];
        let out = format_report(&files, 1, true, ColorMode::Plain).expect("should report");
        assert!(out.contains("  big.rs ("), "entry must be present: {out:?}");
        assert!(
            out.contains("scan stopped at 1s"),
            "partial-scan note must be present: {out:?}"
        );
    }

    #[test]
    fn deadline_short_circuits() {
        // Zero deadline guarantees an immediate cutoff without any
        // sleep-based flake risk in CI.
        let dir = fixture();
        write_file(dir.path(), "src.rs", 100 * 1024);
        let report = collect(dir.path(), Duration::from_millis(0));
        assert!(report.timed_out);
    }
}

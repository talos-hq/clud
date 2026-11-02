//! Session temp directory (`~/.clud/tmp`) — issue #509.
//!
//! While a clud session is active we point the backend agent's temp dir
//! (`TMPDIR` on Unix, `TMP`+`TEMP` on Windows) at a clud-owned location so
//! the scatter of agent/tooling temp files lands somewhere the daemon can
//! reclaim, instead of the OS temp dir where nothing ages it out.
//!
//! Two halves:
//! - [`ensure_dir`] — resolve `~/.clud/tmp`, create it, hand the path back
//!   to the env builders in `runner.rs` / `daemon/io_helpers.rs`.
//! - [`sweep_stale`] — reclaim entries that are both older than
//!   [`STALE_THRESHOLD`] (72h) by their own mtime *and* have no recent
//!   activity anywhere beneath them, to a bounded depth
//!   ([`MAX_NESTED_DEPTH`]). The daemon resumes a bounded work queue via
//!   `daemon/session_tmp_sweep.rs` on every tick while work is pending.
//!
//!   This said "drop **top-level** entries" until #1148, and meant it: a
//!   directory's mtime tracks its *direct* children only, so the agent
//!   harness's `claude-<uid>/<project>/<session>/` tree — whose root gains a
//!   child whenever a new project is used, and is therefore permanently
//!   fresh — was skipped in full, forever. 59 GB of it, on the box that
//!   reported this, while the sweep deleted 2,731 flat entries a day and
//!   logged every one. The failure mode to keep in mind here is not "the
//!   sweep stopped running"; it is "the sweep ran happily and could not see.
//!
//! Like `gc::uv_cache`, this operates directly on the filesystem — there is
//! no redb registry row. All errors are non-fatal: a failed sweep never
//! crashes the daemon, and a failed `ensure_dir` just falls back to the OS
//! temp dir (the env vars are simply not overridden).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[cfg(test)]
use super::delete_audit;
use super::session_tmp_continuation;

/// How old (by mtime) a top-level entry must be before [`sweep_stale`]
/// removes it.
///
/// **72 hours, and the number is the weekend.** Someone who stops work Friday
/// evening and returns Monday morning has been away ~63 hours; at the previous
/// 48h every scratch artifact they left behind was gone before they sat back
/// down. Three days clears that gap with margin and is the floor every
/// temp-reclaiming policy in this repo now shares —
/// [`crate::gc::uv_cache::STALE_THRESHOLD`] and
/// [`crate::gc::target_sweep::DEFAULT_STALE_DAYS`] were brought to the same
/// value for the same reason.
///
/// It remains a *separate* constant from the worktree GC policy: session-temp
/// lifetime and worktree staleness are independent policies that only happen
/// to share a number (issue #509).
pub const STALE_THRESHOLD: Duration = Duration::from_secs(72 * 60 * 60);

/// The audit `rule` string, derived from [`STALE_THRESHOLD`] so the two cannot
/// drift. It read `stale>48h` for as long as the constant said 48h, which is
/// exactly the kind of agreement a `format!` should be enforcing rather than a
/// reviewer.
pub(super) fn stale_rule() -> String {
    format!("session-tmp stale>{}h", STALE_THRESHOLD.as_secs() / 3_600)
}

/// Outcome of [`sweep_tick_at`]. `removed` counts files+dirs dropped (or,
/// in `dry_run`, that would have been); `skipped` counts lock/permission
/// failures that a later sweep will retry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PendingPhases {
    pub exploring: usize,
    pub probing: usize,
    pub scanning: usize,
    pub rechecking: usize,
    pub deleting_files: usize,
    pub deleting_dirs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Candidate operations or phase transitions completed in this pass.
    pub advanced_steps: usize,
    pub removed: usize,
    pub skipped: usize,
    pub dry_run: bool,
    /// Work done in this tick; pending candidates continue next tick.
    pub examined: u64,
    pub removed_files: u64,
    pub removed_dirs: u64,
    pub reclaimed_bytes: u64,
    pub pending: usize,
    pub phases: PendingPhases,
    pub current_phase: Option<&'static str>,
    pub current_path: Option<PathBuf>,
    pub cursor_examined: Option<usize>,
    pub failures: usize,
    pub pending_age_secs: u64,
    pub no_progress_secs: u64,
    pub retry_count: u32,
    pub last_error_path: Option<PathBuf>,
    pub last_error_class: Option<String>,
    pub last_error_message: Option<String>,
    pub persistent_failure: bool,
    /// Queue items dropped (spared) after repeated failures this tick (#1672).
    pub abandoned: usize,
    /// Session directories over [`SIZE_REPORT_THRESHOLD`] that the sweep kept
    /// because they are still in use (#1148), newest-first by size.
    ///
    /// **Reported, not reclaimed.** Age is the wrong control for a directory
    /// an agent can fill at 10 GB a file, but the fix for that is a policy
    /// decision about deleting a *live* session's working data, and this
    /// commit does not make it. What it does is end the silence: today a
    /// session at 48 GB produces no signal anywhere.
    pub oversized: Vec<(PathBuf, u64)>,
    /// Subtrees skipped as unremovable this pass: owned by another user (a
    /// Docker bind mount's `root` output) or refused with `PermissionDenied`.
    /// Marked on the first hit so the pass finishes the rest (#2486).
    pub foreign_owned: Vec<PathBuf>,
}

/// File (under `~/.clud/state`) holding the last completed pass's
/// [`SweepReport::foreign_owned`] list, read by `clud gc` (#2486).
pub const FOREIGN_OWNED_FILE: &str = "session-tmp-foreign-owned.json";

/// Persist the unremovable list next to the sweep state. An empty list
/// removes the file, so a cleaned-up tree stops being reported.
pub fn write_foreign_owned(state_dir: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
    let file = state_dir.join(FOREIGN_OWNED_FILE);
    if paths.is_empty() {
        return match std::fs::remove_file(&file) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(state_dir)?;
    let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let body = serde_json::to_vec_pretty(&list).map_err(std::io::Error::other)?;
    std::fs::write(file, body)
}

/// Read the list [`write_foreign_owned`] stored; missing or torn reads empty.
pub fn read_foreign_owned(state_dir: &Path) -> Vec<PathBuf> {
    std::fs::read(state_dir.join(FOREIGN_OWNED_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .map(|list| list.into_iter().map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// `clud gc` lines for unremovable subtrees, each with the command that
/// removes it: `sudo rm -rf`, or a throwaway root container when sudo is
/// unavailable (Docker made the files, so Docker can delete them).
pub fn foreign_owned_lines(paths: &[PathBuf]) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "warning: ~/.clud/tmp: {} path(s) owned by another user (likely Docker) cannot be swept; remove them with:",
        paths.len()
    )];
    for path in paths {
        let quoted = shell_quote(&path.display().to_string());
        lines.push(format!("  sudo rm -rf {quoted}"));
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            lines.push(format!(
                "    or: docker run --rm -v {}:/x alpine rm -rf {}",
                shell_quote(&parent.display().to_string()),
                shell_quote(&format!("/x/{}", name.to_string_lossy())),
            ));
        }
    }
    lines
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Size at which a still-live session directory is worth mentioning.
///
/// 5 GiB: large enough that an ordinary scratchpad of logs, patches and
/// intermediate JSON never trips it, small enough to catch the shape that
/// prompted this — VM images and multi-gigabyte tarballs written into a
/// scratchpad clud told the agent to use.
pub const SIZE_REPORT_THRESHOLD: u64 = 5 * 1024 * 1024 * 1024;

/// Opt-out env var. Set to `0`/`false`/`no`/`off` to keep the OS temp dir.
pub const DISABLE_ENV: &str = "CLUD_SESSION_TMP";

/// Temp env vars we override at session launch. `TMPDIR` is the Unix
/// convention; `TMP`/`TEMP` are what Windows `GetTempPath` consults (and
/// some cross-platform tooling reads on Unix too). Setting all three is the
/// most robust. Also the set of keys to strip before re-adding, so we never
/// pass a stale inherited value alongside the override.
pub const OVERRIDDEN_KEYS: &[&str] = &["TMPDIR", "TMP", "TEMP"];

/// Resolve `~/.clud/tmp` without creating it. `None` when no home dir can
/// be determined (headless/misconfigured env) — the caller then leaves the
/// OS temp dir in place.
pub fn session_tmp_dir() -> Option<PathBuf> {
    Some(session_tmp_dir_for(&crate::home::user_home()?))
}

/// `<home>/.clud/tmp`. Pure; `worktree_root` (#1485) asserts its own root is
/// a sibling of this one, never inside it.
pub fn session_tmp_dir_for(home: &Path) -> PathBuf {
    home.join(".clud").join("tmp")
}

/// Whether the redirect is disabled via [`DISABLE_ENV`]. Default: enabled.
pub fn is_disabled() -> bool {
    match std::env::var(DISABLE_ENV) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

/// The `(key, value)` temp-env overrides to layer onto a child environment,
/// or an empty vec when the redirect is disabled or the dir can't be created
/// (in which case the child keeps the OS temp dir). Creates `~/.clud/tmp` as
/// a side effect on the success path.
pub fn env_overrides() -> Vec<(String, String)> {
    if is_disabled() {
        return Vec::new();
    }
    let Some(dir) = ensure_dir() else {
        return Vec::new();
    };
    build_overrides(&dir)
}

fn build_overrides(dir: &Path) -> Vec<(String, String)> {
    let value = dir.to_string_lossy().into_owned();
    OVERRIDDEN_KEYS
        .iter()
        .map(|key| ((*key).to_string(), value.clone()))
        .collect()
}

/// Resolve `~/.clud/tmp` and create it. Returns the path on success so the
/// env builders can point `TMPDIR`/`TMP`/`TEMP` at it. Any failure (no home,
/// unwritable volume) yields `None` and the caller keeps the OS temp dir.
pub fn ensure_dir() -> Option<PathBuf> {
    let dir = session_tmp_dir()?;
    match fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(_) => None,
    }
}

/// How many levels below the root the sweep will look for stale trees.
///
/// The tree this exists for is `claude-<uid>/<project-slug>/<session-uuid>/`,
/// which is three. It is **not** hard-coded by name, and deliberately so: the
/// issue that reported this described that layout as "created by clud itself,
/// so the depth is known, not guessed", but clud does not create it. Nothing
/// in this repo constructs that path — clud only points `TMPDIR` at
/// `~/.clud/tmp`, and the agent harness creates its own tree underneath.
/// Matching on `claude-*` would be clud hard-coding a foreign layout it does
/// not own and is not told about when it changes, which is how this bug
/// reappears silently the next time the harness reorganises. A small depth
/// bound needs no such agreement, and covers the same ground.
pub(super) const MAX_NESTED_DEPTH: usize = 3;

/// Maximum entry/operation allowance for one daemon continuation tick.
/// Exhausting it yields persisted work to the next tick, never an idle/busy
/// decision. The legacy one-shot test fixture below also uses this value.
const SUBTREE_SCAN_BUDGET: usize = 200_000;

/// Is anything in `dir`'s subtree newer than `threshold`?
///
/// Returns as soon as it finds one recent entry, so the common case — a live
/// session, which is writing constantly — costs a few `stat`s rather than a
/// full walk. Only a genuinely idle tree is walked to the end, and that tree
/// is about to be removed, so the walk happens once.
///
/// **Every failure answers `true`.** An unreadable directory, a broken
/// timestamp, a clock that moved backwards, or a subtree so large it exhausts
/// the budget all mean the same thing: this code could not establish that the
/// tree is idle. The consequence of a wrong `true` is that some bytes survive
/// until the next sweep. The consequence of a wrong `false` is
/// `remove_dir_all` over a directory somebody is using — a 48 GB scratchpad,
/// or the working files of the session running this very sweep. Those are not
/// symmetric, and the tie goes to keeping data.
#[cfg(test)]
fn has_recent_activity(
    dir: &Path,
    now: SystemTime,
    threshold: Duration,
    budget: &mut usize,
) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        if *budget == 0 {
            return true;
        }
        *budget -= 1;
        let Ok(entry) = entry else { return true };
        let Ok(meta) = entry.metadata() else {
            return true;
        };
        let Ok(mtime) = meta.modified() else {
            return true;
        };
        match now.duration_since(mtime) {
            // Newer than the threshold: something here is in use.
            Ok(age) if age <= threshold => return true,
            Ok(_) => {}
            // Clock skew (mtime in the future). Unresolvable, so: keep.
            Err(_) => return true,
        }
        if meta.is_dir() {
            subdirs.push(entry.path());
        }
    }
    // Breadth first: a live session's recently-written file is usually shallow,
    // and finding it early is the whole point of the early exit.
    subdirs
        .into_iter()
        .any(|sub| has_recent_activity(&sub, now, threshold, budget))
}

/// Total bytes in a subtree, or `None` if the budget ran out.
///
/// Apparent size, matching `du --apparent-size`: the sparse VM images that
/// prompted the size rule are 259 GB apparent and 48 GB on disk, and the
/// number worth reporting is the one that says what was written.
#[cfg(test)]
fn subtree_size(dir: &Path, budget: &mut usize) -> Option<u64> {
    let mut total = 0u64;
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let entry = entry.ok()?;
        let meta = entry.metadata().ok()?;
        if meta.is_dir() {
            total = total.saturating_add(subtree_size(&entry.path(), budget)?);
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    Some(total)
}

/// Production sweep entry point — called from the daemon's periodic tick.
pub fn sweep_stale(now: SystemTime, dry_run: bool) -> std::io::Result<SweepReport> {
    let Some(root) = session_tmp_dir() else {
        return Ok(SweepReport {
            dry_run,
            ..Default::default()
        });
    };
    let Some(parent) = root.parent() else {
        return Ok(SweepReport {
            dry_run,
            ..Default::default()
        });
    };
    let work_path = parent.join("state/session-tmp-sweep.work.json");
    sweep_tick_at(&root, &work_path, now, dry_run, SUBTREE_SCAN_BUDGET)
}

/// Run one bounded continuation quantum. The work queue is durable at
/// `work_path`; a nonzero `pending` count means the next daemon tick must
/// resume it regardless of the ordinary six-hour start cadence.
pub fn sweep_tick_at(
    root: &Path,
    work_path: &Path,
    now: SystemTime,
    dry_run: bool,
    allowance: usize,
) -> std::io::Result<SweepReport> {
    let tick = session_tmp_continuation::sweep_tick_at(root, work_path, now, dry_run, allowance)?;
    Ok(report_from_tick(tick, dry_run))
}

pub fn sweep_tick_with_quantum(
    root: &Path,
    work_path: &Path,
    now: SystemTime,
    dry_run: bool,
    allowance: usize,
    candidate_quantum: usize,
) -> std::io::Result<SweepReport> {
    let tick = session_tmp_continuation::sweep_tick_with_quantum(
        root,
        work_path,
        now,
        dry_run,
        allowance,
        candidate_quantum,
    )?;
    Ok(report_from_tick(tick, dry_run))
}

fn report_from_tick(tick: session_tmp_continuation::TickReport, dry_run: bool) -> SweepReport {
    SweepReport {
        advanced_steps: tick.advanced_steps,
        removed: (tick.removed_files + tick.removed_dirs) as usize,
        skipped: tick.failures,
        dry_run,
        examined: tick.examined,
        removed_files: tick.removed_files,
        removed_dirs: tick.removed_dirs,
        reclaimed_bytes: tick.reclaimed_bytes,
        pending: tick.pending,
        phases: tick.phases,
        current_phase: tick.current_phase,
        current_path: tick.current_path,
        cursor_examined: tick.cursor_examined,
        failures: tick.failures,
        pending_age_secs: tick.pending_age_secs,
        no_progress_secs: tick.no_progress_secs,
        retry_count: tick.retry_count,
        last_error_path: tick.last_error_path,
        last_error_class: tick.last_error_class,
        last_error_message: tick.last_error_message,
        persistent_failure: tick.persistent_failure,
        abandoned: tick.abandoned,
        oversized: tick.oversized,
        foreign_owned: tick.foreign_owned,
    }
}

/// Legacy one-shot fixture retained only for historical policy tests. The
/// shipped daemon and new regression tests use [`sweep_tick_at`] instead.
/// Missing directory is a valid empty state, not an error.
#[cfg(test)]
pub fn sweep_stale_at(root: &Path, now: SystemTime, dry_run: bool) -> std::io::Result<SweepReport> {
    sweep_stale_at_with_budget(root, now, dry_run, SUBTREE_SCAN_BUDGET)
}

#[cfg(test)]
fn sweep_stale_at_with_budget(
    root: &Path,
    now: SystemTime,
    dry_run: bool,
    scan_budget: usize,
) -> std::io::Result<SweepReport> {
    let mut report = SweepReport {
        dry_run,
        ..Default::default()
    };
    if !root.exists() {
        return Ok(report);
    }
    let mut budget = scan_budget;
    // A separate budget: sizing is a full walk with no early exit, and it must
    // not be able to starve the freshness checks that decide what gets
    // deleted. Running out of it costs a size report; running out of the other
    // one costs nothing, because exhaustion there means "keep".
    let mut size_budget = scan_budget;
    sweep_level(
        root,
        now,
        dry_run,
        0,
        &mut budget,
        &mut size_budget,
        &mut report,
    )?;
    report
        .oversized
        .sort_by_key(|entry| std::cmp::Reverse(entry.1));
    Ok(report)
}

/// One directory level of the sweep.
///
/// `depth` is distance from the sweep root: 0 is `~/.clud/tmp` itself.
///
/// # Why this recurses at all (#1148)
///
/// A directory's mtime changes when its *direct* children change, and nothing
/// deeper. The agent harness lays out `claude-<uid>/<project>/<session>/`
/// under our root, and `claude-<uid>/` gains a direct child whenever a session
/// runs against a project it has not seen lately — so its mtime is always
/// fresh, the single-level sweep always skipped it, and everything beneath it
/// was unreachable *forever*. On the box that reported this, 59 GB, of which
/// one session was 48 GB, while the sweep was healthily deleting 2,731 flat
/// entries a day and logging every one.
///
/// # Why removal now needs two agreeing signals
///
/// A stale own-mtime is no longer sufficient on its own; the subtree must also
/// be idle ([`has_recent_activity`]). That is a deliberate tightening beyond
/// the reported bug, because the same mtime semantics cut the other way: a
/// directory whose own mtime is old but whose *contents* are being written
/// right now was, until this commit, deleted out from under its writer. The
/// nested descent would have widened the blast radius of that.
#[cfg(test)]
fn sweep_level(
    dir: &Path,
    now: SystemTime,
    dry_run: bool,
    depth: usize,
    budget: &mut usize,
    size_budget: &mut usize,
    report: &mut SweepReport,
) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        // duration_since returns Err on clock skew (future mtime) — skip it;
        // we'll reconsider once wall-clock catches up.
        let Ok(age) = now.duration_since(mtime) else {
            continue;
        };
        let own_mtime_is_stale = age > STALE_THRESHOLD;

        if meta.is_dir() {
            if own_mtime_is_stale && !has_recent_activity(&path, now, STALE_THRESHOLD, budget) {
                remove_entry(&path, true, dry_run, &stale_rule(), report);
                continue;
            }
            // Still in use, or holding something that is. Look inside for a
            // stale tree the parent's fresh mtime is hiding — the #1148 case.
            if depth + 1 < MAX_NESTED_DEPTH {
                // A failure to read one subdirectory must not abandon the rest
                // of the sweep, so the error is dropped rather than returned.
                let _ = sweep_level(&path, now, dry_run, depth + 1, budget, size_budget, report);
            } else if let Some(bytes) = subtree_size(&path, size_budget) {
                // Only at the deepest evaluated level — the session dir. Sizing
                // is a full walk with no early exit, so doing it at the level
                // above would re-walk every session once per project, and at
                // the root would walk the entire temp tree.
                if bytes >= SIZE_REPORT_THRESHOLD {
                    report.oversized.push((path.clone(), bytes));
                }
            }
            continue;
        }

        if own_mtime_is_stale {
            remove_entry(&path, false, dry_run, &stale_rule(), report);
        }
    }
    Ok(())
}

/// Audit, then remove, counting the outcome. Audit happens before the act
/// (#893): the root is clud-owned, but the audit line is what proves what was
/// removed after the fact.
#[cfg(test)]
fn remove_entry(path: &Path, is_dir: bool, dry_run: bool, rule: &str, report: &mut SweepReport) {
    if dry_run {
        report.removed += 1;
        return;
    }
    delete_audit::record("gc.session-tmp", path, rule);
    let result = if is_dir {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => report.removed += 1,
        // Non-fatal (Windows lock, races) — retried on the next sweep.
        Err(_) => report.skipped += 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::time::Duration as StdDuration;
    use tempfile::tempdir;

    fn make_file(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        let mut f = File::create(&path).unwrap();
        writeln!(f, "temp payload {name}").unwrap();
        path
    }

    fn make_subdir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        make_file(&dir, "inner.txt");
        dir
    }

    // -----------------------------------------------------------------
    // #1148: the nested tree the flat sweep could never see.
    // -----------------------------------------------------------------

    /// Backdate a path's mtime by `age`.
    ///
    /// Directories included, which is why this uses `filetime` rather than
    /// `std::fs::File::set_modified`: `File::open` on a directory fails on
    /// Windows. The alternative — sleeping until entries age — is not
    /// something this repo does in tests.
    fn age_path(path: &Path, age: StdDuration) {
        let when = SystemTime::now() - age;
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when)).unwrap();
    }

    /// Build `root/claude-1000/<project>/<session>/scratchpad/big.img`, age the
    /// *session* dir and everything under it, and leave the two dirs above it
    /// fresh — exactly the shape from the report.
    fn nested_session(root: &Path, project: &str, session: &str, age: StdDuration) -> PathBuf {
        let session_dir = root.join("claude-1000").join(project).join(session);
        let scratch = session_dir.join("scratchpad");
        fs::create_dir_all(&scratch).unwrap();
        make_file(&scratch, "big.img");
        for p in [
            scratch.join("big.img"),
            scratch.clone(),
            session_dir.clone(),
        ] {
            age_path(&p, age);
        }
        session_dir
    }

    /// The regression. A stale session under a *fresh* parent was invisible to
    /// the sweep forever, because `claude-1000/` gains a direct child every
    /// time a new project is used and its mtime is therefore always recent.
    #[test]
    fn stale_session_under_a_fresh_parent_is_swept() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let stale = nested_session(
            root,
            "-home-user-proj",
            "sess-old",
            STALE_THRESHOLD + StdDuration::from_secs(3_600),
        );
        // The parent chain is fresh, which is the whole trap.
        assert!(root.join("claude-1000").exists());

        let report = sweep_stale_at(root, SystemTime::now(), false).unwrap();

        assert!(!stale.exists(), "the stale session survived the sweep");
        assert!(report.removed >= 1, "{report:?}");
    }

    /// The hazard that matters more than the leak: a session whose own
    /// directory mtime is old because all its writes land one level down, in
    /// `scratchpad/`. Removing it would delete a running agent's working
    /// files — including, on this machine, the sweep's own session.
    #[test]
    fn a_session_still_being_written_to_is_never_swept() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let old = STALE_THRESHOLD + StdDuration::from_secs(7_200);
        let session = nested_session(root, "-home-user-live", "sess-live", old);
        // Now simulate the live part: one fresh file inside scratchpad, while
        // the session dir's own mtime stays old.
        let fresh = session.join("scratchpad").join("in-progress.txt");
        fs::write(&fresh, "being written").unwrap();
        age_path(&session, old);

        sweep_stale_at(root, SystemTime::now(), false).unwrap();

        assert!(
            fresh.exists(),
            "a live session's scratchpad was deleted out from under it"
        );
        assert!(session.exists());
    }

    /// The same rule at the top level, which is the pre-existing half of the
    /// hazard: this shape was deleted before this commit.
    #[test]
    fn a_top_level_dir_with_fresh_contents_is_never_swept() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let dir = make_subdir(root, "soldr-x-home-123");
        let fresh = dir.join("still-in-use.txt");
        fs::write(&fresh, "live").unwrap();
        age_path(&dir, STALE_THRESHOLD + StdDuration::from_secs(3_600));

        sweep_stale_at(root, SystemTime::now(), false).unwrap();

        assert!(
            fresh.exists(),
            "deleted a directory whose contents are live"
        );
    }

    /// A wholly idle tree still goes, at the top level, as it always did.
    #[test]
    fn a_fully_idle_top_level_dir_is_still_swept() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let dir = make_subdir(root, "soldr-y-home-456");
        let old = STALE_THRESHOLD + StdDuration::from_secs(3_600);
        age_path(&dir.join("inner.txt"), old);
        age_path(&dir, old);

        let report = sweep_stale_at(root, SystemTime::now(), false).unwrap();

        assert!(!dir.exists(), "{report:?}");
    }

    /// One stale session next to a live one: the stale goes, the live stays,
    /// and the project directory survives because it still has a tenant.
    #[test]
    fn a_stale_session_goes_without_taking_its_live_sibling() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let old = STALE_THRESHOLD + StdDuration::from_secs(3_600);
        let stale = nested_session(root, "-proj", "sess-stale", old);
        let live = nested_session(root, "-proj", "sess-live", old);
        let live_file = live.join("scratchpad").join("now.txt");
        fs::write(&live_file, "fresh").unwrap();
        age_path(&live, old);

        sweep_stale_at(root, SystemTime::now(), false).unwrap();

        assert!(!stale.exists(), "the idle session should have gone");
        assert!(live_file.exists(), "the live sibling was taken with it");
    }

    /// The depth bound is a real bound, not decoration: below it, the sweep
    /// stops looking. Asserting it keeps a future "just make it recursive"
    /// change from turning this into an unbounded walk of every temp tree on
    /// the box without someone deciding to.
    #[test]
    fn the_sweep_does_not_descend_past_max_nested_depth() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let old = STALE_THRESHOLD + StdDuration::from_secs(3_600);
        // Entries are *evaluated* at depths 0, 1 and 2 — three levels, which
        // is what covers `claude-<uid>/<project>/<session>`. `d` sits at
        // depth 3, so it is never evaluated on its own. Its ancestors are
        // left fresh deliberately: a stale ancestor would remove this whole
        // tree from above and the test would pass without the bound doing
        // anything.
        let deep = root.join("a").join("b").join("c").join("d");
        fs::create_dir_all(&deep).unwrap();
        let leaf = deep.join("buried.txt");
        fs::write(&leaf, "x").unwrap();
        age_path(&leaf, old);
        age_path(&deep, old);

        sweep_stale_at(root, SystemTime::now(), false).unwrap();

        assert!(
            leaf.exists(),
            "evaluated an entry below MAX_NESTED_DEPTH; the bound is gone"
        );
    }

    /// Exhausting the scan budget must resolve to *keep*. A wrong "idle" is a
    /// `remove_dir_all` over live data; a wrong "busy" costs one sweep cycle.
    #[test]
    fn an_exhausted_scan_budget_keeps_the_tree() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("wide");
        fs::create_dir_all(&dir).unwrap();
        make_file(&dir, "a.txt");
        age_path(
            &dir.join("a.txt"),
            STALE_THRESHOLD + StdDuration::from_secs(3_600),
        );

        let mut budget = 0usize;
        assert!(
            has_recent_activity(&dir, SystemTime::now(), STALE_THRESHOLD, &mut budget),
            "a budget-exhausted scan must answer 'busy', never 'idle'"
        );
    }

    /// #1260: exhausting one tick's work allowance must defer, not cause an
    /// otherwise-idle candidate to survive every future tick.
    #[test]
    fn stale_tree_larger_than_one_scan_budget_eventually_goes() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let old = STALE_THRESHOLD + StdDuration::from_secs(3_600);
        let wide = root.join("wide-stale");
        fs::create_dir_all(&wide).unwrap();
        for index in 0..6 {
            let file = make_file(&wide, &format!("{index}.txt"));
            age_path(&file, old);
        }
        age_path(&wide, old);

        let work_path = tmp.path().join("state/work.json");
        for _ in 0..80 {
            sweep_tick_at(root, &work_path, SystemTime::now(), false, 2).unwrap();
            if !wide.exists() {
                break;
            }
        }
        assert!(!wide.exists(), "an idle tree must not be kept forever");
    }

    /// A fresh parent cannot be removed, so scanning its entire subtree for
    /// idleness must not consume the budget needed by a stale child.
    #[test]
    fn fresh_parent_does_not_starve_stale_child() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let parent = root.join("fresh-parent");
        let stale = parent.join("stale-child");
        fs::create_dir_all(&stale).unwrap();
        let file = make_file(&stale, "old.txt");
        let old = STALE_THRESHOLD + StdDuration::from_secs(3_600);
        age_path(&file, old);
        age_path(&stale, old);

        sweep_stale_at_with_budget(root, SystemTime::now(), false, 1).unwrap();

        assert!(!stale.exists(), "fresh parent exhausted the child's budget");
    }

    /// An mtime in the future (clock skew, a restored backup, a bad NTP step)
    /// is not evidence of idleness.
    #[test]
    fn a_future_mtime_keeps_the_tree() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("skewed");
        fs::create_dir_all(&dir).unwrap();
        make_file(&dir, "a.txt");
        let ahead = SystemTime::now() + StdDuration::from_secs(86_400);
        filetime::set_file_mtime(
            dir.join("a.txt"),
            filetime::FileTime::from_system_time(ahead),
        )
        .unwrap();

        let mut budget = SUBTREE_SCAN_BUDGET;
        assert!(has_recent_activity(
            &dir,
            SystemTime::now(),
            STALE_THRESHOLD,
            &mut budget
        ));
    }

    /// `dry_run` counts without touching anything — the nested path included.
    #[test]
    fn dry_run_reports_nested_removals_without_performing_them() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let stale = nested_session(
            root,
            "-p",
            "s",
            STALE_THRESHOLD + StdDuration::from_secs(3_600),
        );

        let report = sweep_stale_at(root, SystemTime::now(), true).unwrap();

        assert!(report.dry_run);
        assert!(report.removed >= 1, "{report:?}");
        assert!(stale.exists(), "dry_run deleted something");
    }

    /// A live session over the threshold is reported, not deleted. This is the
    /// half of #1148 that age cannot cover: the 48 GB session on the box that
    /// filed it was *fresh* the whole time, so no age-based rule would ever
    /// have mentioned it.
    #[test]
    fn an_oversized_live_session_is_reported_and_kept() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let session = root.join("claude-1000").join("-proj").join("sess-fat");
        let scratch = session.join("scratchpad");
        fs::create_dir_all(&scratch).unwrap();
        let big = scratch.join("data.img");
        fs::write(&big, vec![0u8; 4096]).unwrap();

        // Threshold is 5 GiB; writing that much in a test is not reasonable,
        // so the check runs against a deliberately tiny bound via the same
        // code path the production constant feeds.
        let mut budget = SUBTREE_SCAN_BUDGET;
        let size = subtree_size(&session, &mut budget).unwrap();
        assert!(size >= 4096, "sized the wrong tree: {size}");

        let report = sweep_stale_at(root, SystemTime::now(), false).unwrap();
        assert!(big.exists(), "an oversized session must never be deleted");
        assert_eq!(report.removed, 0);
    }

    /// The reported list is largest-first, so the line that matters is the
    /// first one read.
    #[test]
    fn oversized_entries_are_sorted_largest_first() {
        let mut report = SweepReport {
            oversized: vec![(PathBuf::from("small"), 10), (PathBuf::from("big"), 100)],
            ..Default::default()
        };
        report
            .oversized
            .sort_by_key(|entry| std::cmp::Reverse(entry.1));
        assert_eq!(report.oversized[0].0, PathBuf::from("big"));
    }

    /// Apparent size, so the sparse VM images that prompted the size rule are
    /// reported at what was written rather than what the filesystem allocated.
    #[test]
    fn subtree_size_sums_the_tree() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("sized");
        fs::create_dir_all(dir.join("inner")).unwrap();
        fs::write(dir.join("a.bin"), vec![0u8; 1024]).unwrap();
        fs::write(dir.join("inner").join("b.bin"), vec![0u8; 2048]).unwrap();

        let mut budget = SUBTREE_SCAN_BUDGET;
        assert_eq!(subtree_size(&dir, &mut budget), Some(3072));
    }

    #[test]
    fn foreign_owned_list_round_trips_and_clears() {
        let temp = tempfile::tempdir().unwrap();
        let paths = vec![PathBuf::from("/home/u/.clud/tmp/s/zc")];
        write_foreign_owned(temp.path(), &paths).unwrap();
        assert_eq!(read_foreign_owned(temp.path()), paths);
        write_foreign_owned(temp.path(), &[]).unwrap();
        assert!(read_foreign_owned(temp.path()).is_empty());
        assert!(!temp.path().join(FOREIGN_OWNED_FILE).exists());
    }

    #[test]
    fn foreign_owned_lines_give_sudo_and_docker_commands() {
        let lines = foreign_owned_lines(&[PathBuf::from("/t/s/it's")]);
        assert!(lines[0].contains("1 path(s)"));
        assert_eq!(lines[1], "  sudo rm -rf '/t/s/it'\\''s'");
        assert_eq!(
            lines[2],
            "    or: docker run --rm -v '/t/s':/x alpine rm -rf '/x/it'\\''s'"
        );
        assert!(foreign_owned_lines(&[]).is_empty());
    }

    #[test]
    fn stale_threshold_is_72h() {
        assert_eq!(STALE_THRESHOLD, Duration::from_secs(72 * 60 * 60));
    }

    #[test]
    fn sweep_on_missing_dir_is_noop() {
        let tmp = tempdir().unwrap();
        let report = sweep_stale_at(&tmp.path().join("nope"), SystemTime::now(), false).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, 0);
    }

    #[test]
    fn sweep_leaves_fresh_entries() {
        let tmp = tempdir().unwrap();
        let f = make_file(tmp.path(), "fresh.txt");
        let d = make_subdir(tmp.path(), "fresh-dir");
        let report = sweep_stale_at(tmp.path(), SystemTime::now(), false).unwrap();
        assert_eq!(report.removed, 0);
        assert!(f.exists());
        assert!(d.exists());
    }

    #[test]
    fn sweep_removes_stale_files_and_dirs() {
        let tmp = tempdir().unwrap();
        let f = make_file(tmp.path(), "old.txt");
        let d = make_subdir(tmp.path(), "old-dir");
        // Pretend "now" is past the threshold so the just-created entries
        // read as older than the 48h threshold.
        let future_now = SystemTime::now() + STALE_THRESHOLD + StdDuration::from_secs(3_600);
        let report = sweep_stale_at(tmp.path(), future_now, false).unwrap();
        assert_eq!(report.removed, 2);
        assert!(!f.exists());
        assert!(!d.exists());
    }

    #[test]
    fn sweep_dry_run_reports_without_deleting() {
        let tmp = tempdir().unwrap();
        let f = make_file(tmp.path(), "old.txt");
        let future_now = SystemTime::now() + STALE_THRESHOLD + StdDuration::from_secs(3_600);
        let report = sweep_stale_at(tmp.path(), future_now, true).unwrap();
        assert_eq!(report.removed, 1);
        assert!(f.exists(), "dry run must not delete");
    }

    #[test]
    fn sweep_ignores_future_mtimes() {
        let tmp = tempdir().unwrap();
        let f = make_file(tmp.path(), "future.txt");
        // "now" far in the past → entry mtime is in the future → skipped.
        let past_now = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_000_000);
        let report = sweep_stale_at(tmp.path(), past_now, false).unwrap();
        assert_eq!(report.removed, 0);
        assert!(f.exists());
    }

    #[test]
    fn build_overrides_sets_all_three_keys_to_dir() {
        let dir = Path::new("/some/clud/tmp");
        let overrides = build_overrides(dir);
        let keys: Vec<&str> = overrides.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["TMPDIR", "TMP", "TEMP"]);
        for (_, v) in &overrides {
            assert_eq!(v, &dir.to_string_lossy());
        }
    }

    #[test]
    fn ensure_dir_creates_under_home() {
        let tmp = tempdir().unwrap();
        let _guard = HomeGuard::set(tmp.path());
        let dir = ensure_dir().expect("ensure_dir should succeed with a valid HOME");
        assert!(dir.exists());
        assert!(dir.ends_with("tmp"));
        assert!(dir.starts_with(tmp.path()));
    }

    /// RAII guard swapping HOME/USERPROFILE for the resolution test. `std::env`
    /// is process-global so serialize via a mutex.
    struct HomeGuard {
        prior_home: Option<String>,
        prior_userprofile: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn set(dir: &Path) -> Self {
            // Shared with the CwdChanged handler tests, which rewrite HOME too;
            // a lock private to this file serialized it against itself only.
            // See crate::test_env.
            let lock = crate::test_env::home_lock();
            let prior_home = std::env::var("HOME").ok();
            let prior_userprofile = std::env::var("USERPROFILE").ok();
            std::env::set_var("HOME", dir);
            std::env::set_var("USERPROFILE", dir);
            Self {
                prior_home,
                prior_userprofile,
                _lock: lock,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prior_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prior_userprofile.take() {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }
}

#[cfg(test)]
mod retention_floor_tests {
    use std::time::Duration;

    /// Every temp-reclaiming policy shares one floor, and the floor is the
    /// weekend.
    ///
    /// The bug this pins down is not "the number was wrong" — it is that four
    /// sweeps each picked a plausible number in isolation (48h, 7d, 14d) and
    /// nobody could see they disagreed. Two of them were longer than the time
    /// this machine takes to fill its disk, so they ran on schedule and
    /// reclaimed nothing. Asserting them together is what makes the next
    /// divergence a test failure instead of a full disk.
    ///
    /// 72h because a Friday-evening-to-Monday-morning absence is ~63h: at 48h
    /// the returning user's artifacts are already gone.
    const WEEKEND_FLOOR: Duration = Duration::from_secs(72 * 60 * 60);

    #[test]
    fn every_temp_policy_shares_the_weekend_floor() {
        assert_eq!(
            super::STALE_THRESHOLD,
            WEEKEND_FLOOR,
            "session-temp must not age out faster than a weekend"
        );
        assert_eq!(
            crate::gc::uv_cache::STALE_THRESHOLD,
            WEEKEND_FLOOR,
            "uv-cache must not age out faster than a weekend"
        );
        assert_eq!(
            Duration::from_secs(crate::gc::target_sweep::DEFAULT_STALE_DAYS * 24 * 60 * 60),
            WEEKEND_FLOOR,
            "target sweep must not age out faster than a weekend"
        );
    }

    /// The floor is a floor, not a target: it has to stay short enough to
    /// actually reclaim. A week-long window on a box that fills in four days
    /// is an off switch, which is how 7d and 14d survived unnoticed.
    #[test]
    fn the_floor_is_still_short_enough_to_reclaim() {
        assert!(
            WEEKEND_FLOOR < Duration::from_secs(4 * 24 * 60 * 60),
            "a retention window must be shorter than the time the disk takes to fill"
        );
    }
}

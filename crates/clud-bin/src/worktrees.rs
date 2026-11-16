//! `--clean-worktrees` implementation — issue #83.
//!
//! Long-running `clud` use accumulates git worktrees (one per agent run,
//! plus manual debugging worktrees). This module enumerates them via
//! `git worktree list --porcelain`, classifies each as
//! clean / dirty / unpushed / no-upstream / branch-gone, and removes those
//! that are safe to remove. "Safe" means **clean** AND
//! (older than `--stale-after` OR upstream `[gone]`).
//!
//! Issue #1606: the daemon's squash-aware verdict is consulted first
//! (`worktrees_verdict.rs`); a `reclaimable` verdict adds a removal that the
//! daemon's re-verifying executor performs, and every other verdict leaves
//! the rules above untouched (DD-129).
//!
//! The flow is intentionally side-effect free until we actually invoke
//! `git worktree remove`, so a `--dry-run` is a faithful preview of what a
//! real run would do.
//!
//! No new external crates — we shell out to `git` and parse text.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use running_process::{NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::daemon::repo_worktree_cli::{self, PrLookup, ProcsSource, RepoWorktreeRow};
use crate::gc::extract_pid_from_lock_reason;
use crate::path_norm::slash_separators;
use crate::session_registry::{LivenessProbe, OsLivenessProbe};
use crate::subprocess;
use crate::win_creation_flags::invisible_helper_creationflags;

pub const ENV_GC_LOCKED_HARD_AGE_DAYS: &str = "CLUD_GC_LOCKED_HARD_AGE_DAYS";
const DEFAULT_GC_LOCKED_HARD_AGE_DAYS: u64 = 7;
const SECS_PER_DAY: u64 = 86_400;

/// One row of the porcelain output from `git worktree list --porcelain`.
///
/// We only retain fields we actually use. `bare` worktrees and the main
/// worktree itself are flagged so we never try to remove them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    /// True when the porcelain block ends with a `locked` line.
    /// Live locks are skipped; stale dead/no-PID locks pass through a
    /// hard-age gate before normal removal rules are applied.
    pub locked: bool,
    /// Optional human-readable reason that accompanies a `locked` line.
    /// Claude Code emits `locked claude agent agent-<id> (pid <pid>)`
    /// when it holds a worktree for an in-flight agent; the GC
    /// liveness probe parses the pid out of this string.
    pub locked_reason: Option<String>,
    /// True when the porcelain block ends with a `prunable` line.
    /// Indicates git already considers the worktree stale; we use this
    /// as a hint but still apply our normal classification.
    pub prunable: bool,
}

/// Result of inspecting a worktree's working tree + upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeStatus {
    /// Clean working tree, upstream exists, no unpushed commits.
    Clean,
    /// `git status --porcelain` produced output.
    Dirty,
    /// Working tree clean but `@{u}..HEAD` is non-empty.
    Unpushed,
    /// `HEAD` has no upstream configured.
    NoUpstream,
    /// `git branch -vv` reports the upstream as `[gone]` (deleted on remote).
    BranchGone,
}

impl WorktreeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorktreeStatus::Clean => "clean",
            WorktreeStatus::Dirty => "dirty",
            WorktreeStatus::Unpushed => "unpushed",
            WorktreeStatus::NoUpstream => "no-upstream",
            WorktreeStatus::BranchGone => "branch-gone",
        }
    }
}

/// Liveness of a git-worktree lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStatus {
    Unlocked,
    /// Locked, but the lock reason did not contain a parseable PID.
    NoPid,
    /// Locked by a PID that is still alive.
    LivePid(u32),
    /// Locked by a PID that is no longer alive.
    DeadPid(u32),
}

/// Inputs to the stale-detection predicate. Kept as a plain struct so the
/// logic is trivial to unit-test without touching disk or `git`.
#[derive(Debug, Clone, Copy)]
pub struct StalenessInputs {
    pub status: WorktreeStatus,
    /// Age of the worktree's working directory (mtime delta).
    pub age: Duration,
    /// Liveness of any git-worktree lock.
    pub lock_status: LockStatus,
    /// Locked worktrees are presumed orphaned after this age, even if the
    /// lock PID still appears live.
    pub locked_hard_age: Duration,
}

/// Options gathered from the CLI flags.
#[derive(Debug, Clone)]
pub struct CleanOptions {
    pub stale_after: Duration,
    pub dry_run: bool,
    pub yes: bool,
    pub force: bool,
}

/// `--clean-worktrees` entry point. Returns the process exit code.
pub fn run(opts: &CleanOptions) -> i32 {
    let main_repo = match locate_main_repo_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let env = CleanEnv {
        lookup_prs: Arc::new(repo_worktree_cli::lookup_prs_for_cli),
        procs: Arc::new(repo_worktree_cli::live_process_cwds),
        wt_root: crate::gc::worktree_root::worktree_root(),
        liveness: &OsLivenessProbe,
        locked_hard_age: locked_hard_age_from_env(),
        verdict_deadline: VERDICT_DEADLINE,
    };
    run_in(&main_repo, opts, &env)
}

/// Everything [`run_in`] reads from the outside world besides the repo
/// itself, injected so the real-git tests (#1606) never reach the network,
/// the real process table or the real `~/.clud`.
///
/// The PR lookup and the process table are `Arc`s because the verdict is
/// gathered on a worker thread the CLI stops waiting for at
/// `verdict_deadline` (#1648); a slow source must not hold the command.
pub(crate) struct CleanEnv<'a> {
    pub(crate) lookup_prs: PrLookup,
    pub(crate) procs: ProcsSource,
    pub(crate) wt_root: Option<PathBuf>,
    pub(crate) liveness: &'a dyn LivenessProbe,
    pub(crate) locked_hard_age: Duration,
    /// Issue #1648: total time the CLI waits for verdict rows. A worktree
    /// with no row by then keeps the ancestry decision, marked
    /// `verdict timed out`.
    pub(crate) verdict_deadline: Duration,
}

/// Issue #1648: the whole verdict phase of `--clean-worktrees` gets this
/// long, well under the 10 s the CLI smoke test allows on a cold Windows
/// runner. Past it, the ancestry rules decide, which is the pre-#1606
/// behavior.
const VERDICT_DEADLINE: Duration = Duration::from_secs(4);

/// [`run`] against an explicit repo and environment.
pub(crate) fn run_in(main_repo: &Path, opts: &CleanOptions, env: &CleanEnv<'_>) -> i32 {
    let main_repo = main_repo.to_path_buf();
    let (rows, plan) = match plan_in(&main_repo, opts, env) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: failed to list worktrees: {e}");
            return 1;
        }
    };

    // Print a status table for visibility.
    print_table(&rows, opts);

    if plan.candidates.is_empty() {
        println!("\nNo worktrees match the removal criteria.");
        print_skipped(&plan.skipped);
        return 0;
    }

    println!("\nRemoval plan ({} candidate(s)):", plan.candidates.len());
    for c in &plan.candidates {
        println!("  remove  {}  ({})", c.entry.path.display(), c.reason);
    }
    print_skipped(&plan.skipped);

    if opts.dry_run {
        println!("\n--dry-run: no changes made.");
        return 0;
    }

    if !opts.yes && !confirm_interactive(plan.candidates.len()) {
        println!("Aborted.");
        return 0;
    }

    let mut removed = 0usize;
    let mut failed = 0usize;
    let mut spared = 0usize;
    for c in &plan.candidates {
        match execute_candidate(&main_repo, c, opts, env) {
            Tally::Removed(note) if note.is_empty() => {
                println!("removed {}", c.entry.path.display());
                removed += 1;
            }
            Tally::Removed(note) => {
                println!("removed {} ({note})", c.entry.path.display());
                removed += 1;
            }
            Tally::Skipped(reason) => {
                println!("skipped {} ({reason})", c.entry.path.display());
                spared += 1;
            }
            Tally::Failed(e) => {
                eprintln!("error removing {}: {}", c.entry.path.display(), e);
                failed += 1;
            }
        }
    }

    println!(
        "\nSummary: {removed} removed, {skipped} skipped, {failed} failed.",
        removed = removed,
        skipped = plan.skipped.len() + spared,
        failed = failed,
    );

    if failed > 0 {
        1
    } else {
        0
    }
}

/// Remove one planned candidate. A verdict-driven candidate goes through
/// the daemon's executor, which re-verifies from scratch and never forces
/// (DD-123); an ancestry-driven one keeps the original removal path.
fn execute_candidate(
    main_repo: &Path,
    c: &Candidate,
    opts: &CleanOptions,
    env: &CleanEnv<'_>,
) -> Tally {
    match &c.via_verdict {
        Some(row) => worktrees_verdict::tally_reclaim(repo_worktree_cli::reclaim_for_cli(
            row,
            env.wt_root.clone(),
            &*env.lookup_prs,
            &*env.procs,
        )),
        None => match remove_worktree_path(main_repo, &c.entry.path, opts.force) {
            Ok(note) => Tally::Removed(note.trim().to_string()),
            Err(e) => Tally::Failed(e),
        },
    }
}

/// Side-effect free: list, classify, probe the shared verdict, and plan.
/// `--dry-run` prints exactly this plan, so it is a faithful preview.
fn plan_in(
    main_repo: &Path,
    opts: &CleanOptions,
    env: &CleanEnv<'_>,
) -> Result<(Vec<Classified>, Plan), String> {
    let main_repo = main_repo.to_path_buf();
    let raw = run_git(&main_repo, &["worktree", "list", "--porcelain"])?;
    let entries = parse_worktree_porcelain(&raw);

    // Issue #1606: the daemon's squash-aware verdict for every worktree,
    // gathered by the same probe the daemon uses. A worktree the probe
    // yields no row for stays on the ancestry rules alone.
    //
    // Issue #1648: only when there is a worktree to judge (a lone main
    // checkout pays for no process table and no `gh`), on a worker thread,
    // and for at most `verdict_deadline` in total. Rows that arrived in
    // time count; the rest are marked `verdict timed out`.
    let has_candidates = entries.iter().skip(1).any(|e| !e.bare);
    let (verdict_rows, timed_out) = if has_candidates {
        collect_verdicts(
            repo_worktree_cli::spawn_cli_probe(
                main_repo.clone(),
                env.lookup_prs.clone(),
                env.procs.clone(),
                env.wt_root.clone(),
            ),
            Instant::now() + env.verdict_deadline,
        )
    } else {
        (Vec::new(), false)
    };

    // Gather classified rows. The first entry from `git worktree list` is the
    // main worktree — never a candidate for removal regardless of staleness.
    let mut rows: Vec<Classified> = Vec::new();
    for (idx, entry) in entries.iter().enumerate() {
        if entry.bare {
            continue;
        }
        let is_main = idx == 0;
        let status = if is_main {
            // Don't probe the main worktree; it's never a removal candidate
            // and inspecting it is wasted work.
            WorktreeStatus::Clean
        } else {
            classify_status(&entry.path)
        };
        let age = path_age(&entry.path).unwrap_or(Duration::ZERO);
        let want = canonical_or_raw(&entry.path);
        let verdict_row = verdict_rows
            .iter()
            .find(|r| canonical_or_raw(Path::new(&r.path)) == want)
            .cloned();
        rows.push(Classified {
            entry: entry.clone(),
            status,
            age,
            is_main,
            verdict_timed_out: timed_out && verdict_row.is_none(),
            verdict_row,
        });
    }

    let plan = build_plan_with_liveness(&rows, opts, env.liveness, env.locked_hard_age);
    Ok((rows, plan))
}

/// Issue #1648: drain verdict rows until the probe finishes (the channel
/// disconnects) or `deadline` passes. Returns the rows plus whether the
/// deadline cut the probe short.
fn collect_verdicts(
    rx: std::sync::mpsc::Receiver<RepoWorktreeRow>,
    deadline: Instant,
) -> (Vec<RepoWorktreeRow>, bool) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut rows = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(row) => rows.push(row),
            Err(RecvTimeoutError::Disconnected) => return (rows, false),
            Err(RecvTimeoutError::Timeout) => return (rows, true),
        }
    }
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn print_skipped(skipped: &[Candidate]) {
    if skipped.is_empty() {
        return;
    }
    println!("\nSkipped ({}):", skipped.len());
    for s in skipped {
        println!("  skip    {}  ({})", s.entry.path.display(), s.reason);
    }
}

#[derive(Debug, Clone)]
struct Classified {
    entry: WorktreeEntry,
    status: WorktreeStatus,
    age: Duration,
    is_main: bool,
    /// Issue #1606: the daemon probe's row for this worktree, if it has one.
    verdict_row: Option<RepoWorktreeRow>,
    /// Issue #1648: the verdict deadline passed before this worktree's row
    /// arrived.
    verdict_timed_out: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    entry: WorktreeEntry,
    reason: String,
    /// Set when the shared verdict (not ancestry) made this a candidate: the
    /// row the daemon's executor re-verifies before removing (#1606).
    via_verdict: Option<RepoWorktreeRow>,
}

#[derive(Debug, Default)]
struct Plan {
    candidates: Vec<Candidate>,
    skipped: Vec<Candidate>,
}

fn build_plan_with_liveness(
    rows: &[Classified],
    opts: &CleanOptions,
    probe: &dyn LivenessProbe,
    locked_hard_age: Duration,
) -> Plan {
    let mut plan = Plan::default();
    for row in rows {
        if row.is_main {
            continue;
        }
        let inputs = StalenessInputs {
            status: row.status,
            age: row.age,
            lock_status: lock_status_for_entry(&row.entry, probe),
            locked_hard_age,
        };
        let verdict = row.verdict_row.as_ref().map(|r| &r.verdict);
        let action = if row.verdict_timed_out {
            worktrees_verdict::decide_verdict_timed_out(inputs, opts)
        } else {
            worktrees_verdict::decide_with_verdict(inputs, verdict, opts)
        };
        match action {
            Action::Remove(reason) => plan.candidates.push(Candidate {
                entry: row.entry.clone(),
                reason,
                via_verdict: None,
            }),
            Action::Reclaim(reason) => plan.candidates.push(Candidate {
                entry: row.entry.clone(),
                reason,
                via_verdict: row.verdict_row.clone(),
            }),
            Action::Skip(reason) => plan.skipped.push(Candidate {
                entry: row.entry.clone(),
                reason,
                via_verdict: None,
            }),
            Action::Ignore => {}
        }
    }
    plan
}

fn lock_status_for_entry(entry: &WorktreeEntry, probe: &dyn LivenessProbe) -> LockStatus {
    if !entry.locked {
        return LockStatus::Unlocked;
    }
    let Some(reason) = entry.locked_reason.as_deref() else {
        return LockStatus::NoPid;
    };
    let Some(pid) = extract_pid_from_lock_reason(reason) else {
        return LockStatus::NoPid;
    };
    if probe.is_alive(pid) {
        LockStatus::LivePid(pid)
    } else {
        LockStatus::DeadPid(pid)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Remove(String),
    /// Issue #1606: the shared verdict says `reclaimable`; remove through the
    /// daemon's re-verifying executor instead of the ancestry path.
    Reclaim(String),
    Skip(String),
    /// Worktree didn't trip any criterion; don't print it in the skipped list.
    Ignore,
}

fn decide_action(inputs: StalenessInputs, opts: &CleanOptions) -> Action {
    let lock_prefix = match locked_removal_prefix(inputs) {
        Ok(prefix) => prefix,
        Err(reason) => return Action::Skip(reason),
    };
    let is_old = inputs.age >= opts.stale_after;
    let action = match inputs.status {
        WorktreeStatus::Clean => {
            if is_old {
                Action::Remove(format!("clean + stale ({})", fmt_age(inputs.age)))
            } else {
                Action::Ignore
            }
        }
        WorktreeStatus::BranchGone => {
            // Upstream is gone → always a removal candidate when clean-ish.
            // We treat branch-gone as already-implied-clean: the branch went
            // away on the remote, so even if we have local commits they're
            // unreachable from the remote.
            Action::Remove("upstream branch gone".to_string())
        }
        WorktreeStatus::NoUpstream => {
            if is_old {
                if opts.force {
                    Action::Remove(format!(
                        "no upstream + stale ({}) [--force]",
                        fmt_age(inputs.age)
                    ))
                } else {
                    Action::Skip("no upstream (use --force to remove)".to_string())
                }
            } else {
                Action::Ignore
            }
        }
        WorktreeStatus::Dirty => {
            if opts.force {
                Action::Remove("dirty [--force]".to_string())
            } else {
                Action::Skip("dirty (use --force to remove)".to_string())
            }
        }
        WorktreeStatus::Unpushed => {
            if opts.force {
                Action::Remove("unpushed commits [--force]".to_string())
            } else {
                Action::Skip("unpushed commits (use --force to remove)".to_string())
            }
        }
    };
    apply_lock_prefix(action, lock_prefix)
}

fn locked_removal_prefix(inputs: StalenessInputs) -> Result<Option<String>, String> {
    match inputs.lock_status {
        LockStatus::Unlocked => Ok(None),
        LockStatus::LivePid(pid) => {
            if inputs.age > inputs.locked_hard_age {
                Ok(Some(format!(
                    "stale lock (pid {pid} still alive; hard age exceeded)"
                )))
            } else {
                Err(format!("locked (live pid {pid})"))
            }
        }
        LockStatus::NoPid => {
            if inputs.age > inputs.locked_hard_age {
                Ok(Some("stale lock (no pid)".to_string()))
            } else {
                Err(format!(
                    "locked (no pid; hard age not reached: {} <= {})",
                    fmt_age(inputs.age),
                    fmt_age(inputs.locked_hard_age)
                ))
            }
        }
        LockStatus::DeadPid(pid) => {
            if inputs.age > inputs.locked_hard_age {
                Ok(Some(format!("stale lock (dead pid {pid})")))
            } else {
                Err(format!(
                    "locked (dead pid {pid}; hard age not reached: {} <= {})",
                    fmt_age(inputs.age),
                    fmt_age(inputs.locked_hard_age)
                ))
            }
        }
    }
}

fn apply_lock_prefix(action: Action, lock_prefix: Option<String>) -> Action {
    let Some(prefix) = lock_prefix else {
        return action;
    };
    match action {
        Action::Remove(reason) => Action::Remove(format!("{prefix}; {reason}")),
        Action::Reclaim(reason) => Action::Reclaim(format!("{prefix}; {reason}")),
        Action::Skip(reason) => Action::Skip(format!("{prefix}; {reason}")),
        Action::Ignore => Action::Ignore,
    }
}

#[path = "worktrees_verdict.rs"]
mod worktrees_verdict;
use worktrees_verdict::Tally;

#[path = "worktrees_locked_age.rs"]
mod worktrees_locked_age;
use worktrees_locked_age::locked_hard_age_from_env;

#[path = "worktrees_format.rs"]
mod worktrees_format;
pub(crate) use worktrees_format::fmt_age;

fn print_table(rows: &[Classified], _opts: &CleanOptions) {
    println!("Worktrees:");
    if rows.is_empty() {
        println!("  (none)");
        return;
    }
    let path_w = rows
        .iter()
        .map(|r| r.entry.path.to_string_lossy().len())
        .max()
        .unwrap_or(0)
        .max(4);
    println!(
        "  {:<path_w$}  {:<12}  {:<10}  NOTES",
        "PATH",
        "STATUS",
        "AGE",
        path_w = path_w
    );
    for r in rows {
        let verdict = r
            .verdict_row
            .as_ref()
            .filter(|_| !r.is_main)
            .map(|v| format!(" [{}: {}]", v.verdict.state.as_str(), v.verdict.reason))
            .unwrap_or_default();
        let notes = if r.is_main {
            "main".to_string()
        } else if r.entry.locked {
            "locked".to_string()
        } else if r.entry.prunable {
            "prunable".to_string()
        } else {
            String::new()
        };
        println!(
            "  {:<path_w$}  {:<12}  {:<10}  {}",
            r.entry.path.display(),
            r.status.as_str(),
            fmt_age(r.age),
            format!("{notes}{verdict}").trim(),
            path_w = path_w
        );
    }
}

fn confirm_interactive(n: usize) -> bool {
    print!(
        "{}",
        crate::yes_no::question(
            &format!("Remove {n} worktree(s)?"),
            crate::yes_no::Default::No
        )
    );
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    crate::yes_no::answer(&line, crate::yes_no::Default::No)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemovalOutcome {
    GitRemove,
    FallbackAfterGitSuccess,
    FallbackAfterGitFailure,
    PrunedAfterGitFailure,
}

impl RemovalOutcome {
    fn note(self) -> &'static str {
        match self {
            Self::GitRemove => "",
            Self::FallbackAfterGitSuccess => " (direct fallback after git reported success)",
            Self::FallbackAfterGitFailure => " (direct fallback after git remove failed)",
            Self::PrunedAfterGitFailure => " (path already gone; pruned stale git metadata)",
        }
    }
}

pub(crate) fn remove_worktree_path(
    main_repo: &Path,
    path: &Path,
    force: bool,
) -> Result<&'static str, String> {
    remove_worktree_path_if_unused(
        main_repo,
        path,
        force,
        &repo_worktree_cli::source_reclamation_reason,
    )
}

fn remove_worktree_path_if_unused(
    main_repo: &Path,
    path: &Path,
    force: bool,
    source_probe: &dyn Fn(&Path) -> Option<&'static str>,
) -> Result<&'static str, String> {
    if path.try_exists().unwrap_or(true) {
        if let Some(reason) = source_probe(path) {
            return Err(reason.to_string());
        }
    }
    remove_worktree_path_with_git_and_process_refs(
        main_repo,
        path,
        force,
        run_git,
        live_process_refs_for_worktree,
    )
    .map(RemovalOutcome::note)
}

#[cfg(test)]
fn remove_worktree_path_with_git<F>(
    main_repo: &Path,
    path: &Path,
    force: bool,
    git: F,
) -> Result<RemovalOutcome, String>
where
    F: Fn(&Path, &[&str]) -> Result<String, String>,
{
    remove_worktree_path_with_git_and_process_refs(main_repo, path, force, git, |_| Vec::new())
}

fn remove_worktree_path_with_git_and_process_refs<F, P>(
    main_repo: &Path,
    path: &Path,
    force: bool,
    git: F,
    process_refs: P,
) -> Result<RemovalOutcome, String>
where
    F: Fn(&Path, &[&str]) -> Result<String, String>,
    P: Fn(&Path) -> Vec<WorktreeProcessRef>,
{
    ensure_no_live_process_refs(path, &process_refs)?;
    crate::gc::delete_audit::record_required(
        "worktree.git-remove",
        path,
        &format!(
            "repo={} force={force}; live-process guard passed",
            main_repo.display()
        ),
    )
    .map_err(|error| format!("refusing unaudited worktree removal: {error}"))?;
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    let path_str = path.to_string_lossy().to_string();
    args.push(&path_str);

    match git(main_repo, &args) {
        Ok(_) => {
            if !path_try_exists(path)? {
                return Ok(RemovalOutcome::GitRemove);
            }
            if !force {
                return Err("git reported removal but source is still present; refusing recursive fallback without --force".into());
            }
            fallback_remove_and_prune(main_repo, path, &git, &process_refs).map_err(
                |fallback_err| {
                    format!(
                        "git worktree remove reported success but {} still exists; fallback failed: {fallback_err}",
                        path.display()
                    )
                },
            )?;
            Ok(RemovalOutcome::FallbackAfterGitSuccess)
        }
        Err(git_err) => {
            if path_try_exists(path)? {
                if !force {
                    return Err(git_err);
                }
                fallback_remove_and_prune(main_repo, path, &git, &process_refs).map_err(
                    |fallback_err| {
                        format!(
                            "git worktree remove failed: {git_err}; fallback failed: {fallback_err}"
                        )
                    },
                )?;
                Ok(RemovalOutcome::FallbackAfterGitFailure)
            } else {
                best_effort_unlock_and_prune_worktree(main_repo, path, &git);
                Ok(RemovalOutcome::PrunedAfterGitFailure)
            }
        }
    }
}

fn fallback_remove_and_prune<F, P>(
    main_repo: &Path,
    path: &Path,
    git: &F,
    process_refs: &P,
) -> Result<(), String>
where
    F: Fn(&Path, &[&str]) -> Result<String, String>,
    P: Fn(&Path) -> Vec<WorktreeProcessRef>,
{
    ensure_no_live_process_refs(path, process_refs)?;
    if path_try_exists(path)? {
        // Audit before acting (#893): this fallback physically removes a
        // worktree after the git-level removal failed.
        crate::gc::delete_audit::record_required(
            "worktree.fallback-remove",
            path,
            "git-worktree fallback prune; fresh live-process guard passed",
        )
        .map_err(|error| format!("refusing unaudited fallback removal: {error}"))?;
        match std::fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!("remove_dir_all({}): {e}", path.display()));
            }
        }
    }
    if path_try_exists(path)? {
        return Err(format!(
            "{} still exists after remove_dir_all",
            path.display()
        ));
    }
    best_effort_unlock_and_prune_worktree(main_repo, path, git);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeProcessRef {
    pid: u32,
    parent_pid: Option<u32>,
    name: String,
    command: String,
    exe: Option<PathBuf>,
    cwd: Option<PathBuf>,
}

fn ensure_no_live_process_refs<P>(path: &Path, process_refs: &P) -> Result<(), String>
where
    P: Fn(&Path) -> Vec<WorktreeProcessRef>,
{
    let refs = process_refs(path);
    if refs.is_empty() {
        return Ok(());
    }
    Err(format!(
        "live process(es) still reference worktree {}: {}. Wait for useful verification to finish, or stop only the exact abandoned/timed-out process tree before cleanup.",
        path.display(),
        format_process_refs(&refs)
    ))
}

fn format_process_refs(refs: &[WorktreeProcessRef]) -> String {
    let mut parts: Vec<String> = refs
        .iter()
        .take(5)
        .map(|r| {
            let parent = r
                .parent_pid
                .map(|pid| format!(", parent {pid}"))
                .unwrap_or_default();
            let exe = r
                .exe
                .as_ref()
                .map(|p| format!(", exe {}", p.display()))
                .unwrap_or_default();
            let cwd = r
                .cwd
                .as_ref()
                .map(|p| format!(", cwd {}", p.display()))
                .unwrap_or_default();
            format!(
                "pid {}{parent}, name {}{exe}{cwd}, cmd {}",
                r.pid, r.name, r.command
            )
        })
        .collect();
    if refs.len() > parts.len() {
        parts.push(format!("and {} more", refs.len() - parts.len()));
    }
    parts.join("; ")
}

fn live_process_refs_for_worktree(path: &Path) -> Vec<WorktreeProcessRef> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    system
        .processes()
        .values()
        .filter_map(|process| {
            let cmd: Vec<OsString> = process.cmd().to_vec();
            let exe = process.exe().map(PathBuf::from);
            let cwd = process.cwd().map(PathBuf::from);
            if !process_ref_matches_worktree(&target, exe.as_deref(), cwd.as_deref(), &cmd) {
                return None;
            }
            Some(WorktreeProcessRef {
                pid: process.pid().as_u32(),
                parent_pid: process.parent().map(|pid| pid.as_u32()),
                name: process.name().to_string_lossy().into_owned(),
                command: cmd
                    .iter()
                    .map(|part| part.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                exe,
                cwd,
            })
        })
        .collect()
}

fn process_ref_matches_worktree(
    path: &Path,
    exe: Option<&Path>,
    cwd: Option<&Path>,
    cmd: &[OsString],
) -> bool {
    let needle = normalize_process_match_text(&path.to_string_lossy());
    if needle.is_empty() {
        return false;
    }
    if exe
        .map(|p| normalize_process_match_text(&p.to_string_lossy()).contains(&needle))
        .unwrap_or(false)
    {
        return true;
    }
    if cwd
        .map(|p| normalize_process_match_text(&p.to_string_lossy()).contains(&needle))
        .unwrap_or(false)
    {
        return true;
    }
    cmd.iter()
        .map(|part| normalize_process_match_text(&part.to_string_lossy()))
        .any(|part| part.contains(&needle))
}

fn normalize_process_match_text(value: &str) -> String {
    let value = slash_separators(value);
    let value = value.strip_prefix("//?/").unwrap_or(&value);
    if crate::platform::host::WINDOWS {
        value.to_ascii_lowercase()
    } else {
        value.to_owned()
    }
}

fn best_effort_unlock_and_prune_worktree<F>(main_repo: &Path, path: &Path, git: &F)
where
    F: Fn(&Path, &[&str]) -> Result<String, String>,
{
    let path_str = path.to_string_lossy().to_string();
    let _ = git(main_repo, &["worktree", "unlock", &path_str]);
    let _ = git(main_repo, &["worktree", "prune", "--expire=now"]);
}

fn path_try_exists(path: &Path) -> Result<bool, String> {
    path.try_exists()
        .map_err(|e| format!("checking whether {} exists: {e}", path.display()))
}

// ---------- git-side helpers ----------

/// Returns the absolute path of the **main** worktree (the directory that owns
/// `.git/worktrees/`). When called from inside any worktree of the same repo,
/// `git worktree list --porcelain` from that worktree still reports the main
/// worktree first, so we can use the current directory as a starting point.
pub fn locate_main_repo_root() -> Result<PathBuf, String> {
    locate_main_repo_root_from(Path::new("."))
}

/// Same as [`locate_main_repo_root`], but resolved relative to an arbitrary
/// `start` directory instead of the process's current directory
/// (zackees/clud#532: `cmd-scan`'s per-hook-invocation repo-root lookup needs
/// to resolve from the tool call's reported `cwd`, which can differ from the
/// hook process's own cwd).
pub fn locate_main_repo_root_from(start: &Path) -> Result<PathBuf, String> {
    let out = run_git(
        start,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(out.trim());
    // `--git-common-dir` points at `<main>/.git` (or a bare repo dir). The
    // main worktree is its parent. If the path's filename is `.git`, take
    // the parent; otherwise the repo itself is bare and we use the dir.
    let main_root = if common.file_name().and_then(|n| n.to_str()) == Some(".git") {
        common
            .parent()
            .ok_or_else(|| format!("could not derive repo root from {}", common.display()))?
            .to_path_buf()
    } else {
        common
    };
    Ok(main_root)
}

/// Run `git <args>` in `cwd` and return its stdout on success.
///
/// All subprocess spawning in `clud` flows through `running-process-core`
/// (enforced by `ci/banned_imports.py`). We capture combined stdout/stderr
/// so the error path can surface git's diagnostic on failure.
pub(crate) fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let config = ProcessConfig {
        command: subprocess::command_spec_for_subprocess(argv.clone()),
        cwd: Some(cwd.to_path_buf()),
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        // git is a piped helper; suppress the conhost popup on Windows.
        creationflags: invisible_helper_creationflags(),
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new(config);
    process
        .start()
        .map_err(|e| format!("failed to start git: {e}"))?;

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
        .map_err(|e| format!("waiting for git: {e}"))?;
    for event in process.drain_combined() {
        buf.extend_from_slice(&event.line);
        buf.push(b'\n');
    }
    let output = String::from_utf8_lossy(&buf).into_owned();
    if exit_code != 0 {
        return Err(format!(
            "git {} failed (exit {}): {}",
            args.join(" "),
            exit_code,
            output.trim()
        ));
    }
    Ok(output)
}

/// Classify a single worktree's working tree + upstream state.
///
/// Returns `Dirty` if `git status --porcelain` is non-empty (regardless of
/// upstream state), otherwise checks upstream:
/// - `[gone]` upstream → `BranchGone`
/// - no upstream configured → `NoUpstream`
/// - `@{u}..HEAD` has commits → `Unpushed`
/// - everything in order → `Clean`
///
/// Any unexpected git error is treated as `Dirty` (safer default — we'd
/// rather skip than mistakenly remove).
pub fn classify_status(path: &Path) -> WorktreeStatus {
    // Dirty check first — cheap and definitive.
    match run_git(path, &["status", "--porcelain"]) {
        Ok(out) if !out.trim().is_empty() => return WorktreeStatus::Dirty,
        Ok(_) => {}
        Err(_) => return WorktreeStatus::Dirty,
    }

    // Branch-gone check via `git for-each-ref` over the current branch.
    // `git rev-parse --abbrev-ref HEAD` yields the branch name; "HEAD"
    // means detached.
    let branch = match run_git(path, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return WorktreeStatus::Dirty,
    };
    if branch == "HEAD" || branch.is_empty() {
        return WorktreeStatus::NoUpstream;
    }

    // Use `git for-each-ref --format='%(upstream:track)'` to detect [gone].
    // The format produces literally "[gone]" if the upstream has been
    // deleted, "[ahead N]" / "[behind N]" / empty otherwise.
    let track = run_git(
        path,
        &[
            "for-each-ref",
            "--format=%(upstream:track)",
            &format!("refs/heads/{}", branch),
        ],
    )
    .unwrap_or_default();
    if track.contains("gone") {
        return WorktreeStatus::BranchGone;
    }

    // Does the branch have an upstream at all? `rev-parse @{u}` errors when
    // there's no upstream — that's the "no-upstream" signal.
    if run_git(path, &["rev-parse", "--abbrev-ref", "@{u}"]).is_err() {
        return WorktreeStatus::NoUpstream;
    }

    // Unpushed commits?
    match run_git(path, &["rev-list", "--count", "@{u}..HEAD"]) {
        Ok(out) => {
            let n: u64 = out.trim().parse().unwrap_or(0);
            if n > 0 {
                WorktreeStatus::Unpushed
            } else {
                WorktreeStatus::Clean
            }
        }
        Err(_) => WorktreeStatus::Clean,
    }
}

fn path_age(path: &Path) -> Option<Duration> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    SystemTime::now().duration_since(mtime).ok()
}

// ---------- porcelain parser ----------

/// Parse the output of `git worktree list --porcelain`.
///
/// Format (one block per worktree, separated by blank lines):
///
/// ```text
/// worktree <path>
/// HEAD <sha>
/// branch refs/heads/<name>
/// [bare]
/// [detached]
/// [locked [reason...]]
/// [prunable [reason...]]
/// ```
pub fn parse_worktree_porcelain(raw: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(e) = current.take() {
                out.push(e);
            }
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        match key {
            "worktree" => {
                if let Some(e) = current.take() {
                    out.push(e);
                }
                current = Some(WorktreeEntry {
                    path: PathBuf::from(value),
                    head: None,
                    branch: None,
                    bare: false,
                    detached: false,
                    locked: false,
                    locked_reason: None,
                    prunable: false,
                });
            }
            "HEAD" => {
                if let Some(c) = current.as_mut() {
                    c.head = Some(value.to_string());
                }
            }
            "branch" => {
                if let Some(c) = current.as_mut() {
                    let name = value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string();
                    c.branch = Some(name);
                }
            }
            "bare" => {
                if let Some(c) = current.as_mut() {
                    c.bare = true;
                }
            }
            "detached" => {
                if let Some(c) = current.as_mut() {
                    c.detached = true;
                }
            }
            "locked" => {
                if let Some(c) = current.as_mut() {
                    c.locked = true;
                    let reason = value.trim();
                    if !reason.is_empty() {
                        c.locked_reason = Some(reason.to_string());
                    }
                }
            }
            "prunable" => {
                if let Some(c) = current.as_mut() {
                    c.prunable = true;
                }
            }
            _ => {}
        }
    }
    if let Some(e) = current.take() {
        out.push(e);
    }
    out
}

// ---------- duration parser ----------

#[path = "worktrees_duration.rs"]
mod worktrees_duration;
pub use worktrees_duration::parse_duration;

#[cfg(test)]
#[path = "worktrees_tests.rs"]
mod tests;

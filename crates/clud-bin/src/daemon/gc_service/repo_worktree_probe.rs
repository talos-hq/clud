//! Issue #1591: gather [`RepoWorktreeFacts`] for every worktree of the repos
//! the GC registry knows about, and turn them into verdicts.
//!
//! Runs only on the `clud-gc-repo-worktree-probe` thread, never on the
//! registry worker (#946): every query is a bounded `git`/`gh` spawn through
//! `running-process` (`extern_repo::probe_cmd`). A query that fails or times
//! out yields `None`/`Unavailable`, which the verdict treats as doubt and
//! spares. Read-only: nothing here mutates a repository; the #1603
//! executor (`repo_worktree_reclaim_exec`) re-runs [`probe_one`] immediately
//! before it deletes anything.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::Deserialize;

use crate::gc::extract_pid_from_lock_reason;
use crate::gc::worktree_root::is_under_worktree_root;
use crate::session_registry::{LivenessProbe, OsLivenessProbe};
use crate::worktrees::{parse_worktree_porcelain, WorktreeEntry};

use super::extern_repo::{git_discovery_env_is_poisoned, probe_cmd};
use super::repo_worktree::{
    repo_worktree_verdict, reserved_dir_verdict, PrFact, RepoWorktreeFacts, RepoWorktreeVerdict,
    ReservedDirFacts,
};
use super::repo_worktree_reclaim::{own_git_helpers, process_inside, ProcessCwdSnapshot};

/// `clud gc list` kind for these rows. Not a registry kind: no redb row
/// backs them, so no purge path can select them.
pub(crate) const REPO_WORKTREE_KIND: &str = "repo-worktree";

const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// `gh` talks to the network; give it longer, still bounded.
const GH_TIMEOUT: Duration = Duration::from_secs(20);
/// Cap on default-branch commits scanned for a patch match per worktree.
const PATCH_SCAN_LIMIT: &str = "100";

/// One probed worktree, as cached for `clud gc list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoWorktreeRow {
    pub(crate) path: String,
    pub(crate) repo_root: String,
    pub(crate) branch: Option<String>,
    /// `HEAD` as the probe saw it; `None` when not read (a guard spared the
    /// row before the tip mattered). The reclaim re-check requires it
    /// unchanged (#1603).
    pub(crate) tip: Option<String>,
    /// Directory mtime, standing in for a creation time in `gc list`.
    pub(crate) mtime_unix: i64,
    pub(crate) verdict: RepoWorktreeVerdict,
    /// Issue #1486: an unclaimed `worktrees` child judged by
    /// [`reserved_dir_verdict`], not a git worktree. `repo_root` is empty
    /// and the executor removes it with `remove_dir`, never git.
    pub(crate) reservation: bool,
}

/// One row of `gh pr list --json number,state,headRefName,headRefOid`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct PrRecord {
    pub(crate) number: u64,
    pub(crate) state: String,
    #[serde(rename = "headRefName")]
    pub(crate) head_ref_name: String,
    #[serde(rename = "headRefOid")]
    pub(crate) head_ref_oid: String,
}

/// Pure: fold every PR whose head is `branch` into one [`PrFact`].
///
/// `prs == None` means the lookup was unavailable. An open PR wins over any
/// merged one, because branch names get reused. Among merged PRs, any whose
/// head covers the tip proves the work landed; otherwise an undeterminable
/// coverage beats a definite "not covered" (both spare, but the reason
/// should not claim more than we know).
pub(crate) fn pr_fact_for_branch(
    branch: &str,
    prs: Option<&[PrRecord]>,
    covered: &mut dyn FnMut(&str) -> Option<bool>,
) -> PrFact {
    let Some(prs) = prs else {
        return PrFact::Unavailable;
    };
    let mine: Vec<&PrRecord> = prs.iter().filter(|p| p.head_ref_name == branch).collect();
    if let Some(open) = mine.iter().find(|p| p.state.eq_ignore_ascii_case("OPEN")) {
        return PrFact::Open {
            number: open.number,
        };
    }
    let mut merged: Vec<&&PrRecord> = mine
        .iter()
        .filter(|p| p.state.eq_ignore_ascii_case("MERGED"))
        .collect();
    if merged.is_empty() {
        // #1961: a closed, unmerged PR is named rather than read as "no PR".
        return mine
            .iter()
            .filter(|p| p.state.eq_ignore_ascii_case("CLOSED"))
            .map(|p| p.number)
            .max()
            .map_or(PrFact::NoPr, |number| PrFact::Closed { number });
    }
    merged.sort_by_key(|p| std::cmp::Reverse(p.number));
    let mut first_unknown = None;
    let mut first_uncovered = None;
    for pr in &merged {
        match covered(&pr.head_ref_oid) {
            Some(true) => {
                return PrFact::Merged {
                    number: pr.number,
                    tip_covered: Some(true),
                }
            }
            None => {
                first_unknown.get_or_insert(pr.number);
            }
            Some(false) => {
                first_uncovered.get_or_insert(pr.number);
            }
        }
    }
    match (first_unknown, first_uncovered) {
        (Some(number), _) => PrFact::Merged {
            number,
            tip_covered: None,
        },
        (None, Some(number)) => PrFact::Merged {
            number,
            tip_covered: Some(false),
        },
        (None, None) => PrFact::NoPr,
    }
}

/// Pure: a `git diff -U0` rendering with the parts that differ between two
/// textually identical changes removed (blob ids and hunk line numbers), so
/// the branch's cumulative change can be compared with a squash commit.
pub(crate) fn normalize_diff(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.starts_with("index ") && !l.starts_with("@@"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    match probe_cmd("git", cwd, args, GIT_TIMEOUT)? {
        (0, out) => Some(out),
        _ => None,
    }
}

/// Look up this repo's PRs once per tick. `None` unless the origin is a
/// GitHub remote and `gh` answers with parseable JSON in time.
pub(crate) fn lookup_prs(repo_root: &Path) -> Option<Vec<PrRecord>> {
    lookup_prs_with_timeout(repo_root, GH_TIMEOUT)
}

/// [`lookup_prs`] with a caller-chosen `gh` timeout: the interactive
/// `--clean-worktrees` uses a short one (#1648).
pub(crate) fn lookup_prs_with_timeout(
    repo_root: &Path,
    gh_timeout: Duration,
) -> Option<Vec<PrRecord>> {
    let url = git(repo_root, &["remote", "get-url", "origin"])?;
    if !url.contains("github.com") {
        return None;
    }
    let (code, out) = probe_cmd(
        "gh",
        repo_root,
        &[
            "pr",
            "list",
            "--state",
            "all",
            "--limit",
            "500",
            "--json",
            "number,state,headRefName,headRefOid",
        ],
        gh_timeout,
    )?;
    if code != 0 {
        return None;
    }
    serde_json::from_str(&out).ok()
}

pub(super) fn default_ref(cwd: &Path) -> Option<String> {
    if let Some(name) = git(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        let name = name.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    ["origin/main", "origin/master"]
        .into_iter()
        .find(|c| git(cwd, &["rev-parse", "--verify", "--quiet", c]).is_some())
        .map(str::to_string)
}

/// `(dirty, untracked)` from one porcelain status call.
pub(super) fn work_tree_status(cwd: &Path) -> (Option<bool>, Option<bool>) {
    let Some(out) = git(
        cwd,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
        ],
    ) else {
        return (None, None);
    };
    let mut dirty = false;
    let mut untracked = false;
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        if line.starts_with("??") {
            untracked = true;
        } else {
            dirty = true;
        }
    }
    (Some(dirty), Some(untracked))
}

/// Whether `tip` is `head` or an ancestor of it. `None` when git cannot say
/// (for example `head` is not present locally).
pub(super) fn tip_covered_by(cwd: &Path, tip: &str, head: &str) -> Option<bool> {
    if tip == head {
        return Some(true);
    }
    match probe_cmd(
        "git",
        cwd,
        &["merge-base", "--is-ancestor", tip, head],
        GIT_TIMEOUT,
    )? {
        (0, _) => Some(true),
        (1, _) => Some(false),
        _ => None,
    }
}

/// Fallback evidence: the branch's cumulative change since its merge-base
/// appears verbatim as one commit on the default branch (a squash merge).
/// `Some(false)` for a branch with no commits of its own.
/// `(upstream, merge-base, commits ahead)` of `tip` against the default
/// branch. `None` when any query fails.
pub(super) fn ahead_of_default(cwd: &Path, tip: &str) -> Option<(String, String, u64)> {
    let upstream = default_ref(cwd)?;
    let base = git(cwd, &["merge-base", &upstream, tip])?
        .trim()
        .to_string();
    let ahead = git(cwd, &["rev-list", "--count", &format!("{base}..{tip}")])?;
    let ahead = ahead.trim().parse::<u64>().ok()?;
    Some((upstream, base, ahead))
}

fn patch_landed(cwd: &Path, tip: &str) -> Option<bool> {
    patch_landed_with(cwd, tip, &mut |commit| commit_diff(cwd, commit))
}

/// One default-branch commit's change, normalized for [`patch_landed_with`].
pub(super) fn commit_diff(cwd: &Path, commit: &str) -> Option<String> {
    git(
        cwd,
        &[
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            &format!("{commit}^"),
            commit,
        ],
    )
    .map(|diff| normalize_diff(&diff))
}

/// [`patch_landed`] with the per-commit diff injected, so a caller judging
/// many branches against one default branch (`repo_branch_probe`) diffs each
/// candidate commit once instead of once per branch.
pub(super) fn patch_landed_with(
    cwd: &Path,
    tip: &str,
    commit_diff: &mut dyn FnMut(&str) -> Option<String>,
) -> Option<bool> {
    let (upstream, base, ahead) = ahead_of_default(cwd, tip)?;
    let base = base.as_str();
    if ahead == 0 {
        return Some(false);
    }
    let branch_diff = normalize_diff(&git(
        cwd,
        &["diff", "-U0", "--no-color", "--no-ext-diff", base, tip],
    )?);
    if branch_diff.trim().is_empty() {
        return Some(false);
    }
    let candidates = git(
        cwd,
        &[
            "rev-list",
            "--no-merges",
            "--max-count",
            PATCH_SCAN_LIMIT,
            &format!("{base}..{upstream}"),
        ],
    )?;
    Some(
        candidates
            .split_whitespace()
            .any(|commit| commit_diff(commit).is_some_and(|diff| diff == branch_diff)),
    )
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn locked_by_live_or_unknown_pid(entry: &WorktreeEntry) -> bool {
    if !entry.locked {
        return false;
    }
    // A lock with no parseable pid is doubt, and doubt spares.
    match entry
        .locked_reason
        .as_deref()
        .and_then(extract_pid_from_lock_reason)
    {
        Some(pid) => OsLivenessProbe.is_alive(pid),
        None => true,
    }
}

fn mtime_unix(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Issue #1603: the full process table's cwds, the producer half of the
/// `ProcessFacts` approach (process-reaping.md). The decision
/// ([`process_inside`]) only ever sees this data.
///
/// Trust check: if our own process's cwd cannot be read, cwd reading does not
/// work here at all, and an empty answer would read as "nobody inside" — so
/// the snapshot is marked unavailable and every worktree is spared. A single
/// process whose cwd is unreadable (typically another user's) is skipped.
pub(crate) fn collect_process_cwds() -> ProcessCwdSnapshot {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
    );
    let own = sysinfo::Pid::from_u32(std::process::id());
    let own_cwd_readable = system
        .process(own)
        .and_then(|p| p.cwd())
        .is_some_and(|c| !c.as_os_str().is_empty());
    if !own_cwd_readable {
        return ProcessCwdSnapshot::default();
    }
    let table: Vec<(u32, Option<u32>, String)> = system
        .processes()
        .values()
        .map(|p| {
            (
                p.pid().as_u32(),
                p.parent().map(|pp| pp.as_u32()),
                p.name().to_string_lossy().into_owned(),
            )
        })
        .collect();
    let helpers = own_git_helpers(&table, std::process::id());
    let mut cwds: Vec<PathBuf> = system
        .processes()
        .values()
        .filter(|p| !helpers.contains(&p.pid().as_u32()))
        .filter_map(|p| p.cwd())
        .filter(|c| !c.as_os_str().is_empty())
        .map(canonical)
        .collect();
    cwds.sort();
    cwds.dedup();
    ProcessCwdSnapshot {
        available: true,
        cwds,
    }
}

/// Gather facts for one worktree entry. `prs` is the repo's lookup result.
/// Returns the facts plus the `HEAD` it read, if it got that far.
///
/// `prs` is called only when a worktree gets as far as needing PR facts,
/// so a caller can defer the network lookup until then (#1648).
fn facts_for<'p>(
    entry: &WorktreeEntry,
    is_main: bool,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    prs: &dyn Fn() -> Option<&'p [PrRecord]>,
    wt_root: Option<&Path>,
) -> (RepoWorktreeFacts, Option<String>) {
    let path = &entry.path;
    let path_exists = path.try_exists().unwrap_or(true);
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mtime = mtime_unix(path);
    let age_secs = (mtime > 0 && now >= mtime).then(|| (now - mtime) as u64);
    let canon = canonical(path);
    let in_table = process_inside(procs, &canon);
    let process_inside =
        live_cwds.iter().any(|cwd| cwd.starts_with(&canon)) || in_table == Some(true);
    let mut facts = RepoWorktreeFacts {
        evaluated: true,
        path_exists,
        is_main_checkout: is_main,
        locked_live_pid: locked_by_live_or_unknown_pid(entry),
        process_inside,
        processes_unverifiable: in_table.is_none(),
        detached: entry.detached || entry.branch.is_none(),
        dirty: None,
        untracked: None,
        pr: PrFact::Unavailable,
        patch_landed: None,
        under_worktree_root: wt_root.is_some_and(|root| is_under_worktree_root(path, root)),
        commits_ahead: None,
        age_secs,
    };
    // Every cheaper guard that already spares makes the rest moot.
    if !path_exists
        || facts.is_main_checkout
        || facts.locked_live_pid
        || facts.process_inside
        || facts.processes_unverifiable
        || facts.detached
    {
        return (facts, None);
    }
    let (dirty, untracked) = work_tree_status(path);
    facts.dirty = dirty;
    facts.untracked = untracked;
    if dirty != Some(false) || untracked != Some(false) {
        return (facts, None);
    }
    let Some(tip) = git(path, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string()) else {
        facts.dirty = None;
        return (facts, None);
    };
    let branch = entry
        .branch
        .as_deref()
        .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
        .unwrap_or_default();
    facts.pr = pr_fact_for_branch(branch, prs(), &mut |head| tip_covered_by(path, &tip, head));
    // #1961: the patch match also decides a merged PR whose head does not
    // cover the tip (rebased before its squash merge).
    if !matches!(
        facts.pr,
        PrFact::Open { .. }
            | PrFact::Merged {
                tip_covered: Some(true),
                ..
            }
    ) {
        facts.patch_landed = patch_landed(path, &tip);
    }
    if matches!(
        facts.pr,
        PrFact::NoPr | PrFact::Unavailable | PrFact::Closed { .. }
    ) {
        facts.commits_ahead = ahead_of_default(path, &tip).map(|(_, _, n)| n);
    }
    (facts, Some(tip))
}

/// Probe every worktree of `repo_root` with an all-clear process table.
/// Test-only convenience over [`probe_repo_with`]; production always passes
/// a real [`collect_process_cwds`] snapshot.
#[cfg(test)]
pub(crate) fn probe_repo(
    repo_root: &Path,
    live_cwds: &[PathBuf],
    prs: Option<&[PrRecord]>,
) -> Vec<RepoWorktreeRow> {
    let procs = ProcessCwdSnapshot {
        available: true,
        cwds: Vec::new(),
    };
    probe_repo_with(repo_root, live_cwds, &procs, prs, None)
}

/// Probe every worktree of `repo_root`. `prs` is injected so tests can
/// exercise both the merged-PR path and the `gh`-unavailable fallback, and
/// `wt_root` (#1485, production: `~/.clud/worktrees`) so tests never read the
/// real home.
pub(crate) fn probe_repo_with(
    repo_root: &Path,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    prs: Option<&[PrRecord]>,
    wt_root: Option<&Path>,
) -> Vec<RepoWorktreeRow> {
    probe_entries(repo_root, None, live_cwds, procs, prs, wt_root)
}

/// Re-probe exactly one worktree from scratch (#1603's pre-delete re-check).
/// `None` when `git worktree list` no longer shows it.
pub(crate) fn probe_one(
    repo_root: &Path,
    worktree: &Path,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    prs: Option<&[PrRecord]>,
    wt_root: Option<&Path>,
) -> Option<RepoWorktreeRow> {
    probe_entries(repo_root, Some(worktree), live_cwds, procs, prs, wt_root)
        .into_iter()
        .next()
}

fn probe_entries(
    repo_root: &Path,
    only: Option<&Path>,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    prs: Option<&[PrRecord]>,
    wt_root: Option<&Path>,
) -> Vec<RepoWorktreeRow> {
    let mut rows = Vec::new();
    probe_entries_each(
        repo_root,
        only,
        live_cwds,
        procs,
        &|| prs,
        wt_root,
        &mut |row| rows.push(row),
    );
    rows
}

/// Issue #1648: [`probe_repo_with`] for the interactive CLI. Rows are handed
/// to `sink` one at a time as each worktree finishes, so a caller with a
/// deadline keeps every verdict that arrived in time, and `prs` is only
/// called if some worktree actually needs PR facts.
pub(crate) fn probe_repo_each<'p>(
    repo_root: &Path,
    procs: &ProcessCwdSnapshot,
    prs: &dyn Fn() -> Option<&'p [PrRecord]>,
    wt_root: Option<&Path>,
    sink: &mut dyn FnMut(RepoWorktreeRow),
) {
    probe_entries_each(repo_root, None, &[], procs, prs, wt_root, sink);
}

fn probe_entries_each<'p>(
    repo_root: &Path,
    only: Option<&Path>,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    prs: &dyn Fn() -> Option<&'p [PrRecord]>,
    wt_root: Option<&Path>,
    sink: &mut dyn FnMut(RepoWorktreeRow),
) {
    if git_discovery_env_is_poisoned() {
        return;
    }
    let Some(raw) = git(repo_root, &["worktree", "list", "--porcelain"]) else {
        return;
    };
    let entries = parse_worktree_porcelain(&raw);
    let main_path = entries.first().map(|e| canonical(&e.path));
    let live_cwds: Vec<PathBuf> = live_cwds.iter().map(|p| canonical(p)).collect();
    let only = only.map(canonical);
    entries
        .iter()
        .filter(|e| !e.bare)
        .filter(|e| {
            only.as_deref()
                .is_none_or(|want| canonical(&e.path) == want)
        })
        .for_each(|entry| {
            let is_main = main_path.as_deref() == Some(canonical(&entry.path).as_path());
            let (facts, tip) = facts_for(entry, is_main, &live_cwds, procs, prs, wt_root);
            sink(RepoWorktreeRow {
                path: entry.path.to_string_lossy().to_string(),
                repo_root: main_path
                    .as_deref()
                    .unwrap_or(repo_root)
                    .to_string_lossy()
                    .to_string(),
                branch: entry
                    .branch
                    .as_deref()
                    .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_string()),
                tip,
                mtime_unix: mtime_unix(&entry.path),
                verdict: repo_worktree_verdict(&facts),
                reservation: false,
            });
        });
}

/// #1485: every direct child of the worktree root, as candidate repo roots.
/// A worktree there resolves (via `--git-common-dir`) to its main repo, so
/// its whole repo is probed even if clud never recorded a visit to it. The
/// root itself is never returned, so it can never become a target.
pub(crate) fn worktree_root_children(wt_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(wt_root) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

/// Probe every visited repo, one `gh` lookup per repo, deduplicating repos
/// that are worktrees of one another and rows reached through two roots.
/// Worktrees under `wt_root` also seed their repos (#1485).
pub(crate) fn probe_repos(
    repo_roots: &[String],
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    wt_root: Option<&Path>,
) -> Vec<RepoWorktreeRow> {
    let mut repo_roots = repo_roots.to_vec();
    if let Some(root) = wt_root {
        repo_roots.extend(worktree_root_children(root));
    }
    let mut seen_roots = HashSet::new();
    let mut out = Vec::new();
    for root in &repo_roots {
        out.extend(probe_one_root(
            Path::new(root),
            live_cwds,
            procs,
            wt_root,
            &mut seen_roots,
        ));
    }
    with_reservations(dedupe_rows(out), live_cwds, procs, wt_root)
}

/// #2103: probe one queued root. Empty when it is missing, not a repo, or a
/// repo another root in this pass already covered (same common dir).
pub(crate) fn probe_one_root(
    root: &Path,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    wt_root: Option<&Path>,
    seen_roots: &mut HashSet<PathBuf>,
) -> Vec<RepoWorktreeRow> {
    if !root.try_exists().unwrap_or(false) {
        return Vec::new();
    }
    let Some(common) = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        return Vec::new();
    };
    if !seen_roots.insert(canonical(Path::new(common.trim()))) {
        return Vec::new();
    }
    let prs = lookup_prs(root);
    probe_repo_with(root, live_cwds, procs, prs.as_deref(), wt_root)
}

/// First row per path wins.
pub(crate) fn dedupe_rows(rows: Vec<RepoWorktreeRow>) -> Vec<RepoWorktreeRow> {
    let mut seen = HashSet::new();
    rows.into_iter()
        .filter(|row| seen.insert(row.path.clone()))
        .collect()
}

/// #1486: every worktrees child no worktree list claimed is judged as a
/// reservation, so an unused `alloc_wt_path` directory is not leaked.
pub(crate) fn with_reservations(
    mut out: Vec<RepoWorktreeRow>,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    wt_root: Option<&Path>,
) -> Vec<RepoWorktreeRow> {
    out.retain(|row| !row.reservation);
    if let Some(root) = wt_root {
        let claimed: HashSet<PathBuf> = out.iter().map(|r| canonical(Path::new(&r.path))).collect();
        for child in worktree_root_children(root) {
            let child = Path::new(&child);
            if !claimed.contains(&canonical(child)) {
                out.push(probe_reservation(child, live_cwds, procs));
            }
        }
    }
    out
}

/// Issue #1486: judge one unclaimed `worktrees` child. Pure over the
/// filesystem and the injected process snapshot; runs no git. The #1603
/// executor calls it again immediately before removing anything.
pub(crate) fn probe_reservation(
    path: &Path,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
) -> RepoWorktreeRow {
    let row = |verdict| RepoWorktreeRow {
        path: path.to_string_lossy().to_string(),
        repo_root: String::new(),
        branch: None,
        tip: None,
        mtime_unix: mtime_unix(path),
        verdict,
        reservation: true,
    };
    let path_exists = path.try_exists().unwrap_or(true);
    let canon = canonical(path);
    let in_table = process_inside(procs, &canon);
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mtime = mtime_unix(path);
    let facts = ReservedDirFacts {
        path_exists,
        has_git_entry: path.join(".git").symlink_metadata().is_ok(),
        empty: std::fs::read_dir(path)
            .ok()
            .map(|mut entries| entries.next().is_none()),
        process_inside: live_cwds
            .iter()
            .any(|cwd| canonical(cwd).starts_with(&canon))
            || in_table == Some(true),
        processes_unverifiable: in_table.is_none(),
        age_secs: (mtime > 0 && now >= mtime).then(|| (now - mtime) as u64),
    };
    row(reserved_dir_verdict(&facts))
}

#[cfg(test)]
#[path = "repo_worktree_probe_tests.rs"]
mod tests;

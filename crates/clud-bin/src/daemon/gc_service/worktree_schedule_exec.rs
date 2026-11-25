//! Build-output pruning and recovery of verified remnants in the managed root.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crate::gc::keep_list;
use crate::gc::target_prune;
use crate::gc::worktree_root::SizeCheck;
use crate::gc::worktree_schedule::{self, Candidate};
use crate::gc::worktree_size_cache::{cache_path_for, now_unix, read_cache};

use super::repo_worktree::RepoWorktreeState;
use super::repo_worktree_probe::RepoWorktreeRow;
use super::repo_worktree_reclaim::{ProcessCwdSnapshot, ReclaimMode};
use super::{GcOp, GcRequestMsg, RegistryMsg};

const INSERT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Kept, reserved, reclaimable-elsewhere, or holding a live session/process.
fn in_use(
    r: &RepoWorktreeRow,
    keep: &keep_list::KeepList,
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
) -> bool {
    r.reservation
        || r.verdict.state == RepoWorktreeState::Reclaimable
        || keep.keeps(&r.path)
        || super::entry_path_contains_live_cwd_path(&r.path, live_cwds)
        || super::source_guard::source_in_use_with_processes(Path::new(&r.path), procs).is_some()
}

/// Build one candidate per large worktree. Anything the probe did not see as
/// a worktree, a reservation, a reclaimable row, a kept path or a path with a
/// live session or process inside is exempt.
fn candidates(
    wt_root: &Path,
    rows: &[RepoWorktreeRow],
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    now: i64,
) -> Vec<Candidate> {
    let keep = keep_list::load(&keep_list::keep_path_for(wt_root));
    let top = crate::gc::worktree_top::read_for(wt_root, now, 6 * 60 * 60);
    let mut out: Vec<Candidate> = top
        .into_iter()
        .map(|t| {
            let row = rows.iter().find(|r| r.path == t.path);
            let exempt = row.is_none_or(|r| in_use(r, &keep, live_cwds, procs));
            Candidate {
                path: t.path,
                bytes: t.bytes,
                idle_secs: row.map_or(0, |r| now.saturating_sub(r.mtime_unix)),
                exempt,
                dirty: None,
            }
        })
        .collect();
    // #2487: age retention looks at every worktree under the root, not only
    // the largest; size is unknown (0) for one outside the top list.
    for r in rows {
        let idle_secs = now.saturating_sub(r.mtime_unix);
        if idle_secs < worktree_schedule::MAX_AGE_SECS
            || !direct_child(Path::new(&r.path), wt_root)
            || out.iter().any(|c| c.path == r.path)
        {
            continue;
        }
        out.push(Candidate {
            path: r.path.clone(),
            bytes: 0,
            idle_secs,
            exempt: in_use(r, &keep, live_cwds, procs),
            dirty: None,
        });
    }
    out
}

/// Keep budget-driven build-output pruning. Whole checkouts go through
/// the repo-worktree janitor so all removals use the same safety checks.
pub(super) fn run(
    wt_root: Option<&Path>,
    rows: &[RepoWorktreeRow],
    live_cwds: &[PathBuf],
    procs: &ProcessCwdSnapshot,
    mode: ReclaimMode,
) {
    let Some(wt_root) = wt_root else { return };
    if cfg!(test) || !procs.available || mode != ReclaimMode::Delete {
        return;
    }
    let _ = worktree_schedule::save(wt_root, &worktree_schedule::Schedule::default());
    let now = now_unix();
    let cached = read_cache(&cache_path_for(wt_root));
    let over = matches!(cached.map(|c| c.check), Some(SizeCheck::Over(_)));
    let mut cands = candidates(wt_root, rows, live_cwds, procs, now);
    if over || cands.iter().any(worktree_schedule::aged) {
        let warn_bytes = cached.map_or(0, |c| c.warn_bytes);
        prune_build_output(wt_root, rows, live_cwds, &mut cands, over, warn_bytes, now);
    }
}

/// An unclaimed, nonempty directory can be moved only when the janitor's
/// receipt and the original commit independently verify what survived.
pub(super) fn recover_remnants(
    wt_root: Option<&Path>,
    rows: &mut [RepoWorktreeRow],
    live_cwds: &[PathBuf],
    mode: ReclaimMode,
    tx: &mpsc::Sender<RegistryMsg>,
) {
    let Some(root) = wt_root else { return };
    let keep = keep_list::load(&keep_list::keep_path_for(root));
    for row in rows {
        if !matches!(
            row.verdict.reason.as_str(),
            "not empty" | "unverifiable" | "dirty"
        ) || keep.keeps(&row.path)
        {
            continue;
        }
        recover_remnant(root, row, live_cwds, mode, tx);
    }
}

fn recover_remnant(
    root: &Path,
    row: &mut RepoWorktreeRow,
    live_cwds: &[PathBuf],
    mode: ReclaimMode,
    tx: &mpsc::Sender<RegistryMsg>,
) {
    let path = Path::new(&row.path);
    if !direct_child(path, root) {
        return;
    }
    let repo = match super::worktree_remnants::verify(root, path) {
        Ok(repo) => repo,
        Err(reason) => {
            if row.reservation || reason != super::worktree_remnants::UNKNOWN_PROVENANCE {
                row.verdict.reason = reason.to_owned();
            }
            return;
        }
    };
    let procs = super::repo_worktree_cli::live_process_cwds();
    let current_keep = keep_list::load(&keep_list::keep_path_for(root));
    if !procs.available
        || current_keep.keeps(&row.path)
        || super::entry_path_contains_live_cwd_path(&row.path, live_cwds)
        || super::source_guard::source_in_use_with_processes(path, &procs).is_some()
    {
        row.verdict.reason = "verified remnant still in use or pinned".to_owned();
        return;
    }
    row.verdict.reason = "verified partial worktree removal".to_owned();
    if mode != ReclaimMode::Delete {
        return;
    }
    if let Err(reason) = super::worktree_remnants::verify(root, path) {
        row.verdict.reason = reason.to_owned();
        return;
    }
    match crate::gc::delete_audit::record_required("gc.worktree-remnant", path, &row.verdict.reason)
    {
        Ok(()) if trash(path, &root.with_file_name("trash"), tx, now_unix()) => {
            super::worktree_remnants::forget(root, path);
            // A whole-repo prune could discard a different remnant's
            // registration and private refs.
            let reason =
                super::repo_worktree_reclaim_exec::forget_quarantined_registration(&repo, path);
            row.verdict.state = RepoWorktreeState::Dangling;
            row.verdict.reason = reason;
            row.reservation = true;
        }
        Ok(()) => row.verdict.reason = "remnant quarantine failed".to_owned(),
        Err(error) => row.verdict.reason = format!("audit unavailable: {error}"),
    }
}

fn direct_child(path: &Path, root: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| crate::path_norm::same_path(parent, root))
}

/// #2487: move stray top-level `*.log` files out of the worktrees root into
/// the capped `~/.clud/logs/worktrees`. Delete mode only.
pub(super) fn tidy_root_logs(wt_root: Option<&Path>, mode: ReclaimMode) {
    let Some(wt_root) = wt_root else { return };
    if cfg!(test) || mode != ReclaimMode::Delete {
        return;
    }
    let logs = crate::gc::worktree_root_logs::logs_dir_for(wt_root);
    let r = crate::gc::worktree_root_logs::tidy(wt_root, &logs, std::time::SystemTime::now());
    if r.moved > 0 || r.deleted > 0 {
        eprintln!(
            "[clud] gc: moved {} stray log(s) from {} to {}; expired {} ({} bytes)",
            r.moved,
            wt_root.display(),
            logs.display(),
            r.deleted,
            r.deleted_bytes
        );
    }
}

/// #2302: prune `target/` dirs of idle, unused worktrees before anything is
/// scheduled for trash (#2487: also those due by age while under budget). Returns whether the root is still over budget; any
/// failure (no soldr, prune error) leaves the previous verdict standing.
fn prune_build_output(
    wt_root: &Path,
    rows: &[RepoWorktreeRow],
    live_cwds: &[PathBuf],
    cands: &mut [Candidate],
    over: bool,
    warn_bytes: u64,
    now: i64,
) -> bool {
    let picked: Vec<usize> = (0..cands.len())
        .filter(|&i| target_prune::eligible(&cands[i]) && worktree_schedule::due(&cands[i], over))
        .collect();
    if picked.is_empty() {
        return true;
    }
    let mut run = match target_prune::soldr_runner() {
        Ok(r) => r,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("[clud] gc: build-output prune skipped: {e}");
            }
            return true;
        }
    };
    let mut state = target_prune::PruneState::load(wt_root);
    let mut total = 0u64;
    let mut pruned = 0usize;
    for i in picked {
        // Re-evaluated right before each prune with a fresh process table:
        // the snapshot behind `cands` is stale after earlier, slow prunes.
        let path = cands[i].path.clone();
        let unused = || {
            let keep = keep_list::load(&keep_list::keep_path_for(wt_root));
            let procs = super::repo_worktree_cli::live_process_cwds();
            procs.available
                && rows
                    .iter()
                    .find(|r| r.path == path)
                    .is_some_and(|r| !in_use(r, &keep, live_cwds, &procs))
        };
        let out = target_prune::prune_worktree(
            Path::new(&path),
            &mut run,
            &target_prune::real_size,
            &unused,
            &mut state,
            now,
        );
        if out.targets_pruned > 0 {
            eprintln!(
                "[clud] gc: pruned {} target dir(s) in {}, reclaimed {} bytes",
                out.targets_pruned, cands[i].path, out.reclaimed_bytes
            );
        }
        cands[i].bytes = cands[i].bytes.saturating_sub(out.reclaimed_bytes);
        total = total.saturating_add(out.reclaimed_bytes);
        pruned += out.targets_pruned;
    }
    state.save(wt_root);
    // A capped size scan can report 0 reclaimed for a real prune, so any
    // successful prune forces a re-measure.
    if total == 0 && pruned == 0 {
        return true;
    }
    // Re-measure and record: the size report and top list now show the
    // post-prune magnitudes, not the stale pre-prune ones.
    let _ = crate::gc::worktree_size_cache::refresh_cache(wt_root, warn_bytes, now);
    matches!(
        read_cache(&cache_path_for(wt_root)).map(|c| c.check),
        Some(SizeCheck::Over(_))
    )
}

fn trash(path: &Path, trash_root: &Path, tx: &mpsc::Sender<RegistryMsg>, now: i64) -> bool {
    let display = path.display();
    let record = match crate::trash::trash_one_path(path, trash_root, false) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[clud] gc: could not trash {display}: {e}");
            return false;
        }
    };
    eprintln!(
        "[clud] gc: moved verified worktree remnant {display} to {} (recoverable for 72 h)",
        record.quarantined_path.display()
    );
    let input = crate::trash::insert_input_for_record(&record, now);
    let op = GcOp::Insert {
        kind: input.kind,
        path: input.path,
        repo_root: input.repo_root,
        branch: input.branch,
        agent_id: input.agent_id,
        created_unix: Some(input.now_unix),
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if tx
        .send(RegistryMsg::Op(GcRequestMsg { op, reply_tx }))
        .is_err()
        || reply_rx.recv_timeout(INSERT_REPLY_TIMEOUT).is_err()
    {
        eprintln!(
            "[clud] gc: trash row for {} not registered; remove it by hand when no longer needed",
            record.quarantine_dir.display()
        );
    }
    true
}

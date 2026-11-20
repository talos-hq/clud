//! Retire landed branches (DD-230): switch the primary checkout off a landed
//! branch, fast-forward the default branch, delete the local branch at its
//! verified tip, and delete its `origin` twin under a lease.
//!
//! Like the worktree reclaimer (DD-123), nothing is deleted from a stale
//! verdict: each branch is re-probed immediately before it is touched, and
//! must still be `retire` at the same tip. Nothing is ever forced: `git
//! switch` refuses to overwrite local files, the fast-forward is
//! `--ff-only`, and each remote delete carries `--force-with-lease` on the
//! tip that was judged.

use std::path::Path;
use std::time::Duration;

use super::extern_repo::probe_cmd_streams;
use super::repo_branch::{BranchAction, BranchCheckout};
use super::repo_branch_probe::{
    probe_branches_only, remote_branch_decision, retirable, BranchPr, BranchReport, BranchRow,
    ProbeOptions,
};
use super::repo_worktree_reclaim::branch_delete_decision;

const GIT_QUERY_TIMEOUT: Duration = Duration::from_secs(20);
const GIT_PUSH_TIMEOUT: Duration = Duration::from_secs(120);
/// Remote deletes per `git push`, so a large sweep is a few round trips.
const PUSH_BATCH: usize = 50;

/// What retiring one branch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetireOutcome {
    pub(crate) branch: String,
    /// The local branch was deleted.
    pub(crate) retired: bool,
    pub(crate) notes: Vec<String>,
}

fn git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    match probe_cmd_streams("git", cwd, args, timeout) {
        Some((0, out, _)) => Ok(out),
        Some((code, _, err)) => Err(format!(
            "git {} exited {code}: {}",
            args.join(" "),
            err.trim()
        )),
        None => Err(format!(
            "git {} timed out or failed to start",
            args.join(" ")
        )),
    }
}

fn rev(cwd: &Path, refname: &str) -> Option<String> {
    git(
        cwd,
        &["rev-parse", "--verify", "--quiet", refname],
        GIT_QUERY_TIMEOUT,
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

/// Switch the primary checkout to the default branch and fast-forward it.
/// `Err` only when the switch itself failed; a refused fast-forward is a
/// note, because the checkout is already off the landed branch.
fn return_to_default(report: &BranchReport, notes: &mut Vec<String>) -> Result<(), String> {
    let root = &report.repo_root;
    let local_default = format!("refs/heads/{}", report.default_branch);
    if rev(root, &local_default).is_some() {
        git(root, &["switch", &report.default_branch], GIT_QUERY_TIMEOUT)?;
    } else {
        git(
            root,
            &[
                "switch",
                "-c",
                &report.default_branch,
                "--track",
                &report.default_ref,
            ],
            GIT_QUERY_TIMEOUT,
        )?;
    }
    notes.push(format!("switched to {}", report.default_branch));
    match git(
        root,
        &["merge", "--ff-only", "--quiet", &report.default_ref],
        GIT_QUERY_TIMEOUT,
    ) {
        Ok(_) => notes.push(format!("fast-forwarded to {}", report.default_ref)),
        Err(err) => notes.push(format!(
            "{} not fast-forwarded: {err}",
            report.default_branch
        )),
    }
    Ok(())
}

/// Re-probe one branch with the run's PR snapshot (PR state cannot go from
/// merged back to open; local state is what may have moved).
fn still_retirable(cwd: &Path, row: &BranchRow, prs: Option<&[BranchPr]>) -> Result<(), String> {
    let lookup = |_: &Path| prs.map(<[BranchPr]>::to_vec);
    let no_setting = |_: &Path| None;
    let opts = ProbeOptions {
        prs: &lookup,
        delete_on_merge: &no_setting,
        remote_only: false,
    };
    let fresh =
        probe_branches_only(cwd, Some(&[row.name.as_str()]), &opts).ok_or("re-probe failed")?;
    let Some(now) = fresh.rows.first() else {
        return Err("branch gone".to_string());
    };
    if now.tip != row.tip {
        return Err("branch moved".to_string());
    }
    if now.verdict.action != BranchAction::Retire {
        return Err(format!("now {}", now.verdict.reason));
    }
    Ok(())
}

/// Retire one local branch. Remote deletes are returned as leased refspecs
/// for one batched push.
fn retire_one(
    report: &BranchReport,
    row: &BranchRow,
    prs: Option<&[BranchPr]>,
    delete_remote: bool,
    remote_deletes: &mut Vec<(String, String)>,
) -> RetireOutcome {
    let root = &report.repo_root;
    let mut outcome = RetireOutcome {
        branch: row.name.clone(),
        retired: false,
        notes: Vec::new(),
    };
    if let Err(reason) = still_retirable(root, row, prs) {
        outcome.notes.push(format!("kept: {reason}"));
        return outcome;
    }
    if row.checkout == BranchCheckout::Current {
        if let Err(err) = return_to_default(report, &mut outcome.notes) {
            outcome.notes.push(format!("kept: {err}"));
            return outcome;
        }
    }
    let local_ref = format!("refs/heads/{}", row.name);
    match branch_delete_decision(rev(root, &local_ref).as_deref(), &row.tip) {
        Ok(()) => match git(root, &["branch", "-D", &row.name], GIT_QUERY_TIMEOUT) {
            Ok(_) => {
                outcome.retired = true;
                outcome.notes.push("deleted local branch".to_string());
            }
            Err(err) => outcome.notes.push(format!("kept local branch: {err}")),
        },
        Err(reason) => outcome.notes.push(format!("kept local branch: {reason}")),
    }
    if outcome.retired && delete_remote {
        if let Some(remote_tip) = &row.remote_tip {
            match remote_branch_decision(&row.name, remote_tip, Some(&row.tip), prs) {
                Ok(_) => remote_deletes.push((row.name.clone(), remote_tip.clone())),
                Err(reason) => outcome
                    .notes
                    .push(format!("kept origin/{}: {reason}", row.name)),
            }
        }
    }
    outcome
}

/// Delete `origin` branches, each leased on the tip that was judged, in
/// batches. Returns `(branch, Ok | Err(reason))` per branch.
pub(crate) fn delete_remote_branches(
    root: &Path,
    branches: &[(String, String)],
) -> Vec<(String, Result<(), String>)> {
    let mut results = Vec::new();
    for batch in branches.chunks(PUSH_BATCH) {
        let mut args = vec!["push".to_string(), "--porcelain".to_string()];
        for (name, tip) in batch {
            args.push(format!("--force-with-lease=refs/heads/{name}:{tip}"));
        }
        args.push("origin".to_string());
        for (name, _) in batch {
            args.push(format!(":refs/heads/{name}"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        // Without --atomic each ref succeeds or fails on its own; the
        // porcelain status lines say which.
        let output = match probe_cmd_streams("git", root, &argv, GIT_PUSH_TIMEOUT) {
            Some((_, out, err)) => format!("{out}\n{err}"),
            None => String::new(),
        };
        for (name, _) in batch {
            results.push((name.clone(), push_ref_status(&output, name)));
        }
    }
    results
}

/// Pure: read one ref's result out of `git push --porcelain` output.
/// A deleted ref's line is `-\t:refs/heads/<name>\t[deleted]`.
pub(crate) fn push_ref_status(output: &str, name: &str) -> Result<(), String> {
    let target = format!(":refs/heads/{name}\t");
    let line = output.lines().find(|line| line.contains(&target));
    match line {
        Some(line) if line.starts_with('-') => Ok(()),
        Some(line) => Err(line.rsplit('\t').next().unwrap_or(line).to_string()),
        None => Err("no push status".to_string()),
    }
}

/// Retire every `retire` row of `report`, the current branch first so the
/// checkout leaves it before anything else runs.
pub(crate) fn retire_branches(
    report: &BranchReport,
    prs: Option<&[BranchPr]>,
    delete_remote: bool,
) -> Vec<RetireOutcome> {
    let mut rows: Vec<&BranchRow> = retirable(report).collect();
    rows.sort_by_key(|row| row.checkout != BranchCheckout::Current);
    let mut remote_deletes = Vec::new();
    let mut outcomes: Vec<RetireOutcome> = rows
        .into_iter()
        .map(|row| retire_one(report, row, prs, delete_remote, &mut remote_deletes))
        .collect();
    for (name, result) in delete_remote_branches(&report.repo_root, &remote_deletes) {
        let note = match result {
            Ok(()) => format!("deleted origin/{name}"),
            Err(reason) => format!("kept origin/{name}: {reason}"),
        };
        if let Some(outcome) = outcomes.iter_mut().find(|o| o.branch == name) {
            outcome.notes.push(note);
        }
    }
    outcomes
}

#[cfg(test)]
#[path = "repo_branch_exec_tests.rs"]
mod tests;

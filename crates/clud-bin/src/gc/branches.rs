//! `clud gc branches` and the session-start branch nudge (DD-230).
//!
//! The verdict, probe and executor live with the worktree reclaimer in
//! `daemon::gc_service` (`repo_branch*`), so the CLI, the nudge and the
//! daemon all judge "landed" by the same DD-122 evidence. This module only
//! renders reports and wires the network lookups.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::args::GcBranchesArgs;
use crate::daemon::repo_branch::{BranchAction, BranchCheckout};
use crate::daemon::repo_branch_exec::{delete_remote_branches, retire_branches, RetireOutcome};
use crate::daemon::repo_branch_probe::{
    current_branch, lookup_branch_prs, lookup_branch_prs_for_head, lookup_delete_on_merge,
    probe_branches, probe_branches_only, BranchPr, BranchReport, ProbeOptions,
};

/// PRs listed for a whole-repo report; older branches beyond this read as
/// "no PR" and are kept unless a patch match proves them.
const PR_LIMIT: u32 = 3000;
const PR_TIMEOUT: Duration = Duration::from_secs(60);
/// The session nudge runs before the first prompt; keep it short.
const NUDGE_GH_TIMEOUT: Duration = Duration::from_secs(4);

/// The exact command a skill or the nudge tells an agent to run.
pub const RETIRE_COMMAND: &str = "\"$CLUD_EXE\" gc branches --retire";

/// How to turn on GitHub's own head-branch deletion.
pub const ENABLE_SETTING_COMMAND: &str =
    "gh api -X PATCH repos/{owner}/{repo} -F delete_branch_on_merge=true";

/// Entry point for `clud gc branches`.
pub fn run_cli(args: &GcBranchesArgs) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            eprintln!("error: cannot read the current directory: {err}");
            return 1;
        }
    };
    if !args.no_fetch {
        if let Err(err) =
            crate::worktrees::run_git(&cwd, &["fetch", "--prune", "--quiet", "origin"])
        {
            eprintln!("warning: git fetch --prune origin failed; judging the last fetch: {err}");
        }
    }
    let prs = lookup_branch_prs(&cwd, PR_LIMIT, PR_TIMEOUT);
    let lookup = |_: &Path| prs.clone();
    let setting = |root: &Path| lookup_delete_on_merge(root);
    let opts = ProbeOptions {
        prs: &lookup,
        delete_on_merge: &setting,
        remote_only: args.sweep_remote,
    };
    let Some(report) = probe_branches(&cwd, &opts) else {
        eprintln!("error: not a git checkout with an origin default branch");
        return 1;
    };
    let outcomes = if args.retire {
        retire(&report, prs.as_deref(), args)
    } else {
        Vec::new()
    };
    if args.json {
        println!("{}", report_json(&report, &outcomes));
    } else {
        print!("{}", render(&report, &outcomes, args.retire));
    }
    0
}

/// Retire local rows, then (with `--sweep-remote`) the remote-only ones.
fn retire(
    report: &BranchReport,
    prs: Option<&[BranchPr]>,
    args: &GcBranchesArgs,
) -> Vec<RetireOutcome> {
    let mut outcomes = retire_branches(report, prs, !args.keep_remote);
    if args.sweep_remote {
        let doomed: Vec<(String, String)> = report
            .remote_rows
            .iter()
            .filter(|row| row.decision.is_ok())
            .map(|row| (row.name.clone(), row.tip.clone()))
            .collect();
        for (name, result) in delete_remote_branches(&report.repo_root, &doomed) {
            outcomes.push(RetireOutcome {
                branch: format!("origin/{name}"),
                retired: result.is_ok(),
                notes: vec![match result {
                    Ok(()) => "deleted".to_string(),
                    Err(reason) => format!("kept: {reason}"),
                }],
            });
        }
    }
    outcomes
}

fn checkout_label(checkout: BranchCheckout) -> &'static str {
    match checkout {
        BranchCheckout::Current => " [current]",
        BranchCheckout::Worktree => " [worktree]",
        BranchCheckout::None => "",
    }
}

/// Pure: the human-readable report.
pub(crate) fn render(report: &BranchReport, outcomes: &[RetireOutcome], retired: bool) -> String {
    let mut out = format!(
        "branches of {} (default {}{})\n",
        report.repo_root.display(),
        report.default_branch,
        if report.prs_available {
            ""
        } else {
            "; PR lookup unavailable, patch evidence only"
        }
    );
    let mut rows: Vec<_> = report.rows.iter().collect();
    rows.sort_by_key(|row| (action_rank(row.verdict.action), row.name.clone()));
    let width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);
    for row in &rows {
        out.push_str(&format!(
            "  {:<9}  {:<width$}  {}{}\n",
            row.verdict.action.as_str(),
            row.name,
            row.verdict.reason,
            checkout_label(row.checkout),
        ));
    }
    let remote_ok = report
        .remote_rows
        .iter()
        .filter(|r| r.decision.is_ok())
        .count();
    if !report.remote_rows.is_empty() {
        out.push_str(&format!(
            "  origin-only branches: {} judged, {remote_ok} proven merged\n",
            report.remote_rows.len()
        ));
    }
    for outcome in outcomes {
        out.push_str(&format!(
            "  {}: {}\n",
            outcome.branch,
            outcome.notes.join("; ")
        ));
    }
    out.push_str(&summary(report, retired, remote_ok));
    out
}

fn action_rank(action: BranchAction) -> u8 {
    match action {
        BranchAction::Retire => 0,
        BranchAction::FollowUp => 1,
        BranchAction::Keep => 2,
    }
}

fn summary(report: &BranchReport, retired: bool, remote_ok: usize) -> String {
    let count = |action| {
        report
            .rows
            .iter()
            .filter(|r| r.verdict.action == action)
            .count()
    };
    let retire = count(BranchAction::Retire);
    let follow_up = count(BranchAction::FollowUp);
    let mut out = String::new();
    if !retired && (retire > 0 || remote_ok > 0) {
        out.push_str(&format!(
            "{retire} landed local branch(es){}. Run `{RETIRE_COMMAND}{}` to switch back to {} \
             and delete them. It re-verifies each branch at its tip before deleting it and leases \
             every origin delete; a bare `git switch` leaves them behind.\n",
            if remote_ok > 0 {
                format!(" and {remote_ok} merged origin-only branch(es)")
            } else {
                String::new()
            },
            if remote_ok > 0 { " --sweep-remote" } else { "" },
            report.default_branch
        ));
    }
    if follow_up > 0 {
        out.push_str(&format!(
            "{follow_up} branch(es) hold commits after their merged PR: open a new PR for that \
             work, never delete it.\n"
        ));
    }
    if report.delete_branch_on_merge == Some(false) {
        out.push_str(&format!(
            "warning: this repository has GitHub's \"Automatically delete head branches\" \
             (delete_branch_on_merge) off, so every merged PR leaves its branch on origin. \
             Ask the repository admin to enable it: `{ENABLE_SETTING_COMMAND}`\n"
        ));
    }
    out
}

/// Pure: the `--json` document.
pub(crate) fn report_json(report: &BranchReport, outcomes: &[RetireOutcome]) -> Value {
    json!({
        "repo_root": report.repo_root.display().to_string(),
        "default_branch": report.default_branch,
        "prs_available": report.prs_available,
        "delete_branch_on_merge": report.delete_branch_on_merge,
        "branches": report.rows.iter().map(|row| json!({
            "name": row.name,
            "tip": row.tip,
            "current": row.checkout == BranchCheckout::Current,
            "worktree": row.checkout == BranchCheckout::Worktree,
            "remote_tip": row.remote_tip,
            "action": row.verdict.action.as_str(),
            "reason": row.verdict.reason,
        })).collect::<Vec<_>>(),
        "origin_only": report.remote_rows.iter().map(|row| json!({
            "name": row.name,
            "tip": row.tip,
            "deletable": row.decision.is_ok(),
            "reason": match &row.decision { Ok(r) | Err(r) => r },
        })).collect::<Vec<_>>(),
        "outcomes": outcomes.iter().map(|o| json!({
            "branch": o.branch,
            "retired": o.retired,
            "notes": o.notes,
        })).collect::<Vec<_>>(),
    })
}

/// The session-start nudge for the checkout at `cwd`: one paragraph when
/// its current branch has landed (or carries commits after its merged PR),
/// otherwise `None`. Never fetches; one narrow `gh pr list --head` call,
/// and only when the checkout is on a non-default branch.
pub fn session_nudge(cwd: &Path) -> Option<String> {
    let branch = current_branch(cwd)?;
    let lookup = |root: &Path| lookup_branch_prs_for_head(root, &branch, NUDGE_GH_TIMEOUT);
    let no_setting = |_: &Path| None;
    let opts = ProbeOptions {
        prs: &lookup,
        delete_on_merge: &no_setting,
        remote_only: false,
    };
    let report = probe_branches_only(cwd, Some(&[branch.as_str()]), &opts)?;
    nudge_text(&report)
}

/// Pure: the nudge for a report probed for the current branch only.
pub(crate) fn nudge_text(report: &BranchReport) -> Option<String> {
    let row = report
        .rows
        .iter()
        .find(|row| row.checkout == BranchCheckout::Current)?;
    match row.verdict.action {
        BranchAction::Retire => Some(format!(
            "clud branch hygiene: this checkout is still on `{}`, which has already landed \
             ({}). Before starting new work, run `{RETIRE_COMMAND}` to switch back to `{}`, \
             fast-forward it, and delete the landed branch and its origin copy. If you keep \
             the branch on purpose, tell the user why.",
            row.name, row.verdict.reason, report.default_branch
        )),
        BranchAction::FollowUp => Some(format!(
            "clud branch hygiene: this checkout is on `{}`, which has {}: that work is not on \
             `{}`. Open a new PR for it (or tell the user why not) before the branch is \
             forgotten.",
            row.name, row.verdict.reason, report.default_branch
        )),
        BranchAction::Keep => None,
    }
}

#[cfg(test)]
#[path = "branches_tests.rs"]
mod tests;

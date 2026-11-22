//! Gather [`RepoBranchFacts`] for every local branch of one checkout and
//! judge them (DD-230). Read-only: `repo_branch_exec` re-probes one branch
//! from scratch immediately before it changes anything.
//!
//! Cost per run: one `git for-each-ref`, one `git worktree list`, at most one
//! `gh pr list` and one `gh api` call, and the DD-122 patch scan, whose
//! per-commit diffs are cached across branches because every branch is
//! compared with the same default branch.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::worktrees::parse_worktree_porcelain;

use super::extern_repo::probe_cmd;
use super::repo_branch::{
    repo_branch_verdict, BranchAction, BranchCheckout, RepoBranchFacts, RepoBranchVerdict,
};
use super::repo_worktree::PrFact;
use super::repo_worktree_probe::{
    ahead_of_default, commit_diff, default_ref, git, patch_landed_with, pr_fact_for_branch,
    tip_covered_by, work_tree_status, PrRecord,
};

/// One row of `gh pr list --json number,state,headRefName,headRefOid,baseRefName`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct BranchPr {
    pub(crate) number: u64,
    pub(crate) state: String,
    #[serde(rename = "headRefName")]
    pub(crate) head_ref_name: String,
    #[serde(rename = "headRefOid")]
    pub(crate) head_ref_oid: String,
    #[serde(rename = "baseRefName", default)]
    pub(crate) base_ref_name: String,
}

impl BranchPr {
    fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("OPEN")
    }

    fn is_merged(&self) -> bool {
        self.state.eq_ignore_ascii_case("MERGED")
    }

    fn record(&self) -> PrRecord {
        PrRecord {
            number: self.number,
            state: self.state.clone(),
            head_ref_name: self.head_ref_name.clone(),
            head_ref_oid: self.head_ref_oid.clone(),
        }
    }
}

/// One judged local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchRow {
    pub(crate) name: String,
    pub(crate) tip: String,
    pub(crate) checkout: BranchCheckout,
    /// `refs/remotes/origin/<name>` as of the last fetch.
    pub(crate) remote_tip: Option<String>,
    pub(crate) verdict: RepoBranchVerdict,
}

/// One `origin` branch with no local branch of the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteRow {
    pub(crate) name: String,
    pub(crate) tip: String,
    /// `Ok(reason)` when it may be deleted, `Err(reason)` when kept.
    pub(crate) decision: Result<String, String>,
}

/// Everything `clud gc branches` reports for one checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchReport {
    pub(crate) repo_root: PathBuf,
    /// Local name of the default branch (`main`, `master`, ...).
    pub(crate) default_branch: String,
    /// The remote-tracking default (`origin/main`).
    pub(crate) default_ref: String,
    pub(crate) rows: Vec<BranchRow>,
    pub(crate) remote_rows: Vec<RemoteRow>,
    /// The repository's `delete_branch_on_merge` setting; `None` when `gh`
    /// could not say (not GitHub, offline, unauthenticated).
    pub(crate) delete_branch_on_merge: Option<bool>,
    /// Whether the PR lookup answered; without it only patch evidence counts.
    pub(crate) prs_available: bool,
}

/// How the probe reaches the network. Injected so tests run offline.
pub(crate) struct ProbeOptions<'a> {
    pub(crate) prs: &'a dyn Fn(&Path) -> Option<Vec<BranchPr>>,
    pub(crate) delete_on_merge: &'a dyn Fn(&Path) -> Option<bool>,
    /// Also judge `origin` branches that have no local branch.
    pub(crate) remote_only: bool,
}

const GH_SETTING_TIMEOUT: Duration = Duration::from_secs(10);

/// Every PR of the checkout's GitHub repo, up to `limit`. `None` when the
/// origin is not GitHub or `gh` does not answer with JSON in time.
pub(crate) fn lookup_branch_prs(
    repo_root: &Path,
    limit: u32,
    timeout: Duration,
) -> Option<Vec<BranchPr>> {
    let url = git(repo_root, &["remote", "get-url", "origin"])?;
    if !url.contains("github.com") {
        return None;
    }
    let limit = limit.to_string();
    let (code, out) = probe_cmd(
        "gh",
        repo_root,
        &[
            "pr",
            "list",
            "--state",
            "all",
            "--limit",
            &limit,
            "--json",
            "number,state,headRefName,headRefOid,baseRefName",
        ],
        timeout,
    )?;
    (code == 0).then(|| serde_json::from_str(&out).ok())?
}

/// PRs whose head is `branch`, for the one-branch session nudge: far
/// cheaper than listing every PR of a large repository.
pub(crate) fn lookup_branch_prs_for_head(
    repo_root: &Path,
    branch: &str,
    timeout: Duration,
) -> Option<Vec<BranchPr>> {
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
            "--head",
            branch,
            "--json",
            "number,state,headRefName,headRefOid,baseRefName",
        ],
        timeout,
    )?;
    (code == 0).then(|| serde_json::from_str(&out).ok())?
}

/// The repository's `delete_branch_on_merge`, through `gh api`.
pub(crate) fn lookup_delete_on_merge(repo_root: &Path) -> Option<bool> {
    let (code, out) = probe_cmd(
        "gh",
        repo_root,
        &[
            "api",
            "repos/{owner}/{repo}",
            "--jq",
            ".delete_branch_on_merge",
        ],
        GH_SETTING_TIMEOUT,
    )?;
    if code != 0 {
        return None;
    }
    match out.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The checkout's top level and whether it is the repository's primary
/// checkout (not a linked worktree).
fn checkout_root(cwd: &Path) -> Option<(PathBuf, bool)> {
    let top = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    let git_dir = git(cwd, &["rev-parse", "--absolute-git-dir"])?;
    let common = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Some((top, git_dir.trim() == common.trim()))
}

/// `owner/repo` of `cwd`'s `origin` when it is a GitHub remote.
pub(crate) fn origin_github_slug(cwd: &Path) -> Option<String> {
    github_slug_from_url(git(cwd, &["remote", "get-url", "origin"])?.trim())
}

/// `owner/repo` from an https or ssh GitHub remote URL.
pub(crate) fn github_slug_from_url(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("github.com")?;
    let rest = rest.trim_start_matches([':', '/']).trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !repo.is_empty())
        .then(|| format!("{owner}/{repo}"))
}

/// The branch checked out at `cwd`, if any.
pub(crate) fn current_branch(cwd: &Path) -> Option<String> {
    let name = git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Branch name → worktree path, for every branch checked out anywhere.
fn checked_out_branches(repo_root: &Path) -> HashMap<String, PathBuf> {
    let raw = git(repo_root, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    parse_worktree_porcelain(&raw)
        .into_iter()
        .filter_map(|entry| {
            let branch = entry.branch?;
            let name = branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&branch)
                .to_string();
            Some((name, entry.path))
        })
        .collect()
}

/// `(name, tip)` for every ref under `prefix` (`refs/heads/`, `refs/remotes/origin/`).
fn refs_under(repo_root: &Path, prefix: &str) -> Vec<(String, String)> {
    let out = git(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            prefix,
        ],
    )
    .unwrap_or_default();
    out.lines()
        .filter_map(|line| {
            let (name, tip) = line.split_once('\0')?;
            Some((name.strip_prefix(prefix)?.to_string(), tip.to_string()))
        })
        .collect()
}

/// Shared, per-run state for judging many branches of one checkout.
struct Judge<'a> {
    root: &'a Path,
    prs: Option<Vec<PrRecord>>,
    diffs: HashMap<String, Option<String>>,
}

impl Judge<'_> {
    fn facts(&mut self, name: &str, tip: &str, checkout: BranchCheckout) -> RepoBranchFacts {
        let root = self.root;
        let mut facts = RepoBranchFacts {
            checkout,
            dirty: None,
            pr: PrFact::Unavailable,
            patch_landed: None,
            commits_ahead: None,
        };
        if checkout == BranchCheckout::Worktree {
            return facts;
        }
        if checkout == BranchCheckout::Current {
            facts.dirty = work_tree_status(root).0;
            if facts.dirty != Some(false) {
                return facts;
            }
        }
        facts.pr = pr_fact_for_branch(name, self.prs.as_deref(), &mut |head| {
            tip_covered_by(root, tip, head)
        });
        if !matches!(
            facts.pr,
            PrFact::Open { .. }
                | PrFact::Merged {
                    tip_covered: Some(true),
                    ..
                }
        ) {
            let diffs = &mut self.diffs;
            facts.patch_landed = patch_landed_with(root, tip, &mut |commit| {
                diffs
                    .entry(commit.to_string())
                    .or_insert_with(|| commit_diff(root, commit))
                    .clone()
            });
        }
        if matches!(
            facts.pr,
            PrFact::NoPr | PrFact::Unavailable | PrFact::Closed { .. }
        ) {
            facts.commits_ahead = ahead_of_default(root, tip).map(|(_, _, n)| n);
        }
        facts
    }
}

/// Judge every local branch of the checkout at `cwd` (and, with
/// `remote_only`, every `origin` branch without a local twin). `None` when
/// `cwd` is not a git checkout or has no default branch on `origin`.
pub(crate) fn probe_branches(cwd: &Path, opts: &ProbeOptions<'_>) -> Option<BranchReport> {
    probe_branches_only(cwd, None, opts)
}

/// [`probe_branches`] restricted to the named local branches (the
/// executor's pre-delete re-check and the session nudge).
pub(crate) fn probe_branches_only(
    cwd: &Path,
    only: Option<&[&str]>,
    opts: &ProbeOptions<'_>,
) -> Option<BranchReport> {
    let (root, primary) = checkout_root(cwd)?;
    let default_ref = default_ref(&root)?;
    let default_branch = default_ref
        .strip_prefix("origin/")
        .unwrap_or(&default_ref)
        .to_string();
    let current = if primary { current_branch(&root) } else { None };
    let checked_out = checked_out_branches(&root);
    let locals: Vec<(String, String)> = refs_under(&root, "refs/heads/")
        .into_iter()
        .filter(|(name, _)| *name != default_branch)
        .filter(|(name, _)| only.is_none_or(|only| only.contains(&name.as_str())))
        .collect();
    let remotes: HashMap<String, String> = refs_under(&root, "refs/remotes/origin/")
        .into_iter()
        .collect();
    let branch_prs = if locals.is_empty() && !opts.remote_only {
        None
    } else {
        (opts.prs)(&root)
    };
    let mut judge = Judge {
        root: &root,
        prs: branch_prs
            .as_ref()
            .map(|prs| prs.iter().map(BranchPr::record).collect()),
        diffs: HashMap::new(),
    };
    let mut rows = Vec::new();
    for (name, tip) in &locals {
        let checkout = match (
            current.as_deref() == Some(name),
            checked_out.contains_key(name),
        ) {
            (true, _) => BranchCheckout::Current,
            (false, true) => BranchCheckout::Worktree,
            (false, false) => BranchCheckout::None,
        };
        let facts = judge.facts(name, tip, checkout);
        rows.push(BranchRow {
            name: name.clone(),
            tip: tip.clone(),
            checkout,
            remote_tip: remotes.get(name).cloned(),
            verdict: repo_branch_verdict(&facts),
        });
    }
    let remote_rows = if opts.remote_only {
        let local_names: HashSet<String> = refs_under(&root, "refs/heads/")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        remote_only_rows(
            &remotes,
            &local_names,
            &default_branch,
            branch_prs.as_deref(),
        )
    } else {
        Vec::new()
    };
    Some(BranchReport {
        delete_branch_on_merge: (opts.delete_on_merge)(&root),
        prs_available: branch_prs.is_some(),
        repo_root: root,
        default_branch,
        default_ref,
        rows,
        remote_rows,
    })
}

fn remote_only_rows(
    remotes: &HashMap<String, String>,
    local_names: &HashSet<String>,
    default_branch: &str,
    prs: Option<&[BranchPr]>,
) -> Vec<RemoteRow> {
    let mut rows: Vec<RemoteRow> = remotes
        .iter()
        .filter(|(name, _)| name.as_str() != "HEAD" && name.as_str() != default_branch)
        .filter(|(name, _)| !local_names.contains(name.as_str()))
        .map(|(name, tip)| RemoteRow {
            name: name.clone(),
            tip: tip.clone(),
            decision: remote_branch_decision(name, tip, None, prs),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Pure: may `origin/<name>` (at `remote_tip`) be deleted?
///
/// Only with positive PR evidence: a merged PR whose head is exactly the
/// remote tip, or (for a local branch already judged landed) the remote tip
/// equal to that verified local tip. Never while any open PR has the branch
/// as its head or as its base — deleting a base branch closes the PRs
/// stacked on it, where GitHub's own `delete_branch_on_merge` retargets
/// them.
pub(crate) fn remote_branch_decision(
    name: &str,
    remote_tip: &str,
    verified_local_tip: Option<&str>,
    prs: Option<&[BranchPr]>,
) -> Result<String, String> {
    let Some(prs) = prs else {
        return Err("PR lookup unavailable".to_string());
    };
    if let Some(open) = prs.iter().find(|p| p.is_open() && p.head_ref_name == name) {
        return Err(format!("open PR #{}", open.number));
    }
    if let Some(stacked) = prs.iter().find(|p| p.is_open() && p.base_ref_name == name) {
        return Err(format!("base of open PR #{}", stacked.number));
    }
    if let Some(merged) = prs
        .iter()
        .filter(|p| p.is_merged() && p.head_ref_name == name)
        .find(|p| p.head_ref_oid == remote_tip)
    {
        return Ok(format!("merged via PR #{}", merged.number));
    }
    if verified_local_tip == Some(remote_tip) {
        return Ok("same tip as the retired local branch".to_string());
    }
    Err("no merged PR with this exact head".to_string())
}

/// The branches a report would retire.
pub(crate) fn retirable(report: &BranchReport) -> impl Iterator<Item = &BranchRow> {
    report
        .rows
        .iter()
        .filter(|row| row.verdict.action == BranchAction::Retire)
}

#[cfg(test)]
#[path = "repo_branch_probe_tests.rs"]
mod tests;

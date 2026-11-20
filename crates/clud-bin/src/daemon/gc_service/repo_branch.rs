//! Branch hygiene: the DD-122 "landed" verdict applied to local branches,
//! including the branch the primary checkout is parked on (DD-230).
//!
//! The worktree verdict (`repo_worktree`) never touches the main checkout
//! and never looks at a branch that has no worktree, so a squash-merged
//! branch left checked out in the primary clone, or left behind after
//! `git switch`, piles up forever: `git branch --merged` cannot see a squash
//! merge. This verdict reuses the same positive evidence — a merged PR whose
//! head covers the tip, or a patch-equivalent default-branch commit — and
//! adds the one action the worktree path cannot take: switch the primary
//! checkout back to the default branch before deleting the branch.
//!
//! Pure over [`RepoBranchFacts`]; `repo_branch_probe` gathers the facts and
//! `repo_branch_exec` re-verifies before it changes anything.

use super::repo_worktree::PrFact;

/// Where the branch is checked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchCheckout {
    /// Not checked out anywhere.
    None,
    /// Checked out in the primary checkout the command runs in. Retiring it
    /// switches that checkout back to the default branch first.
    Current,
    /// Checked out in a linked worktree; the worktree reclaimer owns it.
    Worktree,
}

/// Every input the verdict needs. `Option` fields are `None` when the query
/// failed or was skipped; a failed query is never read as the safe answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoBranchFacts {
    pub(crate) checkout: BranchCheckout,
    /// Uncommitted changes to tracked files; only read for `Current`.
    pub(crate) dirty: Option<bool>,
    pub(crate) pr: PrFact,
    /// DD-122 fallback: the branch's cumulative diff is one default-branch
    /// commit.
    pub(crate) patch_landed: Option<bool>,
    /// Commits past the merge-base with the default branch.
    pub(crate) commits_ahead: Option<u64>,
}

/// What to do with the branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchAction {
    /// The work landed (or the branch holds none); safe to delete.
    Retire,
    /// Leave it alone.
    Keep,
    /// A merged PR covers only part of the branch: the commits after its
    /// head are unmerged work that needs a new PR.
    FollowUp,
}

impl BranchAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Retire => "retire",
            Self::Keep => "keep",
            Self::FollowUp => "follow-up",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoBranchVerdict {
    pub(crate) action: BranchAction,
    pub(crate) reason: String,
}

impl RepoBranchVerdict {
    fn new(action: BranchAction, reason: impl Into<String>) -> Self {
        Self {
            action,
            reason: reason.into(),
        }
    }
}

/// Decide one local branch. Precedence, first match wins:
///
/// | condition                                   | action    | reason                           |
/// |---------------------------------------------|-----------|----------------------------------|
/// | checked out in a linked worktree            | keep      | `in a worktree`                  |
/// | current, dirty unknown                      | keep      | `unverifiable`                   |
/// | current, uncommitted changes                | keep      | `uncommitted changes`            |
/// | open PR #N                                  | keep      | `open PR #N`                     |
/// | merged PR #N, tip covered                   | retire    | `merged via PR #N`               |
/// | merged PR #N, patch landed                  | retire    | `merged via PR #N (patch match)` |
/// | merged PR #N, commits after its head        | follow-up | `commits after merged PR #N`     |
/// | merged PR #N, coverage unknown              | keep      | `unverifiable`                   |
/// | no/closed PR or no `gh`, patch landed       | retire    | `landed by patch match`          |
/// | no/closed PR or no `gh`, 0 ahead, current   | keep      | `no commits yet`                 |
/// | no/closed PR or no `gh`, 0 ahead, elsewhere | retire    | `contained in default branch`    |
/// | no PR                                       | keep      | `no PR`                          |
/// | closed PR #N                                | keep      | `closed PR #N`                   |
/// | lookup unavailable                          | keep      | `unverifiable`                   |
///
/// Untracked files never block: `git switch` carries them across and
/// refuses on its own if one would be overwritten.
pub(crate) fn repo_branch_verdict(facts: &RepoBranchFacts) -> RepoBranchVerdict {
    use BranchAction::{FollowUp, Keep, Retire};
    match facts.checkout {
        BranchCheckout::Worktree => return RepoBranchVerdict::new(Keep, "in a worktree"),
        BranchCheckout::Current => match facts.dirty {
            None => return RepoBranchVerdict::new(Keep, "unverifiable"),
            Some(true) => return RepoBranchVerdict::new(Keep, "uncommitted changes"),
            Some(false) => {}
        },
        BranchCheckout::None => {}
    }
    let patch_landed = facts.patch_landed == Some(true);
    match facts.pr {
        PrFact::Open { number } => RepoBranchVerdict::new(Keep, format!("open PR #{number}")),
        PrFact::Merged {
            number,
            tip_covered: Some(true),
        } => RepoBranchVerdict::new(Retire, format!("merged via PR #{number}")),
        PrFact::Merged { number, .. } if patch_landed => {
            RepoBranchVerdict::new(Retire, format!("merged via PR #{number} (patch match)"))
        }
        PrFact::Merged {
            number,
            tip_covered: Some(false),
        } => RepoBranchVerdict::new(FollowUp, format!("commits after merged PR #{number}")),
        PrFact::Merged {
            tip_covered: None, ..
        } => RepoBranchVerdict::new(Keep, "unverifiable"),
        PrFact::NoPr | PrFact::Unavailable | PrFact::Closed { .. } => unlanded(facts, patch_landed),
    }
}

/// The rows for a branch no open or merged PR speaks for.
fn unlanded(facts: &RepoBranchFacts, patch_landed: bool) -> RepoBranchVerdict {
    use BranchAction::{Keep, Retire};
    if patch_landed {
        return RepoBranchVerdict::new(Retire, "landed by patch match");
    }
    if facts.commits_ahead == Some(0) {
        return if facts.checkout == BranchCheckout::Current {
            RepoBranchVerdict::new(Keep, "no commits yet")
        } else {
            RepoBranchVerdict::new(Retire, "contained in default branch")
        };
    }
    match facts.pr {
        PrFact::Closed { number } => RepoBranchVerdict::new(Keep, format!("closed PR #{number}")),
        PrFact::NoPr => RepoBranchVerdict::new(Keep, "no PR"),
        _ => RepoBranchVerdict::new(Keep, "unverifiable"),
    }
}

#[cfg(test)]
#[path = "repo_branch_tests.rs"]
mod tests;

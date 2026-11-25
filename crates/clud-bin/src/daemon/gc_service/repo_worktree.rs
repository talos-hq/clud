//! Issue #1591: a squash-aware verdict for the worktrees of repos the GC
//! registry already knows about (`record_repo_visit`).
//!
//! This repo squash-merges, so a merged branch's tip is never an ancestor of
//! the default branch: `git branch --merged` and `merge-base --is-ancestor`
//! both answer "not merged" forever (DD-122). The verdict below instead
//! accepts only *positive* evidence that the work landed — a merged PR whose
//! head covers the local tip, or a patch-equivalent commit on the default
//! branch — and spares on every doubt.
//!
//! The function is pure over [`RepoWorktreeFacts`]: gathering the facts
//! (git, `gh`, lock liveness, session cwds) lives in `repo_worktree_probe`,
//! so the precedence here is unit-testable with no repo, no process table and
//! no network, per the reap/spare rule in `CLAUDE.md`.
//!
//! `clud gc list` surfaces the verdict; since #1603 the daemon also acts on
//! `reclaimable` rows, after re-verifying on the purge pool
//! (`repo_worktree_reclaim`).

use crate::gc::worktree_root::ABANDONED_EMPTY_GRACE_SECS;

/// What the merged-PR lookup said about this worktree's branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrFact {
    /// `gh` missing, unauthenticated, offline, timed out, or not a GitHub
    /// remote. Only the patch-equivalence fallback can prove anything.
    Unavailable,
    /// The lookup ran and no PR has this branch as its head.
    NoPr,
    /// #1961: only closed, unmerged PRs had this head. Nothing landed
    /// through them, so it reads like `NoPr` but names the PR.
    Closed { number: u64 },
    /// At least one open PR has this branch as its head. Wins over any
    /// merged PR with the same head name (branch names get reused).
    Open { number: u64 },
    /// A merged PR had this head. `tip_covered` is whether the local tip is
    /// that PR's `headRefOid` or an ancestor of it; `None` when that could
    /// not be determined (for example the head object is not present
    /// locally).
    Merged {
        number: u64,
        tip_covered: Option<bool>,
    },
}

/// Every input the verdict needs, gathered by the probe. `Option<bool>`
/// fields are `None` when the underlying query failed; a failed query is
/// never read as the safe answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoWorktreeFacts {
    /// `false` until the probe has inspected this worktree at all.
    pub(crate) evaluated: bool,
    pub(crate) path_exists: bool,
    /// The repository's primary checkout (first `git worktree list` entry).
    pub(crate) is_main_checkout: bool,
    /// `locked` with a pid that is still alive.
    pub(crate) locked_live_pid: bool,
    /// A live session's cwd (or a process cwd) sits inside the worktree —
    /// this also covers "the currently checked-out one in any session".
    pub(crate) process_inside: bool,
    /// Issue #1603: the full process table could not be read (see
    /// `ProcessCwdSnapshot`), so "nobody is inside" is unproven.
    pub(crate) processes_unverifiable: bool,
    pub(crate) detached: bool,
    /// Uncommitted changes to tracked files.
    pub(crate) dirty: Option<bool>,
    /// Untracked, non-ignored files.
    pub(crate) untracked: Option<bool>,
    pub(crate) pr: PrFact,
    /// Fallback: the branch has at least one commit past its merge-base with
    /// the default branch, and that cumulative change appears verbatim as a
    /// commit on the default branch. Never true for a branch with no
    /// commits of its own.
    pub(crate) patch_landed: Option<bool>,
    /// #1485: the worktree sits under the clud-owned root `~/.clud/worktrees`.
    /// Only there does the abandoned-empty rule apply; a hand-made sibling
    /// worktree elsewhere is never reclaimed for being empty.
    pub(crate) under_worktree_root: bool,
    /// Commits on the branch past its merge-base with the default branch;
    /// `None` when not computed or the query failed.
    pub(crate) commits_ahead: Option<u64>,
    /// A remote-tracking ref contains HEAD.
    pub(crate) published: bool,
    /// Seconds since the latest checkout activity, or `None` when unknown.
    pub(crate) age_secs: Option<u64>,
}

/// Displayed class, matching `clud gc list`'s `state` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoWorktreeState {
    Reclaimable,
    Pinned,
    Dangling,
}

impl RepoWorktreeState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reclaimable => "reclaimable",
            Self::Pinned => "pinned",
            Self::Dangling => "dangling",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoWorktreeVerdict {
    pub(crate) state: RepoWorktreeState,
    pub(crate) reason: String,
}

impl RepoWorktreeVerdict {
    fn pinned(reason: impl Into<String>) -> Self {
        Self {
            state: RepoWorktreeState::Pinned,
            reason: reason.into(),
        }
    }

    fn reclaimable(reason: impl Into<String>) -> Self {
        Self {
            state: RepoWorktreeState::Reclaimable,
            reason: reason.into(),
        }
    }
}

/// Decide one worktree. Precedence, first match wins:
///
/// | condition                                  | state       | reason                     |
/// |--------------------------------------------|-------------|----------------------------|
/// | not evaluated                              | pinned      | `no verdict yet`           |
/// | path missing                               | dangling    | `path missing`             |
/// | main checkout                              | pinned      | `main checkout`            |
/// | locked by a live pid                       | pinned      | `locked by live pid`       |
/// | a process/session inside                   | pinned      | `process inside`           |
/// | process table unreadable                   | pinned      | `process table unavailable`|
/// | detached HEAD                              | pinned      | `detached`                 |
/// | dirty/untracked unknown                    | pinned      | `unverifiable`             |
/// | dirty                                      | pinned      | `dirty`                    |
/// | untracked files                            | pinned      | `untracked`                |
/// | open PR, managed and published, idle      | reclaimable | `idle-published`           |
/// | other open PR #N                          | pinned      | `open PR #N` or `grace`    |
/// | merged PR #N, tip covered                  | reclaimable | `merged via PR #N`         |
/// | merged PR #N, not covered, patch landed (#1961) | reclaimable | `merged via PR #N (patch match)` |
/// | merged PR #N, tip not covered              | pinned      | `commits after merge`      |
/// | merged PR #N, coverage unknown             | pinned      | `unverifiable`             |
/// | no PR / lookup unavailable, patch landed   | reclaimable | `landed by patch match`    |
/// | no PR, no patch match                      | pinned      | `no PR`                    |
/// | closed PR #N, no patch match (#1961)       | pinned      | `closed PR #N`             |
/// | under worktrees, 0 ahead, age >= 24 h (#1485) | reclaimable | `abandoned-empty`          |
/// | under worktrees, 0 ahead, younger/age unknown | pinned      | `grace`                    |
/// | lookup unavailable, no patch match         | pinned      | `unverifiable`             |
///
/// The abandoned-empty rows apply only when no PR is open or merged for the
/// branch and no patch match already decided. The idle-published rule can
/// also remove an open PR's unused checkout while retaining every ref.
pub(crate) fn repo_worktree_verdict(facts: &RepoWorktreeFacts) -> RepoWorktreeVerdict {
    if !facts.evaluated {
        return RepoWorktreeVerdict::pinned("no verdict yet");
    }
    if !facts.path_exists {
        return RepoWorktreeVerdict {
            state: RepoWorktreeState::Dangling,
            reason: "path missing".to_string(),
        };
    }
    if facts.is_main_checkout {
        return RepoWorktreeVerdict::pinned("main checkout");
    }
    if facts.locked_live_pid {
        return RepoWorktreeVerdict::pinned("locked by live pid");
    }
    if facts.process_inside {
        return RepoWorktreeVerdict::pinned("process inside");
    }
    if facts.processes_unverifiable {
        return RepoWorktreeVerdict::pinned("process table unavailable");
    }
    if facts.detached {
        return RepoWorktreeVerdict::pinned("detached");
    }
    let (Some(dirty), Some(untracked)) = (facts.dirty, facts.untracked) else {
        return RepoWorktreeVerdict::pinned("unverifiable");
    };
    if dirty {
        return RepoWorktreeVerdict::pinned("dirty");
    }
    if untracked {
        return RepoWorktreeVerdict::pinned("untracked");
    }
    let patch_landed = facts.patch_landed == Some(true);
    match facts.pr {
        PrFact::Open { .. } if facts.under_worktree_root && facts.published => {
            idle_verdict(facts.age_secs, IDLE_PUBLISHED)
        }
        PrFact::Open { number } => RepoWorktreeVerdict::pinned(format!("open PR #{number}")),
        PrFact::Merged {
            number,
            tip_covered: Some(true),
        } => RepoWorktreeVerdict::reclaimable(format!("merged via PR #{number}")),
        // #1961: a PR rebased before its squash merge leaves the local tip
        // uncovered by `headRefOid`; the squash commit's patch still proves
        // the work landed.
        PrFact::Merged { number, .. } if patch_landed => {
            RepoWorktreeVerdict::reclaimable(format!("merged via PR #{number} (patch match)"))
        }
        PrFact::Merged {
            tip_covered: Some(false),
            ..
        } => RepoWorktreeVerdict::pinned("commits after merge"),
        PrFact::Merged {
            tip_covered: None, ..
        } => RepoWorktreeVerdict::pinned("unverifiable"),
        PrFact::NoPr | PrFact::Unavailable | PrFact::Closed { .. } if patch_landed => {
            RepoWorktreeVerdict::reclaimable("landed by patch match")
        }
        PrFact::NoPr | PrFact::Unavailable | PrFact::Closed { .. }
            if facts.under_worktree_root && (facts.commits_ahead == Some(0) || facts.published) =>
        {
            let reason = if facts.commits_ahead == Some(0) {
                ABANDONED_EMPTY
            } else {
                IDLE_PUBLISHED
            };
            idle_verdict(facts.age_secs, reason)
        }
        PrFact::NoPr => RepoWorktreeVerdict::pinned("no PR"),
        PrFact::Closed { number } => RepoWorktreeVerdict::pinned(format!("closed PR #{number}")),
        PrFact::Unavailable => RepoWorktreeVerdict::pinned("unverifiable"),
    }
}

fn idle_verdict(age: Option<u64>, reason: &'static str) -> RepoWorktreeVerdict {
    if age.is_some_and(|age| age >= ABANDONED_EMPTY_GRACE_SECS) {
        RepoWorktreeVerdict::reclaimable(reason)
    } else {
        RepoWorktreeVerdict::pinned("grace")
    }
}

/// Issue #1486: facts for a direct child of `~/.clud/worktrees` that no
/// `git worktree list` claimed — typically a directory reserved through
/// `alloc_wt_path` that nobody used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReservedDirFacts {
    pub(crate) path_exists: bool,
    /// The directory holds a `.git` entry: a checkout the repo probe could
    /// not place (its repo moved, or git failed). Never reclaimed here.
    pub(crate) has_git_entry: bool,
    /// `Some(true)` when it has no entries at all; `None` when unreadable.
    pub(crate) empty: Option<bool>,
    pub(crate) process_inside: bool,
    pub(crate) processes_unverifiable: bool,
    /// Seconds since the directory's mtime; `None` when unknown.
    pub(crate) age_secs: Option<u64>,
}

/// Decide one unclaimed `worktrees` child. Precedence, first match wins:
///
/// | condition                          | state       | reason                     |
/// |------------------------------------|-------------|----------------------------|
/// | path missing                       | dangling    | `path missing`             |
/// | a process/session inside           | pinned      | `process inside`           |
/// | process table unreadable           | pinned      | `process table unavailable`|
/// | holds a `.git` entry               | pinned      | `unlisted checkout`        |
/// | emptiness unknown                  | pinned      | `unverifiable`             |
/// | not empty                          | pinned      | `not empty`                |
/// | empty, age >= 24 h                 | reclaimable | `reserved-unused`          |
/// | empty, younger/age unknown         | pinned      | `grace`                    |
///
/// Only an empty directory is ever reclaimable, and the executor removes it
/// with a plain `remove_dir`, which the OS refuses for a non-empty one.
pub(crate) fn reserved_dir_verdict(facts: &ReservedDirFacts) -> RepoWorktreeVerdict {
    if !facts.path_exists {
        return RepoWorktreeVerdict {
            state: RepoWorktreeState::Dangling,
            reason: "path missing".to_string(),
        };
    }
    if facts.process_inside {
        return RepoWorktreeVerdict::pinned("process inside");
    }
    if facts.processes_unverifiable {
        return RepoWorktreeVerdict::pinned("process table unavailable");
    }
    if facts.has_git_entry {
        return RepoWorktreeVerdict::pinned("unlisted checkout");
    }
    match facts.empty {
        None => RepoWorktreeVerdict::pinned("unverifiable"),
        Some(false) => RepoWorktreeVerdict::pinned("not empty"),
        Some(true) => match facts.age_secs {
            Some(age) if age >= ABANDONED_EMPTY_GRACE_SECS => {
                RepoWorktreeVerdict::reclaimable(RESERVED_UNUSED)
            }
            _ => RepoWorktreeVerdict::pinned("grace"),
        },
    }
}

/// The `gc list` reason for a reclaimable unused reservation (#1486).
pub(crate) const RESERVED_UNUSED: &str = "reserved-unused";
pub(crate) const IDLE_PUBLISHED: &str = "idle-published";
pub(crate) const ABANDONED_EMPTY: &str = "abandoned-empty";

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty, idle reservation two days old. Each test flips one fact.
    fn reservation() -> ReservedDirFacts {
        ReservedDirFacts {
            path_exists: true,
            has_git_entry: false,
            empty: Some(true),
            process_inside: false,
            processes_unverifiable: false,
            age_secs: Some(2 * DAY),
        }
    }

    fn assert_reserved(facts: ReservedDirFacts, state: RepoWorktreeState, reason: &str) {
        assert_eq!(
            reserved_dir_verdict(&facts),
            RepoWorktreeVerdict {
                state,
                reason: reason.to_string()
            },
            "facts: {facts:?}"
        );
    }

    // ---- #1486: reserved-unused. Spare + reason first. ----

    #[test]
    fn reserved_dir_spare_rows_each_carry_their_reason() {
        let cases = [
            (
                ReservedDirFacts {
                    process_inside: true,
                    ..reservation()
                },
                "process inside",
            ),
            (
                ReservedDirFacts {
                    processes_unverifiable: true,
                    ..reservation()
                },
                "process table unavailable",
            ),
            (
                ReservedDirFacts {
                    has_git_entry: true,
                    ..reservation()
                },
                "unlisted checkout",
            ),
            (
                ReservedDirFacts {
                    empty: None,
                    ..reservation()
                },
                "unverifiable",
            ),
            (
                ReservedDirFacts {
                    empty: Some(false),
                    age_secs: Some(400 * DAY),
                    ..reservation()
                },
                "not empty",
            ),
            (
                ReservedDirFacts {
                    age_secs: Some(DAY - 1),
                    ..reservation()
                },
                "grace",
            ),
            (
                ReservedDirFacts {
                    age_secs: None,
                    ..reservation()
                },
                "grace",
            ),
        ];
        for (facts, reason) in cases {
            assert_reserved(facts, Pinned, reason);
        }
    }

    #[test]
    fn a_missing_reservation_is_dangling() {
        assert_reserved(
            ReservedDirFacts {
                path_exists: false,
                ..reservation()
            },
            Dangling,
            "path missing",
        );
    }

    #[test]
    fn an_empty_idle_reservation_past_grace_is_reserved_unused() {
        assert_reserved(reservation(), Reclaimable, RESERVED_UNUSED);
        assert_reserved(
            ReservedDirFacts {
                age_secs: Some(DAY),
                ..reservation()
            },
            Reclaimable,
            "reserved-unused",
        );
    }

    /// A clean, idle, attached worktree whose branch is squash-merged via a
    /// PR that covers its tip. Each test flips exactly one fact.
    fn merged() -> RepoWorktreeFacts {
        RepoWorktreeFacts {
            evaluated: true,
            path_exists: true,
            is_main_checkout: false,
            locked_live_pid: false,
            process_inside: false,
            processes_unverifiable: false,
            detached: false,
            dirty: Some(false),
            untracked: Some(false),
            pr: PrFact::Merged {
                number: 1551,
                tip_covered: Some(true),
            },
            // Squash-merged: the tip is NOT an ancestor of main, so there is
            // no ancestry signal anywhere in these facts — only the PR.
            patch_landed: Some(false),
            under_worktree_root: false,
            commits_ahead: None,
            published: false,
            age_secs: Some(0),
        }
    }

    const DAY: u64 = 24 * 60 * 60;

    /// #1485: a clean, commit-free, idle worktree under `~/.clud/worktrees`
    /// with no PR — a reservation that was never used.
    fn empty_in_root(age_secs: u64) -> RepoWorktreeFacts {
        RepoWorktreeFacts {
            pr: PrFact::NoPr,
            patch_landed: Some(false),
            under_worktree_root: true,
            commits_ahead: Some(0),
            age_secs: Some(age_secs),
            ..merged()
        }
    }

    fn assert_verdict(facts: RepoWorktreeFacts, state: RepoWorktreeState, reason: &str) {
        assert_eq!(
            repo_worktree_verdict(&facts),
            RepoWorktreeVerdict {
                state,
                reason: reason.to_string()
            },
            "facts: {facts:?}"
        );
    }

    use RepoWorktreeState::{Dangling, Pinned, Reclaimable};

    #[test]
    fn managed_published_worktree_is_due_at_one_day() {
        for (age, state, reason) in [
            (DAY - 1, Pinned, "grace"),
            (DAY, Reclaimable, "idle-published"),
        ] {
            assert_verdict(
                RepoWorktreeFacts {
                    commits_ahead: Some(1),
                    published: true,
                    ..empty_in_root(age)
                },
                state,
                reason,
            );
        }
    }

    #[test]
    fn open_pr_allows_idle_checkout_reclaim_only_with_a_published_tip() {
        let facts = RepoWorktreeFacts {
            commits_ahead: Some(1),
            pr: PrFact::Open { number: 42 },
            ..empty_in_root(DAY)
        };
        assert_verdict(facts.clone(), Pinned, "open PR #42");
        assert_verdict(
            RepoWorktreeFacts {
                published: true,
                ..facts
            },
            Reclaimable,
            IDLE_PUBLISHED,
        );
    }

    // ---- Spare cases first: every guard asserts spare + reason. ----

    #[test]
    fn no_verdict_yet_is_spared() {
        let facts = RepoWorktreeFacts {
            evaluated: false,
            ..merged()
        };
        assert_verdict(facts, Pinned, "no verdict yet");
    }

    #[test]
    fn dirty_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                dirty: Some(true),
                ..merged()
            },
            Pinned,
            "dirty",
        );
    }

    #[test]
    fn untracked_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                untracked: Some(true),
                ..merged()
            },
            Pinned,
            "untracked",
        );
    }

    #[test]
    fn unreadable_status_is_spared_as_unverifiable() {
        assert_verdict(
            RepoWorktreeFacts {
                dirty: None,
                ..merged()
            },
            Pinned,
            "unverifiable",
        );
        assert_verdict(
            RepoWorktreeFacts {
                untracked: None,
                ..merged()
            },
            Pinned,
            "unverifiable",
        );
    }

    #[test]
    fn commits_after_merge_are_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::Merged {
                    number: 1551,
                    tip_covered: Some(false),
                },
                patch_landed: Some(false),
                ..merged()
            },
            Pinned,
            "commits after merge",
        );
    }

    /// #1961: a PR rebased on GitHub before its squash merge leaves the
    /// local tip uncovered by `headRefOid`. The patch match compares the
    /// whole `merge-base..tip` diff, so a commit made after the merge would
    /// make it false; a match proves everything local landed.
    #[test]
    fn merged_pr_rebased_before_merge_is_reclaimed_by_patch_match() {
        for tip_covered in [Some(false), None] {
            assert_verdict(
                RepoWorktreeFacts {
                    pr: PrFact::Merged {
                        number: 1576,
                        tip_covered,
                    },
                    patch_landed: Some(true),
                    ..merged()
                },
                Reclaimable,
                "merged via PR #1576 (patch match)",
            );
        }
    }

    #[test]
    fn closed_unmerged_pr_is_named_and_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::Closed { number: 42 },
                patch_landed: Some(false),
                ..merged()
            },
            Pinned,
            "closed PR #42",
        );
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::Closed { number: 42 },
                patch_landed: Some(true),
                ..merged()
            },
            Reclaimable,
            "landed by patch match",
        );
    }

    #[test]
    fn merged_pr_with_unknown_coverage_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::Merged {
                    number: 1551,
                    tip_covered: None,
                },
                ..merged()
            },
            Pinned,
            "unverifiable",
        );
    }

    #[test]
    fn open_pr_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::Open { number: 1580 },
                patch_landed: Some(true),
                ..merged()
            },
            Pinned,
            "open PR #1580",
        );
    }

    #[test]
    fn no_pr_and_no_patch_match_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::NoPr,
                patch_landed: Some(false),
                ..merged()
            },
            Pinned,
            "no PR",
        );
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::NoPr,
                patch_landed: None,
                ..merged()
            },
            Pinned,
            "no PR",
        );
    }

    #[test]
    fn gh_unavailable_without_patch_match_is_unverifiable() {
        for patch_landed in [Some(false), None] {
            assert_verdict(
                RepoWorktreeFacts {
                    pr: PrFact::Unavailable,
                    patch_landed,
                    ..merged()
                },
                Pinned,
                "unverifiable",
            );
        }
    }

    #[test]
    fn live_lock_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                locked_live_pid: true,
                ..merged()
            },
            Pinned,
            "locked by live pid",
        );
    }

    #[test]
    fn process_inside_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                process_inside: true,
                ..merged()
            },
            Pinned,
            "process inside",
        );
    }

    /// #1603: a process table that cannot be read proves nothing about who
    /// is inside, so it spares.
    #[test]
    fn unreadable_process_table_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                processes_unverifiable: true,
                ..merged()
            },
            Pinned,
            "process table unavailable",
        );
    }

    #[test]
    fn detached_head_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                detached: true,
                ..merged()
            },
            Pinned,
            "detached",
        );
    }

    #[test]
    fn main_checkout_is_spared() {
        assert_verdict(
            RepoWorktreeFacts {
                is_main_checkout: true,
                ..merged()
            },
            Pinned,
            "main checkout",
        );
    }

    #[test]
    fn missing_path_is_dangling() {
        assert_verdict(
            RepoWorktreeFacts {
                path_exists: false,
                ..merged()
            },
            Dangling,
            "path missing",
        );
    }

    /// Precedence: a live holder outranks dirtiness, which outranks the PR.
    #[test]
    fn guards_apply_in_documented_order() {
        assert_verdict(
            RepoWorktreeFacts {
                evaluated: false,
                path_exists: false,
                ..merged()
            },
            Pinned,
            "no verdict yet",
        );
        assert_verdict(
            RepoWorktreeFacts {
                process_inside: true,
                dirty: Some(true),
                ..merged()
            },
            Pinned,
            "process inside",
        );
        assert_verdict(
            RepoWorktreeFacts {
                dirty: Some(true),
                pr: PrFact::Open { number: 7 },
                ..merged()
            },
            Pinned,
            "dirty",
        );
    }

    // ---- Reclaim cases. ----

    /// The RED test of #1591: the branch is NOT an ancestor of main (squash
    /// merge; `patch_landed` is false too), yet the merged PR whose head
    /// covers the tip proves the work landed.
    #[test]
    fn squash_merged_branch_is_reclaimable_without_ancestry() {
        assert_verdict(merged(), Reclaimable, "merged via PR #1551");
    }

    #[test]
    fn gh_unavailable_but_patch_landed_is_reclaimable() {
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::Unavailable,
                patch_landed: Some(true),
                ..merged()
            },
            Reclaimable,
            "landed by patch match",
        );
    }

    #[test]
    fn no_pr_but_patch_landed_is_reclaimable() {
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::NoPr,
                patch_landed: Some(true),
                ..merged()
            },
            Reclaimable,
            "landed by patch match",
        );
    }

    // ---- #1485: abandoned-empty under worktrees. ----

    #[test]
    fn abandoned_empty_past_grace_is_reclaimable() {
        assert_verdict(empty_in_root(DAY), Reclaimable, "abandoned-empty");
        assert_verdict(empty_in_root(400 * DAY), Reclaimable, "abandoned-empty");
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::Unavailable,
                ..empty_in_root(2 * DAY)
            },
            Reclaimable,
            "abandoned-empty",
        );
    }

    #[test]
    fn abandoned_empty_inside_grace_is_spared_as_grace() {
        assert_verdict(empty_in_root(0), Pinned, "grace");
        assert_verdict(empty_in_root(DAY - 1), Pinned, "grace");
        assert_verdict(
            RepoWorktreeFacts {
                age_secs: None,
                ..empty_in_root(0)
            },
            Pinned,
            "grace",
        );
    }

    /// Outside worktrees an empty worktree is someone's hand-made checkout:
    /// the existing `no PR` spare stands however old it is.
    #[test]
    fn empty_worktree_outside_root_is_never_reclaimed_for_age() {
        assert_verdict(
            RepoWorktreeFacts {
                under_worktree_root: false,
                ..empty_in_root(400 * DAY)
            },
            Pinned,
            "no PR",
        );
    }

    /// Commits ahead (or an unknown count) is work: `no PR` / `unverifiable`.
    #[test]
    fn commits_with_no_pr_in_root_are_spared_at_any_age() {
        assert_verdict(
            RepoWorktreeFacts {
                commits_ahead: Some(3),
                ..empty_in_root(400 * DAY)
            },
            Pinned,
            "no PR",
        );
        assert_verdict(
            RepoWorktreeFacts {
                commits_ahead: None,
                ..empty_in_root(400 * DAY)
            },
            Pinned,
            "no PR",
        );
        assert_verdict(
            RepoWorktreeFacts {
                pr: PrFact::Unavailable,
                commits_ahead: Some(1),
                ..empty_in_root(400 * DAY)
            },
            Pinned,
            "unverifiable",
        );
    }

    /// #1485 acceptance 4: arbitrarily old age never changes a spare, even
    /// under worktrees with zero commits ahead.
    #[test]
    fn age_never_overrides_a_spare_in_root() {
        let old = empty_in_root(10_000 * DAY);
        let with = |f: &dyn Fn(&mut RepoWorktreeFacts)| {
            let mut facts = old.clone();
            f(&mut facts);
            facts
        };
        let cases: Vec<(RepoWorktreeFacts, &str)> = vec![
            (with(&|f| f.evaluated = false), "no verdict yet"),
            (with(&|f| f.dirty = Some(true)), "dirty"),
            (with(&|f| f.untracked = Some(true)), "untracked"),
            (with(&|f| f.locked_live_pid = true), "locked by live pid"),
            (with(&|f| f.process_inside = true), "process inside"),
            (with(&|f| f.detached = true), "detached"),
            (with(&|f| f.pr = PrFact::Open { number: 9 }), "open PR #9"),
            (
                with(&|f| {
                    f.pr = PrFact::Merged {
                        number: 9,
                        tip_covered: Some(false),
                    }
                }),
                "commits after merge",
            ),
            (with(&|f| f.commits_ahead = Some(2)), "no PR"),
        ];
        for (facts, reason) in cases {
            assert_verdict(facts, Pinned, reason);
        }
    }

    #[test]
    fn state_names_match_gc_list_json() {
        assert_eq!(Reclaimable.as_str(), "reclaimable");
        assert_eq!(Pinned.as_str(), "pinned");
        assert_eq!(Dangling.as_str(), "dangling");
    }
}

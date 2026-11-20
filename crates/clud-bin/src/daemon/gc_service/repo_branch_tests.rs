//! Decision table for [`repo_branch_verdict`]: every row asserts the action
//! and the reason, per the reap/spare rule in `CLAUDE.md`.

use super::*;

/// A clean, uncheckouted branch whose merged PR head is the tip. Each test
/// flips the facts it is about.
fn landed() -> RepoBranchFacts {
    RepoBranchFacts {
        checkout: BranchCheckout::None,
        dirty: None,
        pr: PrFact::Merged {
            number: 2859,
            tip_covered: Some(true),
        },
        patch_landed: None,
        commits_ahead: Some(1),
    }
}

fn assert_verdict(facts: RepoBranchFacts, action: BranchAction, reason: &str) {
    assert_eq!(
        repo_branch_verdict(&facts),
        RepoBranchVerdict {
            action,
            reason: reason.to_string()
        },
        "facts: {facts:?}"
    );
}

#[test]
fn merged_pr_covering_the_tip_retires() {
    assert_verdict(landed(), BranchAction::Retire, "merged via PR #2859");
}

/// The motivating case: the primary checkout parked on a squash-merged
/// branch is retired (switched back), not pinned like a main worktree.
#[test]
fn the_current_branch_retires_when_clean_and_landed() {
    let facts = RepoBranchFacts {
        checkout: BranchCheckout::Current,
        dirty: Some(false),
        ..landed()
    };
    assert_verdict(facts, BranchAction::Retire, "merged via PR #2859");
}

#[test]
fn the_current_branch_with_uncommitted_changes_is_kept() {
    let facts = RepoBranchFacts {
        checkout: BranchCheckout::Current,
        dirty: Some(true),
        ..landed()
    };
    assert_verdict(facts, BranchAction::Keep, "uncommitted changes");
}

#[test]
fn the_current_branch_with_unknown_status_is_kept() {
    let facts = RepoBranchFacts {
        checkout: BranchCheckout::Current,
        dirty: None,
        ..landed()
    };
    assert_verdict(facts, BranchAction::Keep, "unverifiable");
}

#[test]
fn a_branch_in_a_linked_worktree_is_left_to_the_worktree_reclaimer() {
    let facts = RepoBranchFacts {
        checkout: BranchCheckout::Worktree,
        ..landed()
    };
    assert_verdict(facts, BranchAction::Keep, "in a worktree");
}

#[test]
fn an_open_pr_keeps_the_branch_even_with_a_patch_match() {
    let facts = RepoBranchFacts {
        pr: PrFact::Open { number: 7 },
        patch_landed: Some(true),
        ..landed()
    };
    assert_verdict(facts, BranchAction::Keep, "open PR #7");
}

/// #1961's shape: rebased before the squash, so the head does not cover the
/// local tip, but the squash commit's patch proves the work landed.
#[test]
fn merged_pr_with_a_patch_match_retires() {
    let facts = RepoBranchFacts {
        pr: PrFact::Merged {
            number: 5,
            tip_covered: Some(false),
        },
        patch_landed: Some(true),
        ..landed()
    };
    assert_verdict(
        facts,
        BranchAction::Retire,
        "merged via PR #5 (patch match)",
    );
}

/// Work committed after the PR merged must never be deleted: it is named as
/// follow-up work so the agent opens a new PR.
#[test]
fn commits_after_the_merged_head_need_a_follow_up_pr() {
    let facts = RepoBranchFacts {
        pr: PrFact::Merged {
            number: 5,
            tip_covered: Some(false),
        },
        patch_landed: Some(false),
        ..landed()
    };
    assert_verdict(facts, BranchAction::FollowUp, "commits after merged PR #5");
}

#[test]
fn unknown_coverage_is_kept() {
    let facts = RepoBranchFacts {
        pr: PrFact::Merged {
            number: 5,
            tip_covered: None,
        },
        ..landed()
    };
    assert_verdict(facts, BranchAction::Keep, "unverifiable");
}

#[test]
fn a_patch_match_without_any_pr_retires() {
    for pr in [
        PrFact::NoPr,
        PrFact::Unavailable,
        PrFact::Closed { number: 3 },
    ] {
        let facts = RepoBranchFacts {
            pr,
            patch_landed: Some(true),
            ..landed()
        };
        assert_verdict(facts, BranchAction::Retire, "landed by patch match");
    }
}

#[test]
fn a_branch_with_no_commits_of_its_own_retires_unless_current() {
    let elsewhere = RepoBranchFacts {
        pr: PrFact::NoPr,
        patch_landed: Some(false),
        commits_ahead: Some(0),
        ..landed()
    };
    assert_verdict(
        elsewhere.clone(),
        BranchAction::Retire,
        "contained in default branch",
    );
    // A branch just created for new work has no commits yet; never nudge it.
    let current = RepoBranchFacts {
        checkout: BranchCheckout::Current,
        dirty: Some(false),
        ..elsewhere
    };
    assert_verdict(current, BranchAction::Keep, "no commits yet");
}

#[test]
fn unlanded_work_is_kept_with_the_reason_named() {
    let base = RepoBranchFacts {
        patch_landed: Some(false),
        commits_ahead: Some(2),
        ..landed()
    };
    assert_verdict(
        RepoBranchFacts {
            pr: PrFact::NoPr,
            ..base.clone()
        },
        BranchAction::Keep,
        "no PR",
    );
    assert_verdict(
        RepoBranchFacts {
            pr: PrFact::Closed { number: 9 },
            ..base.clone()
        },
        BranchAction::Keep,
        "closed PR #9",
    );
    assert_verdict(
        RepoBranchFacts {
            pr: PrFact::Unavailable,
            ..base
        },
        BranchAction::Keep,
        "unverifiable",
    );
}

/// A failed ahead count is doubt, never "no commits".
#[test]
fn an_unknown_ahead_count_does_not_read_as_empty() {
    let facts = RepoBranchFacts {
        pr: PrFact::NoPr,
        patch_landed: None,
        commits_ahead: None,
        ..landed()
    };
    assert_verdict(facts, BranchAction::Keep, "no PR");
}

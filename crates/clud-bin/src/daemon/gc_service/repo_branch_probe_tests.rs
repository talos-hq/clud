//! Pure tests for the remote-deletion decision (DD-230). The real-git
//! fixture lives in `repo_branch_exec_tests.rs`.

use super::*;

fn pr(number: u64, state: &str, head: &str, oid: &str, base: &str) -> BranchPr {
    BranchPr {
        number,
        state: state.to_string(),
        head_ref_name: head.to_string(),
        head_ref_oid: oid.to_string(),
        base_ref_name: base.to_string(),
    }
}

#[test]
fn a_merged_pr_with_the_exact_remote_head_allows_deletion() {
    let prs = [pr(4, "MERGED", "feat", "abc", "main")];
    assert_eq!(
        remote_branch_decision("feat", "abc", None, Some(&prs)),
        Ok("merged via PR #4".to_string())
    );
}

/// Someone pushed after the merge: the remote holds work no PR merged.
#[test]
fn a_remote_that_moved_past_the_merged_head_is_kept() {
    let prs = [pr(4, "MERGED", "feat", "abc", "main")];
    assert_eq!(
        remote_branch_decision("feat", "def", None, Some(&prs)),
        Err("no merged PR with this exact head".to_string())
    );
}

#[test]
fn an_open_pr_on_the_same_head_name_keeps_it() {
    let prs = [
        pr(4, "MERGED", "feat", "abc", "main"),
        pr(8, "OPEN", "feat", "def", "main"),
    ];
    assert_eq!(
        remote_branch_decision("feat", "abc", None, Some(&prs)),
        Err("open PR #8".to_string())
    );
}

/// Deleting a stacked PR's base would close that PR.
#[test]
fn the_base_of_an_open_pr_is_kept() {
    let prs = [
        pr(4, "MERGED", "feat", "abc", "main"),
        pr(9, "OPEN", "feat-2", "123", "feat"),
    ];
    assert_eq!(
        remote_branch_decision("feat", "abc", None, Some(&prs)),
        Err("base of open PR #9".to_string())
    );
}

/// A local branch already proven landed (patch match, no PR) may take its
/// remote copy along only when both point at the same commit.
#[test]
fn the_verified_local_tip_allows_deletion_only_at_the_same_commit() {
    let prs: [BranchPr; 0] = [];
    assert_eq!(
        remote_branch_decision("feat", "abc", Some("abc"), Some(&prs)),
        Ok("same tip as the retired local branch".to_string())
    );
    assert!(remote_branch_decision("feat", "abc", Some("zzz"), Some(&prs)).is_err());
}

#[test]
fn no_pr_lookup_means_no_remote_deletion() {
    assert_eq!(
        remote_branch_decision("feat", "abc", Some("abc"), None),
        Err("PR lookup unavailable".to_string())
    );
}

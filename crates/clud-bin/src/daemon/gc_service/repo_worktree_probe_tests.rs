//! Tests for `repo_worktree_probe` (#1591). The pure helpers are tested on
//! injected inputs; one real-git fixture exercises a squash merge end to end.
//! Every repository here lives in a `tempfile` directory — never a real
//! checkout — and every git call goes through `running-process`.

use super::*;
use crate::daemon::gc_service::repo_worktree::RepoWorktreeState;

fn pr(number: u64, state: &str, head: &str, oid: &str) -> PrRecord {
    PrRecord {
        number,
        state: state.to_string(),
        head_ref_name: head.to_string(),
        head_ref_oid: oid.to_string(),
    }
}

#[test]
fn pr_lookup_unavailable_is_unavailable() {
    let fact = pr_fact_for_branch("feat", None, &mut |_| Some(true));
    assert_eq!(fact, PrFact::Unavailable);
}

#[test]
fn open_pr_wins_over_a_merged_one_with_the_same_head() {
    let prs = [pr(1, "MERGED", "feat", "a"), pr(2, "OPEN", "feat", "b")];
    let fact = pr_fact_for_branch("feat", Some(&prs), &mut |_| Some(true));
    assert_eq!(fact, PrFact::Open { number: 2 });
}

#[test]
fn other_branches_prs_are_ignored() {
    let prs = [pr(1, "MERGED", "other", "a"), pr(3, "OPEN", "other", "c")];
    let fact = pr_fact_for_branch("feat", Some(&prs), &mut |_| Some(true));
    assert_eq!(fact, PrFact::NoPr);
}

#[test]
fn only_closed_prs_are_named_not_read_as_no_pr() {
    let prs = [pr(2, "CLOSED", "feat", "b"), pr(5, "CLOSED", "feat", "c")];
    let fact = pr_fact_for_branch("feat", Some(&prs), &mut |_| Some(true));
    assert_eq!(fact, PrFact::Closed { number: 5 });
}

#[test]
fn any_covering_merged_pr_proves_landing() {
    let prs = [
        pr(1, "MERGED", "feat", "old"),
        pr(9, "MERGED", "feat", "new"),
    ];
    let fact = pr_fact_for_branch("feat", Some(&prs), &mut |oid| Some(oid == "old"));
    assert_eq!(
        fact,
        PrFact::Merged {
            number: 1,
            tip_covered: Some(true)
        }
    );
}

#[test]
fn unknown_coverage_is_reported_over_not_covered() {
    let prs = [pr(1, "MERGED", "feat", "a"), pr(2, "MERGED", "feat", "b")];
    let fact = pr_fact_for_branch("feat", Some(&prs), &mut |oid| {
        if oid == "a" {
            None
        } else {
            Some(false)
        }
    });
    assert_eq!(
        fact,
        PrFact::Merged {
            number: 1,
            tip_covered: None
        }
    );
}

#[test]
fn normalize_diff_drops_blob_ids_and_hunk_positions() {
    let a = "diff --git a/f b/f\nindex 111..222 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n+x";
    let b = "diff --git a/f b/f\nindex 333..444 100644\n--- a/f\n+++ b/f\n@@ -9 +9 @@\n+x";
    assert_eq!(normalize_diff(a), normalize_diff(b));
    assert_ne!(normalize_diff(a), normalize_diff(&a.replace("+x", "+y")));
}

// ---- Real-git fixture ----

fn run_git(cwd: &Path, args: &[&str]) -> String {
    match probe_cmd("git", cwd, args, Duration::from_secs(30)) {
        Some((0, out)) => out,
        other => panic!(
            "git {} in {} failed: {other:?}",
            args.join(" "),
            cwd.display()
        ),
    }
}

fn identity(root: &Path) {
    run_git(root, &["config", "user.email", "t@example.com"]);
    run_git(root, &["config", "user.name", "t"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);
    run_git(root, &["config", "core.hooksPath", ""]);
}

fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) {
    std::fs::write(dir.join(name), body).unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", msg]);
}

struct Fixture {
    _tmp: kernal_api::platform::fs::TemporaryDirectory,
    repo: PathBuf,
    merged: PathBuf,
    dirty: PathBuf,
    fresh: PathBuf,
    merged_tip: String,
}

/// `origin` plus a clone `repo` with three sibling worktrees:
/// * `wt-merged` (`feat-a`): two commits, squash-merged into origin as one.
/// * `wt-dirty` (`feat-b`): also squash-merged, but holds an uncommitted edit.
/// * `wt-fresh` (`feat-c`): no commits of its own.
fn fixture() -> Fixture {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    run_git(&origin, &["init", "--initial-branch=main"]);
    identity(&origin);
    commit_file(&origin, "base.txt", "base\n", "base");

    let repo = root.join("repo");
    run_git(
        root,
        &["clone", origin.to_str().unwrap(), repo.to_str().unwrap()],
    );
    identity(&repo);

    let merged = root.join("wt-merged");
    let dirty = root.join("wt-dirty");
    let fresh = root.join("wt-fresh");
    for (dir, branch) in [(&merged, "feat-a"), (&dirty, "feat-b"), (&fresh, "feat-c")] {
        run_git(
            &repo,
            &["worktree", "add", "-b", branch, dir.to_str().unwrap()],
        );
    }
    commit_file(&merged, "a1.txt", "one\n", "a part 1");
    commit_file(&merged, "a2.txt", "two\n", "a part 2");
    commit_file(&dirty, "b.txt", "bee\n", "b");
    let merged_tip = run_git(&merged, &["rev-parse", "HEAD"]).trim().to_string();

    // Unrelated work lands on main first, so the squash commits' parents
    // differ from the branches' merge-base.
    commit_file(&origin, "other.txt", "other\n", "unrelated");
    // Squash merges: one new commit each, no ancestry to the branch tips.
    std::fs::write(origin.join("a1.txt"), "one\n").unwrap();
    commit_file(&origin, "a2.txt", "two\n", "feat a (#1)");
    commit_file(&origin, "b.txt", "bee\n", "feat b (#2)");

    run_git(&repo, &["fetch", "origin"]);
    run_git(&repo, &["remote", "set-head", "origin", "main"]);

    // The dirty sibling: work in progress after its merge.
    std::fs::write(dirty.join("b.txt"), "bee, edited\n").unwrap();

    Fixture {
        _tmp: tmp,
        repo,
        merged,
        dirty,
        fresh,
        merged_tip,
    }
}

fn verdict_for<'a>(rows: &'a [RepoWorktreeRow], path: &Path) -> &'a RepoWorktreeVerdict {
    let want = canonical(path);
    &rows
        .iter()
        .find(|r| canonical(Path::new(&r.path)) == want)
        .unwrap_or_else(|| panic!("no row for {}: {rows:?}", path.display()))
        .verdict
}

/// Precondition the whole feature rests on: after a squash merge the tip is
/// not an ancestor of the default branch, so ancestry cannot see it landed.
#[test]
fn squash_merged_tip_is_not_an_ancestor_of_main() {
    let fx = fixture();
    let status = probe_cmd(
        "git",
        &fx.merged,
        &["merge-base", "--is-ancestor", "HEAD", "origin/main"],
        Duration::from_secs(30),
    );
    assert_eq!(status.map(|(code, _)| code), Some(1));
}

/// `gh` unavailable: the patch fallback proves the squash-merged worktree
/// landed, the dirty sibling is kept, and a commit-less branch is never
/// judged landed.
#[test]
fn patch_fallback_reclaims_the_squash_merged_worktree_and_spares_the_rest() {
    let fx = fixture();
    let rows = probe_repo(&fx.repo, &[], None);

    let merged = verdict_for(&rows, &fx.merged);
    assert_eq!(merged.state, RepoWorktreeState::Reclaimable, "{merged:?}");
    assert_eq!(merged.reason, "landed by patch match");

    let dirty = verdict_for(&rows, &fx.dirty);
    assert_eq!(dirty.state, RepoWorktreeState::Pinned);
    assert_eq!(dirty.reason, "dirty");

    let fresh = verdict_for(&rows, &fx.fresh);
    assert_eq!(fresh.state, RepoWorktreeState::Pinned);
    assert_eq!(fresh.reason, "unverifiable");

    let main = verdict_for(&rows, &fx.repo);
    assert_eq!(main.state, RepoWorktreeState::Pinned);
    assert_eq!(main.reason, "main checkout");
}

/// With a PR lookup: the merged PR whose head is the tip decides, and a
/// commit added after the merge flips the verdict to spare.
#[test]
fn merged_pr_head_decides_and_later_commits_spare() {
    let fx = fixture();
    let prs = vec![pr(1, "MERGED", "feat-a", &fx.merged_tip)];
    let rows = probe_repo(&fx.repo, &[], Some(&prs));
    let merged = verdict_for(&rows, &fx.merged);
    assert_eq!(merged.state, RepoWorktreeState::Reclaimable);
    assert_eq!(merged.reason, "merged via PR #1");
    // No PR for feat-c and no patch: spared as "no PR".
    assert_eq!(verdict_for(&rows, &fx.fresh).reason, "no PR");

    commit_file(&fx.merged, "late.txt", "late\n", "after merge");
    let rows = probe_repo(&fx.repo, &[], Some(&prs));
    let merged = verdict_for(&rows, &fx.merged);
    assert_eq!(merged.state, RepoWorktreeState::Pinned);
    assert_eq!(merged.reason, "commits after merge");
}

/// A live session inside the worktree spares it even though it landed.
#[test]
fn live_session_cwd_inside_spares_a_landed_worktree() {
    let fx = fixture();
    let rows = probe_repo(&fx.repo, std::slice::from_ref(&fx.merged), None);
    let merged = verdict_for(&rows, &fx.merged);
    assert_eq!(merged.state, RepoWorktreeState::Pinned);
    assert_eq!(merged.reason, "process inside");
}

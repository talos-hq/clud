//! Real-git fixture for branch retirement (DD-230). Every repository lives in
//! a `tempfile` directory and every git call goes through `running-process`.

use std::path::{Path, PathBuf};

use super::*;
use crate::daemon::gc_service::extern_repo::probe_cmd;
use crate::daemon::gc_service::repo_branch_probe::probe_branches;

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
    origin: PathBuf,
    repo: PathBuf,
    tip_a: String,
}

/// `origin` and a clone parked on `feat-a`, the motivating shape:
/// * `feat-a` (current): pushed, squash-merged as PR #1 at its exact tip.
/// * `feat-b`: squash-merged with no PR record (patch evidence only).
/// * `feat-c`: unlanded work with no PR.
/// * `feat-d`: no commits of its own.
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

    run_git(&repo, &["switch", "-c", "feat-a"]);
    commit_file(&repo, "a1.txt", "one\n", "a part 1");
    commit_file(&repo, "a2.txt", "two\n", "a part 2");
    run_git(&repo, &["push", "-q", "-u", "origin", "feat-a"]);
    let tip_a = run_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    run_git(&repo, &["switch", "-c", "feat-b", "main"]);
    commit_file(&repo, "b.txt", "bee\n", "b");
    run_git(&repo, &["switch", "-c", "feat-c", "main"]);
    commit_file(&repo, "c.txt", "sea\n", "c");
    run_git(&repo, &["branch", "feat-d", "main"]);

    // Unrelated work first, then the two squash merges.
    commit_file(&origin, "other.txt", "other\n", "unrelated");
    std::fs::write(origin.join("a1.txt"), "one\n").unwrap();
    commit_file(&origin, "a2.txt", "two\n", "feat a (#1)");
    commit_file(&origin, "b.txt", "bee\n", "feat b");

    run_git(&repo, &["fetch", "-q", "origin"]);
    run_git(&repo, &["remote", "set-head", "origin", "main"]);
    run_git(&repo, &["switch", "-q", "feat-a"]);
    Fixture {
        _tmp: tmp,
        origin,
        repo,
        tip_a,
    }
}

fn prs(fx: &Fixture) -> Vec<BranchPr> {
    vec![BranchPr {
        number: 1,
        state: "MERGED".into(),
        head_ref_name: "feat-a".into(),
        head_ref_oid: fx.tip_a.clone(),
        base_ref_name: "main".into(),
    }]
}

fn report(fx: &Fixture) -> BranchReport {
    let prs = prs(fx);
    let lookup = move |_: &Path| Some(prs.clone());
    let setting = |_: &Path| Some(false);
    let opts = ProbeOptions {
        prs: &lookup,
        delete_on_merge: &setting,
        remote_only: false,
    };
    probe_branches(&fx.repo, &opts).expect("fixture is a checkout")
}

fn verdict<'a>(report: &'a BranchReport, name: &str) -> &'a BranchRow {
    report
        .rows
        .iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("no row for {name}: {:?}", report.rows))
}

#[test]
fn the_probe_judges_every_shape() {
    let fx = fixture();
    let report = report(&fx);
    assert_eq!(report.default_branch, "main");
    assert_eq!(report.delete_branch_on_merge, Some(false));
    let a = verdict(&report, "feat-a");
    assert_eq!(a.checkout, BranchCheckout::Current);
    assert_eq!(
        (a.verdict.action, a.verdict.reason.as_str()),
        (BranchAction::Retire, "merged via PR #1")
    );
    let b = &verdict(&report, "feat-b").verdict;
    assert_eq!(
        (b.action, b.reason.as_str()),
        (BranchAction::Retire, "landed by patch match")
    );
    let c = &verdict(&report, "feat-c").verdict;
    assert_eq!((c.action, c.reason.as_str()), (BranchAction::Keep, "no PR"));
    let d = &verdict(&report, "feat-d").verdict;
    assert_eq!(
        (d.action, d.reason.as_str()),
        (BranchAction::Retire, "contained in default branch")
    );
    assert!(
        report.rows.iter().all(|row| row.name != "main"),
        "the default branch is never judged"
    );
}

/// The end-to-end correction: the checkout leaves the landed branch, main
/// is fast-forwarded, landed branches go (origin copy too), live work stays.
#[test]
fn retire_switches_back_and_deletes_only_landed_branches() {
    let fx = fixture();
    let report = report(&fx);
    let outcomes = retire_branches(&report, Some(&prs(&fx)), true);

    assert_eq!(
        run_git(&fx.repo, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        run_git(&fx.repo, &["rev-parse", "main"]),
        run_git(&fx.repo, &["rev-parse", "origin/main"]),
        "main is fast-forwarded: {outcomes:?}"
    );
    let heads = run_git(
        &fx.repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    );
    let heads: Vec<&str> = heads.lines().collect();
    assert_eq!(heads, ["feat-c", "main"], "{outcomes:?}");
    let origin_heads = run_git(
        &fx.origin,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    );
    assert!(
        !origin_heads.lines().any(|l| l == "feat-a"),
        "origin/feat-a is deleted: {outcomes:?}"
    );
}

/// Work committed after the PR merged is follow-up, never deleted.
#[test]
fn commits_after_the_merge_are_never_retired() {
    let fx = fixture();
    commit_file(&fx.repo, "late.txt", "late\n", "after merge");
    let report = report(&fx);
    let a = &verdict(&report, "feat-a").verdict;
    assert_eq!(
        (a.action, a.reason.as_str()),
        (BranchAction::FollowUp, "commits after merged PR #1")
    );
    let outcomes = retire_branches(&report, Some(&prs(&fx)), true);
    assert!(outcomes.iter().all(|o| o.branch != "feat-a"));
    assert_eq!(
        run_git(&fx.repo, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "feat-a"
    );
}

/// A branch that moved between the report and the retire is kept.
#[test]
fn a_branch_that_moved_after_the_report_is_kept() {
    let fx = fixture();
    let report = report(&fx);
    run_git(&fx.repo, &["switch", "-q", "feat-b"]);
    commit_file(&fx.repo, "b2.txt", "more\n", "b more");
    run_git(&fx.repo, &["switch", "-q", "feat-a"]);
    let outcomes = retire_branches(&report, Some(&prs(&fx)), false);
    let b = outcomes.iter().find(|o| o.branch == "feat-b").unwrap();
    assert!(!b.retired);
    assert_eq!(b.notes, ["kept: branch moved"]);
}

/// Uncommitted edits pin the current branch even though it landed.
#[test]
fn uncommitted_changes_keep_the_current_branch() {
    let fx = fixture();
    std::fs::write(fx.repo.join("a1.txt"), "edited\n").unwrap();
    let report = report(&fx);
    let a = &verdict(&report, "feat-a").verdict;
    assert_eq!(
        (a.action, a.reason.as_str()),
        (BranchAction::Keep, "uncommitted changes")
    );
}

#[test]
fn push_status_reads_porcelain_lines() {
    let out = "To ../origin\n-\t:refs/heads/a\t[deleted]\n!\t:refs/heads/b\t[rejected] (stale info)\nDone\n";
    assert_eq!(push_ref_status(out, "a"), Ok(()));
    assert_eq!(
        push_ref_status(out, "b"),
        Err("[rejected] (stale info)".to_string())
    );
    assert_eq!(push_ref_status(out, "c"), Err("no push status".to_string()));
}

//! Issue #1606: `--clean-worktrees` shares the daemon's squash-aware verdict.
//!
//! Tier 1 is a pure decision table over injected facts (ancestry status,
//! lock state, verdict), asserting spare + reason. Tier 2 drives the real CLI
//! path against throwaway repos in `tempfile` directories — never a real
//! checkout, never the real `~/.clud`, never the network, and with an
//! injected all-clear process table.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::{
    decide_action, execute_candidate, plan_in, run_git, run_in, Action, CleanEnv, CleanOptions,
    LockStatus, StalenessInputs, WorktreeStatus,
};
use super::*;
use crate::daemon::repo_worktree_cli::{PrRecord, ProcessCwdSnapshot};
use crate::session_registry::MockLivenessProbe;

const DAY: u64 = 86_400;

fn days(n: u64) -> Duration {
    Duration::from_secs(DAY * n)
}

fn opts(force: bool) -> CleanOptions {
    CleanOptions {
        stale_after: days(1),
        dry_run: false,
        yes: true,
        force,
    }
}

fn inputs(status: WorktreeStatus, age: Duration, lock_status: LockStatus) -> StalenessInputs {
    StalenessInputs {
        status,
        age,
        lock_status,
        locked_hard_age: days(7),
    }
}

fn verdict(state: RepoWorktreeState, reason: &str) -> RepoWorktreeVerdict {
    RepoWorktreeVerdict {
        state,
        reason: reason.to_string(),
    }
}

const ALL_STATUSES: [WorktreeStatus; 5] = [
    WorktreeStatus::Clean,
    WorktreeStatus::Dirty,
    WorktreeStatus::Unpushed,
    WorktreeStatus::NoUpstream,
    WorktreeStatus::BranchGone,
];

/// Every verdict that spares, as `repo_worktree_verdict` spells it.
const SPARE_REASONS: [&str; 12] = [
    "main checkout",
    "locked by live pid",
    "process inside",
    "process table unavailable",
    "detached",
    "unverifiable",
    "dirty",
    "untracked",
    "open PR #3",
    "commits after merge",
    "no PR",
    "grace",
];

// ---- Tier 1: landed work becomes removable ----

#[test]
fn a_reclaimable_verdict_removes_what_ancestry_called_unpushed_or_no_upstream() {
    for (status, age) in [
        (WorktreeStatus::Unpushed, days(0)),
        (WorktreeStatus::Unpushed, days(30)),
        (WorktreeStatus::NoUpstream, days(0)),
        (WorktreeStatus::NoUpstream, days(30)),
        (WorktreeStatus::Clean, days(0)),
    ] {
        for reason in ["merged via PR #7", "landed by patch match"] {
            let act = decide_with_verdict(
                inputs(status, age, LockStatus::Unlocked),
                Some(&verdict(RepoWorktreeState::Reclaimable, reason)),
                &opts(false),
            );
            assert_eq!(
                act,
                Action::Reclaim(reason.to_string()),
                "{status:?} aged {age:?} with verdict {reason:?}"
            );
        }
    }
}

#[test]
fn a_reclaimable_verdict_also_takes_the_verdict_path_under_force() {
    let act = decide_with_verdict(
        inputs(WorktreeStatus::Unpushed, days(30), LockStatus::Unlocked),
        Some(&verdict(RepoWorktreeState::Reclaimable, "merged via PR #7")),
        &opts(true),
    );
    assert_eq!(act, Action::Reclaim("merged via PR #7".to_string()));
}

#[test]
fn a_hard_aged_stale_lock_prefix_carries_onto_a_verdict_removal() {
    let act = decide_with_verdict(
        inputs(
            WorktreeStatus::NoUpstream,
            days(8),
            LockStatus::DeadPid(4242),
        ),
        Some(&verdict(RepoWorktreeState::Reclaimable, "merged via PR #7")),
        &opts(false),
    );
    assert_eq!(
        act,
        Action::Reclaim("stale lock (dead pid 4242); merged via PR #7".to_string())
    );
}

// ---- Tier 1: every spare stays exactly as before, with its reason ----

#[test]
fn every_sparing_verdict_keeps_the_legacy_action_and_names_the_verdict() {
    for reason in SPARE_REASONS {
        let v = verdict(RepoWorktreeState::Pinned, reason);
        for force in [false, true] {
            for status in ALL_STATUSES {
                for age in [days(0), days(30)] {
                    let i = inputs(status, age, LockStatus::Unlocked);
                    let legacy = decide_action(i, &opts(force));
                    let act = decide_with_verdict(i, Some(&v), &opts(force));
                    match legacy {
                        Action::Skip(r) => assert_eq!(
                            act,
                            Action::Skip(format!("{r}; verdict: {reason}")),
                            "spare + reason: {status:?} {age:?} force={force} verdict={reason}"
                        ),
                        other => assert_eq!(
                            act, other,
                            "{status:?} {age:?} force={force} verdict={reason}"
                        ),
                    }
                }
            }
        }
    }
}

#[test]
fn unpushed_work_with_a_sparing_verdict_is_skipped_without_force() {
    for reason in ["no PR", "commits after merge", "open PR #3"] {
        let act = decide_with_verdict(
            inputs(WorktreeStatus::Unpushed, days(30), LockStatus::Unlocked),
            Some(&verdict(RepoWorktreeState::Pinned, reason)),
            &opts(false),
        );
        assert_eq!(
            act,
            Action::Skip(format!(
                "unpushed commits (use --force to remove); verdict: {reason}"
            ))
        );
    }
}

#[test]
fn a_fresh_live_lock_spares_even_a_reclaimable_verdict() {
    let act = decide_with_verdict(
        inputs(
            WorktreeStatus::NoUpstream,
            days(1),
            LockStatus::LivePid(9001),
        ),
        Some(&verdict(RepoWorktreeState::Reclaimable, "merged via PR #7")),
        &opts(true),
    );
    assert_eq!(act, Action::Skip("locked (live pid 9001)".to_string()));
}

#[test]
fn a_dirty_status_beats_a_stale_reclaimable_verdict() {
    // The probe and `git status` ran at different instants: doubt spares.
    let act = decide_with_verdict(
        inputs(WorktreeStatus::Dirty, days(30), LockStatus::Unlocked),
        Some(&verdict(RepoWorktreeState::Reclaimable, "merged via PR #7")),
        &opts(false),
    );
    assert_eq!(
        act,
        Action::Skip("dirty (use --force to remove); verdict: merged via PR #7".to_string())
    );
}

#[test]
fn no_verdict_or_a_dangling_one_is_exactly_the_legacy_decision() {
    let dangling = verdict(RepoWorktreeState::Dangling, "path missing");
    for force in [false, true] {
        for status in ALL_STATUSES {
            for age in [days(0), days(30)] {
                let i = inputs(status, age, LockStatus::Unlocked);
                let legacy = decide_action(i, &opts(force));
                assert_eq!(decide_with_verdict(i, None, &opts(force)), legacy);
                let with_dangling = decide_with_verdict(i, Some(&dangling), &opts(force));
                match legacy {
                    Action::Skip(r) => assert_eq!(
                        with_dangling,
                        Action::Skip(format!("{r}; verdict: path missing"))
                    ),
                    other => assert_eq!(with_dangling, other),
                }
            }
        }
    }
}

// ---- Tier 1: executor outcomes, incl. a concurrent daemon reclaim ----

#[test]
fn a_worktree_the_daemon_already_reclaimed_counts_as_skipped_not_failed() {
    assert_eq!(
        tally_reclaim(ReclaimOutcome::Spared(
            "gone from git worktree list".to_string()
        )),
        Tally::Skipped("verdict changed before removal: gone from git worktree list".to_string())
    );
    assert_eq!(
        tally_reclaim(ReclaimOutcome::Spared("now dirty".to_string())),
        Tally::Skipped("verdict changed before removal: now dirty".to_string())
    );
}

#[test]
fn executor_removals_and_failures_are_reported_as_such() {
    assert_eq!(
        tally_reclaim(ReclaimOutcome::Removed {
            notes: vec!["deleted branch feat".into(), "kept origin/feat: x".into()]
        }),
        Tally::Removed("deleted branch feat; kept origin/feat: x".to_string())
    );
    assert_eq!(
        tally_reclaim(ReclaimOutcome::Failed(
            "git worktree remove exited 128".into()
        )),
        Tally::Failed("git worktree remove exited 128".to_string())
    );
}

/// #1648: a verdict that missed the deadline never adds a removal. Every
/// ancestry skip or ignore becomes a skip naming the timeout; only an
/// ancestry removal (the pre-#1606 behavior) survives unchanged.
#[test]
fn a_timed_out_verdict_keeps_the_ancestry_decision_and_names_the_timeout() {
    let locks = [
        LockStatus::Unlocked,
        LockStatus::NoPid,
        LockStatus::LivePid(7),
        LockStatus::DeadPid(7),
    ];
    for force in [false, true] {
        for status in ALL_STATUSES {
            for age in [Duration::ZERO, days(3), days(30)] {
                for lock in locks {
                    let i = inputs(status, age, lock);
                    let got = decide_verdict_timed_out(i, &opts(force));
                    let expected = match decide_action(i, &opts(force)) {
                        Action::Skip(r) => Action::Skip(format!("{r}; verdict timed out")),
                        Action::Ignore => Action::Skip("verdict timed out".to_string()),
                        other => other,
                    };
                    assert_eq!(got, expected, "{status:?} {age:?} {lock:?} force={force}");
                    assert!(!matches!(got, Action::Reclaim(_)));
                }
            }
        }
    }
}

// ---- Tier 2: real git, temp repos only ----

fn git(cwd: &Path, args: &[&str]) -> String {
    run_git(cwd, args)
        .unwrap_or_else(|e| panic!("git {} in {}: {e}", args.join(" "), cwd.display()))
}

fn identity(root: &Path) {
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["config", "core.hooksPath", ""]);
}

fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) {
    std::fs::write(dir.join(name), body).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", msg]);
}

struct Fixture {
    _tmp: kernal_api::platform::fs::TemporaryDirectory,
    repo: PathBuf,
    merged: PathBuf,
    dirty: PathBuf,
    unmerged: PathBuf,
}

/// `origin` plus a clone `repo` with three worktrees, none with an upstream
/// (so ancestry alone calls every one `no-upstream`):
/// * `wt-merged` (`feat-a`): two commits, squash-merged into origin as one.
/// * `wt-dirty` (`feat-b`): also squash-merged, but holds an uncommitted edit.
/// * `wt-unmerged` (`feat-c`): a commit that never landed.
fn fixture() -> Fixture {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--initial-branch=main"]);
    identity(&origin);
    commit_file(&origin, "base.txt", "base\n", "base");

    let repo = root.join("repo");
    git(
        root,
        &["clone", origin.to_str().unwrap(), repo.to_str().unwrap()],
    );
    identity(&repo);

    let merged = root.join("wt-merged");
    let dirty = root.join("wt-dirty");
    let unmerged = root.join("wt-unmerged");
    for (dir, branch) in [
        (&merged, "feat-a"),
        (&dirty, "feat-b"),
        (&unmerged, "feat-c"),
    ] {
        git(
            &repo,
            &["worktree", "add", "-b", branch, dir.to_str().unwrap()],
        );
    }
    commit_file(&merged, "a1.txt", "one\n", "a part 1");
    commit_file(&merged, "a2.txt", "two\n", "a part 2");
    commit_file(&dirty, "b.txt", "bee\n", "b");
    commit_file(&unmerged, "c.txt", "sea\n", "c, never landed");

    commit_file(&origin, "other.txt", "other\n", "unrelated");
    std::fs::write(origin.join("a1.txt"), "one\n").unwrap();
    commit_file(&origin, "a2.txt", "two\n", "feat a (#1)");
    commit_file(&origin, "b.txt", "bee\n", "feat b (#2)");

    git(&repo, &["fetch", "origin"]);
    git(&repo, &["remote", "set-head", "origin", "main"]);
    std::fs::write(dirty.join("b.txt"), "bee, edited\n").unwrap();

    Fixture {
        _tmp: tmp,
        repo,
        merged,
        dirty,
        unmerged,
    }
}

fn no_prs(_: &Path) -> Option<Vec<PrRecord>> {
    None
}

fn all_clear() -> ProcessCwdSnapshot {
    ProcessCwdSnapshot {
        available: true,
        cwds: Vec::new(),
    }
}

fn with_env<T>(f: impl FnOnce(&CleanEnv<'_>) -> T) -> T {
    let liveness = MockLivenessProbe::with_alive([]);
    let env = CleanEnv {
        lookup_prs: Arc::new(no_prs),
        procs: Arc::new(all_clear),
        wt_root: None,
        liveness: &liveness,
        locked_hard_age: days(7),
        verdict_deadline: Duration::from_secs(60),
    };
    f(&env)
}

/// #1648: a verdict source that never answers in time.
fn with_slow_env<T>(deadline: Duration, f: impl FnOnce(&CleanEnv<'_>) -> T) -> T {
    let liveness = MockLivenessProbe::with_alive([]);
    let env = CleanEnv {
        lookup_prs: Arc::new(no_prs),
        procs: Arc::new(|| {
            std::thread::sleep(Duration::from_secs(30));
            all_clear()
        }),
        wt_root: None,
        liveness: &liveness,
        locked_hard_age: days(7),
        verdict_deadline: deadline,
    };
    f(&env)
}

fn same(a: &Path, b: &Path) -> bool {
    a.canonicalize().ok() == b.canonicalize().ok()
}

fn branch_exists(repo: &Path, name: &str) -> bool {
    run_git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .is_ok()
}

#[test]
fn dry_run_previews_the_verdict_and_reason_the_real_run_acts_on() {
    let fx = fixture();
    let mut o = opts(false);
    o.dry_run = true;
    let (_, plan) = with_env(|env| plan_in(&fx.repo, &o, env)).unwrap();

    assert_eq!(plan.candidates.len(), 1, "{plan:?}");
    let c = &plan.candidates[0];
    assert!(same(&c.entry.path, &fx.merged), "{plan:?}");
    assert_eq!(c.reason, "landed by patch match");
    assert!(c.via_verdict.is_some());
    // The dirty sibling is skipped with the verdict's own reason; the
    // unmerged one is fresh no-upstream, which ancestry ignores.
    assert!(!plan
        .candidates
        .iter()
        .any(|c| same(&c.entry.path, &fx.dirty) || same(&c.entry.path, &fx.unmerged)));
    let dirty = plan
        .skipped
        .iter()
        .find(|s| same(&s.entry.path, &fx.dirty))
        .expect("dirty worktree listed as skipped");
    assert_eq!(
        dirty.reason,
        "dirty (use --force to remove); verdict: dirty"
    );

    assert_eq!(with_env(|env| run_in(&fx.repo, &o, env)), 0);
    for dir in [&fx.merged, &fx.dirty, &fx.unmerged] {
        assert!(dir.exists(), "--dry-run touched {}", dir.display());
    }
}

/// #1648: the verdict phase honors its overall deadline, and a worktree
/// whose verdict did not arrive is spared with `verdict timed out` — a
/// missing verdict never makes anything removable.
#[test]
fn a_slow_verdict_source_is_cut_off_at_the_deadline_and_spares() {
    let fx = fixture();
    let started = Instant::now();
    let (_, plan) = with_slow_env(Duration::from_millis(500), |env| {
        plan_in(&fx.repo, &opts(false), env)
    })
    .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "plan took {elapsed:?} against a 500 ms verdict deadline"
    );
    assert!(plan.candidates.is_empty(), "{plan:?}");
    let merged = plan
        .skipped
        .iter()
        .find(|s| same(&s.entry.path, &fx.merged))
        .expect("merged worktree listed as skipped");
    assert_eq!(merged.reason, "verdict timed out");
    let dirty = plan
        .skipped
        .iter()
        .find(|s| same(&s.entry.path, &fx.dirty))
        .expect("dirty worktree listed as skipped");
    assert_eq!(
        dirty.reason,
        "dirty (use --force to remove); verdict timed out"
    );

    // A real run against the same slow source removes nothing.
    let started = Instant::now();
    assert_eq!(
        with_slow_env(Duration::from_millis(500), |env| run_in(
            &fx.repo,
            &opts(false),
            env
        )),
        0
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    for dir in [&fx.merged, &fx.dirty, &fx.unmerged] {
        assert!(
            dir.exists(),
            "a timed-out verdict removed {}",
            dir.display()
        );
    }
}

#[test]
fn the_cli_removes_a_squash_merged_worktree_and_keeps_dirty_and_unmerged_ones() {
    let fx = fixture();
    assert_eq!(with_env(|env| run_in(&fx.repo, &opts(false), env)), 0);

    assert!(
        !fx.merged.exists(),
        "squash-merged worktree must be removed"
    );
    assert!(
        !branch_exists(&fx.repo, "feat-a"),
        "branch deleted at the verified tip"
    );
    assert!(fx.dirty.exists(), "dirty worktree must survive");
    assert!(branch_exists(&fx.repo, "feat-b"));
    assert!(fx.unmerged.exists(), "unmerged worktree must survive");
    assert!(branch_exists(&fx.repo, "feat-c"));
    assert_eq!(
        std::fs::read_to_string(fx.dirty.join("b.txt")).unwrap(),
        "bee, edited\n",
        "the uncommitted edit is untouched"
    );
}

/// DD-129: no cross-process lock with the daemon. If the daemon reclaims the
/// worktree between the CLI's plan and its removal, the executor's fresh
/// re-probe spares it and the CLI reports a skip, not a failure.
#[test]
fn a_worktree_reclaimed_by_someone_else_after_planning_is_skipped() {
    let fx = fixture();
    let (_, plan) = with_env(|env| plan_in(&fx.repo, &opts(false), env)).unwrap();
    let c = plan
        .candidates
        .iter()
        .find(|c| same(&c.entry.path, &fx.merged))
        .expect("merged worktree planned");

    // Stand-in for the daemon's pool thread winning the race.
    git(
        &fx.repo,
        &["worktree", "remove", fx.merged.to_str().unwrap()],
    );

    let tally = with_env(|env| execute_candidate(&fx.repo, c, &opts(false), env));
    assert_eq!(
        tally,
        Tally::Skipped("verdict changed before removal: gone from git worktree list".to_string())
    );
}

/// A commit after planning moves the tip: the executor re-verifies and
/// spares instead of removing work the plan never saw.
#[test]
fn a_commit_after_planning_spares_the_worktree() {
    let fx = fixture();
    let (_, plan) = with_env(|env| plan_in(&fx.repo, &opts(false), env)).unwrap();
    let c = plan
        .candidates
        .iter()
        .find(|c| same(&c.entry.path, &fx.merged))
        .expect("merged worktree planned")
        .clone();
    commit_file(&fx.merged, "late.txt", "late\n", "after merge");

    let tally = with_env(|env| execute_candidate(&fx.repo, &c, &opts(false), env));
    assert!(
        matches!(&tally, Tally::Skipped(r) if r.starts_with("verdict changed before removal")),
        "{tally:?}"
    );
    assert!(fx.merged.exists());
}

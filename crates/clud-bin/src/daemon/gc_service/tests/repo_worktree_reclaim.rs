//! Issue #1603: real-git integration test for repo-worktree reclaim, driven
//! through the tick's repo-worktree phase (`run_repo_worktree_phase`) and the
//! real purge pool. Every repository lives in a `tempfile` directory and the
//! registry is a temp redb file: no real checkout and no real `~/.clud` is
//! touched. The origin is a local path, so the PR lookup is unavailable and
//! only the patch-match evidence can prove a squash merge — no network.

use super::*;
use crate::daemon::gc_service::extern_repo::probe_cmd;
use crate::daemon::gc_service::repo_worktree::{RepoWorktreeState, RepoWorktreeVerdict};

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

fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    matches!(
        probe_cmd("git", cwd, args, Duration::from_secs(30)),
        Some((0, _))
    )
}

fn identity(root: &Path) {
    run_git(root, &["config", "user.email", "t@example.com"]);
    run_git(root, &["config", "user.name", "t"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);
    run_git(root, &["config", "core.autocrlf", "false"]);
    run_git(root, &["config", "core.hooksPath", ""]);
    // A commit may start auto-maintenance as a detached process (git 2.46+)
    // that is still inside a worktree when the reclaim runs, so it is spared
    // as "process inside" and the test flakes.
    run_git(root, &["config", "maintenance.auto", "false"]);
    run_git(root, &["config", "gc.auto", "0"]);
}

fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) {
    fs::write(dir.join(name), body).unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", msg]);
}

fn has_branch(repo: &Path, branch: &str) -> bool {
    git_ok(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

fn age_checkout(root: &Path, path: &Path) {
    let admin = super::super::worktree_activity::registration(path, root).unwrap();
    let old = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 3600),
    );
    let tracked = run_git(path, &["ls-files", "-z"]);
    for file in [
        admin.join("HEAD"),
        admin.join("index"),
        admin.join("logs/HEAD"),
        path.to_owned(),
    ]
    .into_iter()
    .chain(
        tracked
            .trim_end_matches('\n')
            .split_terminator('\0')
            .map(|file| path.join(file)),
    ) {
        filetime::set_file_mtime(file, old).unwrap();
    }
}

#[test]
fn published_idle_worktree_is_removed_and_recent_edit_is_spared() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    fs::create_dir(&origin).unwrap();
    run_git(&origin, &["init", "--initial-branch=main"]);
    identity(&origin);
    commit_file(&origin, "base.txt", "base\n", "base");
    commit_file(&origin, ".gitignore", "target/\n", "ignore build output");
    let repo = tmp.path().join("repo");
    run_git(
        tmp.path(),
        &["clone", origin.to_str().unwrap(), repo.to_str().unwrap()],
    );
    identity(&repo);
    let wt_root = crate::gc::worktree_root::ensure_worktree_root_at(tmp.path()).unwrap();
    let published = wt_root.join("repo-wt-published");
    let recent = wt_root.join("repo-wt-recent");
    let unpushed = wt_root.join("repo-wt-unpushed");
    let pinned = wt_root.join("repo-wt-pinned");
    let active = wt_root.join("repo-wt-active");
    for (path, branch) in [
        (&published, "published"),
        (&recent, "recent"),
        (&unpushed, "unpushed"),
        (&pinned, "pinned"),
        (&active, "active"),
    ] {
        run_git(
            &repo,
            &["worktree", "add", "-b", branch, path.to_str().unwrap()],
        );
    }
    commit_file(&published, "new.txt", "published\n", "published");
    commit_file(&unpushed, "new.txt", "only local\n", "only local");
    run_git(&published, &["push", "origin", "HEAD:refs/heads/published"]);
    let target = published.join("target");
    fs::create_dir(&target).unwrap();
    fs::write(
        target.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::write(target.join("build-output"), "reproducible output").unwrap();
    for path in [&published, &recent, &unpushed, &pinned, &active] {
        age_checkout(&wt_root, path);
    }
    fs::write(recent.join("base.txt"), "base\n").unwrap();
    let keep_path = crate::gc::keep_list::keep_path_for(&wt_root);
    crate::gc::keep_list::update(&keep_path, &pinned.to_string_lossy(), true).unwrap();
    let registry = Registry::open_at(&tmp.path().join("gc.redb")).unwrap();
    registry
        .note_repo(&repo.to_string_lossy(), &repo.to_string_lossy(), now_unix())
        .unwrap();
    let pool = spawn_purge_pool(2);
    let (tx, rx) = mpsc::channel();
    let mut reasons = SpareReasons::new();
    let config = RepoWorktreeGcConfig {
        mode: ReclaimMode::Delete,
        delete_remote: false,
        wt_root: Some(wt_root),
    };
    let live = vec![active.clone()];
    run_repo_worktree_phase(
        &registry,
        &pool,
        &tx,
        None,
        &mut reasons,
        live.clone(),
        config.clone(),
    );
    apply_repo_snapshot(&rx, &mut reasons);
    assert_eq!(state_for(&reasons, &published).1, "idle-published");
    assert_eq!(state_for(&reasons, &recent).0, RepoWorktreeState::Pinned);
    assert_eq!(state_for(&reasons, &unpushed).0, RepoWorktreeState::Pinned);
    assert_eq!(state_for(&reasons, &active).1, "process inside");
    assert_eq!(
        dispatch_repo_worktree_reclaims(&pool, &tx, &mut reasons, live, &config),
        1
    );
    let outcomes = expect_reclaims(&rx, &mut reasons, 1);
    assert!(matches!(
        outcome_for(&outcomes, &published),
        ReclaimOutcome::Removed { .. }
    ));
    assert!(!published.exists());
    assert!(has_branch(&repo, "published"));
    assert!(!run_git(&repo, &["worktree", "list", "--porcelain"]).contains("repo-wt-published"));
    assert!([recent, unpushed, pinned, active]
        .iter()
        .all(|path| path.exists()));
}

/// Outcomes name worktrees as git spells them and the worktree is gone by
/// then, so compare leniently (8.3 short names, separators; #1628).
fn same_path(a: &str, b: &Path) -> bool {
    crate::path_norm::same_path(Path::new(a), b)
}

#[test]
fn janitor_quarantines_verified_partial_removal_and_keeps_unknown_files() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    run_git(&repo, &["init", "--initial-branch=main"]);
    identity(&repo);
    commit_file(&repo, "base.txt", "base\n", "base");
    commit_file(&repo, ".gitignore", "target/\n", "ignore build output");
    let wt_root = crate::gc::worktree_root::ensure_worktree_root_at(tmp.path()).unwrap();
    let remnant = wt_root.join("repo-wt-remnant");
    let unknown = wt_root.join("repo-wt-unknown");
    let surviving_git = wt_root.join("repo-wt-surviving-git");
    for (path, branch) in [
        (&remnant, "remnant"),
        (&unknown, "unknown"),
        (&surviving_git, "surviving-git"),
    ] {
        run_git(
            &repo,
            &["worktree", "add", "-b", branch, path.to_str().unwrap()],
        );
        if path == &remnant {
            let target = path.join("target");
            fs::create_dir(&target).unwrap();
            fs::write(
                target.join("CACHEDIR.TAG"),
                "Signature: 8a477f597d28d172789f06886806bc55\n",
            )
            .unwrap();
            fs::write(target.join("build-output"), "reproducible output").unwrap();
        }
        let tip = run_git(path, &["rev-parse", "HEAD"]);
        let mut receipt = super::super::worktree_remnants::RemovalReceipt::prepare(
            &wt_root,
            path,
            &repo,
            tip.trim(),
        )
        .unwrap();
        receipt.save(&wt_root).unwrap();
        let leftover = if path == &surviving_git {
            "base.txt"
        } else {
            ".git"
        };
        fs::remove_file(path.join(leftover)).unwrap();
        receipt.confirm_partial(&wt_root);
    }
    fs::write(unknown.join("untracked.txt"), "untracked work").unwrap();
    let registry = Registry::open_at(&tmp.path().join("gc.redb")).unwrap();
    registry
        .note_repo(&repo.to_string_lossy(), &repo.to_string_lossy(), now_unix())
        .unwrap();
    let pool = spawn_purge_pool(2);
    let (tx, rx) = mpsc::channel();
    let mut reasons = SpareReasons::new();
    run_repo_worktree_phase(
        &registry,
        &pool,
        &tx,
        None,
        &mut reasons,
        Vec::new(),
        RepoWorktreeGcConfig {
            mode: ReclaimMode::Delete,
            delete_remote: false,
            wt_root: Some(wt_root.clone()),
        },
    );
    apply_repo_snapshot(&rx, &mut reasons);
    assert_eq!(state_for(&reasons, &remnant).1, "remnant quarantined");
    assert_eq!(state_for(&reasons, &surviving_git).1, "remnant quarantined");
    assert!(!remnant.exists());
    assert!(!surviving_git.exists());
    assert!(unknown.join("untracked.txt").is_file());
    let registrations = run_git(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!registrations.contains("repo-wt-remnant"));
    assert!(!registrations.contains("repo-wt-surviving-git"));
    assert!(registrations.contains("repo-wt-unknown"));
    let trash = wt_root.with_file_name("trash");
    assert!(walkdir::WalkDir::new(trash)
        .into_iter()
        .filter_map(Result::ok)
        .any(|entry| {
            entry.file_name() == "base.txt" && fs::read_to_string(entry.path()).unwrap() == "base\n"
        }));
}

/// Wait for the off-worker probe's snapshot and install it, as the worker
/// loop would.
fn apply_repo_snapshot(rx: &mpsc::Receiver<RegistryMsg>, spare_reasons: &mut SpareReasons) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "repo-worktree probe never published");
        match rx.recv_timeout(remaining) {
            Ok(RegistryMsg::RepoWorktreeVerdicts(rows)) => {
                spare_reasons.replace_repo_worktrees(rows, now_unix());
                return;
            }
            Ok(RegistryMsg::Op(req)) if matches!(req.op, GcOp::Insert { .. }) => {
                req.reply_tx
                    .send(GcReply::InsertOk { inserted: true })
                    .unwrap();
            }
            _ => {}
        }
    }
}

/// Collect exactly `expected` reclaim outcomes, applying each as the worker
/// would. Other messages (the next probe's snapshot) are ignored.
fn expect_reclaims(
    rx: &mpsc::Receiver<RegistryMsg>,
    spare_reasons: &mut SpareReasons,
    expected: usize,
) -> Vec<(String, ReclaimOutcome)> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut out = Vec::new();
    while out.len() < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "got {} of {expected} reclaim outcomes: {out:?}",
            out.len()
        );
        if let Ok(RegistryMsg::RepoWorktreeReclaimed { path, outcome }) = rx.recv_timeout(remaining)
        {
            apply_repo_worktree_reclaimed(spare_reasons, &path, &outcome);
            out.push((path, outcome));
        }
    }
    out
}

fn outcome_for<'a>(outcomes: &'a [(String, ReclaimOutcome)], path: &Path) -> &'a ReclaimOutcome {
    &outcomes
        .iter()
        .find(|(p, _)| same_path(p, path))
        .unwrap_or_else(|| panic!("no outcome for {}: {outcomes:?}", path.display()))
        .1
}

fn state_for(spare_reasons: &SpareReasons, path: &Path) -> (RepoWorktreeState, String) {
    let (rows, _) = spare_reasons.repo_worktrees();
    let row = rows
        .iter()
        .find(|r| same_path(&r.path, path))
        .unwrap_or_else(|| panic!("no row for {}: {rows:?}", path.display()));
    (row.verdict.state, row.verdict.reason.clone())
}

/// After a squash merge, a real tick removes the landed worktree and its
/// local branch; a dirty sibling, an unmerged sibling and the main checkout
/// are kept; and a sibling that turns dirty between the probe and the delete
/// is spared by the pool's re-verification.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn a_real_tick_reclaims_only_the_squash_merged_clean_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let origin = root.join("origin");
    fs::create_dir_all(&origin).unwrap();
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
    let race = root.join("wt-race");
    let dirty = root.join("wt-dirty");
    let unmerged = root.join("wt-unmerged");
    for (dir, branch) in [
        (&merged, "feat-merged"),
        (&race, "feat-race"),
        (&dirty, "feat-dirty"),
        (&unmerged, "feat-unmerged"),
    ] {
        run_git(
            &repo,
            &["worktree", "add", "-b", branch, dir.to_str().unwrap()],
        );
    }
    commit_file(&merged, "m1.txt", "m1\n", "merged part 1");
    commit_file(&merged, "m2.txt", "m2\n", "merged part 2");
    commit_file(&race, "r.txt", "r\n", "race");
    commit_file(&dirty, "d.txt", "d\n", "dirty");
    commit_file(&unmerged, "u.txt", "u\n", "never merged");

    // Unrelated work first, then one squash commit per landed branch.
    commit_file(&origin, "other.txt", "other\n", "unrelated");
    fs::write(origin.join("m1.txt"), "m1\n").unwrap();
    commit_file(&origin, "m2.txt", "m2\n", "merged (#1)");
    commit_file(&origin, "r.txt", "r\n", "race (#2)");
    commit_file(&origin, "d.txt", "d\n", "dirty (#3)");
    run_git(&repo, &["fetch", "origin"]);
    run_git(&repo, &["remote", "set-head", "origin", "main"]);
    // Work in progress after its merge.
    fs::write(dirty.join("d.txt"), "d, edited\n").unwrap();

    let registry = Registry::open_at(&root.join("gc.redb")).unwrap();
    registry
        .record_repo_visit(&repo.to_string_lossy(), &repo.to_string_lossy(), now_unix())
        .unwrap();
    let pool_tx = spawn_purge_pool(2);
    let (completion_tx, rx) = mpsc::channel::<RegistryMsg>();
    let mut spare_reasons = SpareReasons::new();
    let config = RepoWorktreeGcConfig {
        mode: ReclaimMode::Delete,
        delete_remote: false,
        wt_root: None,
    };

    // Tick 1: nothing cached yet, so nothing is dispatched; the probe runs.
    let dispatched = run_repo_worktree_phase(
        &registry,
        &pool_tx,
        &completion_tx,
        None,
        &mut spare_reasons,
        Vec::new(),
        config.clone(),
    );
    assert_eq!(dispatched, 0, "no snapshot yet: nothing may be dispatched");
    apply_repo_snapshot(&rx, &mut spare_reasons);

    use RepoWorktreeState::{Pinned, Reclaimable};
    assert_eq!(
        state_for(&spare_reasons, &merged),
        (Reclaimable, "landed by patch match".to_string())
    );
    assert_eq!(state_for(&spare_reasons, &race).0, Reclaimable);
    assert_eq!(
        state_for(&spare_reasons, &dirty),
        (Pinned, "dirty".to_string())
    );
    assert_eq!(
        state_for(&spare_reasons, &unmerged),
        (Pinned, "unverifiable".to_string())
    );
    assert_eq!(
        state_for(&spare_reasons, &repo),
        (Pinned, "main checkout".to_string())
    );

    // The race: the developer comes back to wt-race after the probe.
    fs::write(race.join("r.txt"), "r, edited\n").unwrap();

    // Tick 2: both cached-reclaimable rows are dispatched; the pool
    // re-verifies each from scratch before touching it.
    let dispatched = run_repo_worktree_phase(
        &registry,
        &pool_tx,
        &completion_tx,
        None,
        &mut spare_reasons,
        Vec::new(),
        config,
    );
    assert_eq!(dispatched, 2, "exactly the two reclaimable rows");
    let outcomes = expect_reclaims(&rx, &mut spare_reasons, 2);

    assert!(
        matches!(
            outcome_for(&outcomes, &merged),
            ReclaimOutcome::Removed { .. }
        ),
        "{outcomes:?}"
    );
    assert!(!merged.exists(), "landed worktree dir must be gone");
    assert!(
        !has_branch(&repo, "feat-merged"),
        "local branch must be gone"
    );
    assert!(
        !run_git(&repo, &["worktree", "list", "--porcelain"]).contains("wt-merged"),
        "git must no longer list it"
    );
    assert!(
        spare_reasons
            .repo_worktrees()
            .0
            .iter()
            .all(|r| !same_path(&r.path, &merged)),
        "gc list must drop the removed row"
    );

    assert_eq!(
        outcome_for(&outcomes, &race),
        &ReclaimOutcome::Spared("now dirty".to_string()),
        "the pool must veto a worktree that turned dirty after the probe"
    );
    assert!(race.exists());
    assert!(has_branch(&repo, "feat-race"));
    assert_eq!(
        fs::read_to_string(race.join("r.txt")).unwrap(),
        "r, edited\n",
        "the racing edit must survive"
    );

    for (dir, branch) in [(&dirty, "feat-dirty"), (&unmerged, "feat-unmerged")] {
        assert!(dir.exists(), "{} must be kept", dir.display());
        assert!(has_branch(&repo, branch), "{branch} must be kept");
    }
    assert_eq!(
        fs::read_to_string(dirty.join("d.txt")).unwrap(),
        "d, edited\n"
    );
    assert!(repo.join(".git").exists(), "main checkout untouched");
    assert!(has_branch(&repo, "main"));
}

/// Observe mode reports and never dispatches, even for a reclaimable row.
#[test]
fn observe_mode_never_dispatches() {
    use crate::daemon::gc_service::repo_worktree::RepoWorktreeVerdict;
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path().join("wt");
    fs::create_dir_all(&wt).unwrap();
    let registry = Registry::open_at(&tmp.path().join("gc.redb")).unwrap();
    let mut spare_reasons = SpareReasons::new();
    spare_reasons.replace_repo_worktrees(
        vec![RepoWorktreeRow {
            path: wt.to_string_lossy().to_string(),
            repo_root: tmp.path().to_string_lossy().to_string(),
            branch: Some("feat".to_string()),
            tip: Some("abc".to_string()),
            mtime_unix: 0,
            verdict: RepoWorktreeVerdict {
                state: RepoWorktreeState::Reclaimable,
                reason: "merged via PR #1".to_string(),
            },
            reservation: false,
        }],
        now_unix(),
    );
    let pool_tx = spawn_purge_pool(1);
    let (completion_tx, _rx) = mpsc::channel::<RegistryMsg>();
    let dispatched = run_repo_worktree_phase(
        &registry,
        &pool_tx,
        &completion_tx,
        None,
        &mut spare_reasons,
        Vec::new(),
        RepoWorktreeGcConfig {
            mode: ReclaimMode::Observe,
            delete_remote: false,
            wt_root: None,
        },
    );
    assert_eq!(dispatched, 0);
    assert!(wt.exists());
}

/// Issue #1485: worktrees under a (tempdir) `worktrees` root are probed even
/// when their repo was never visited, a squash-merged one and an
/// abandoned-empty one past the 24 h grace are reclaimed, a fresh empty one
/// is spared as `grace`, a dirty sibling survives, and the root itself is
/// never removed.
#[test]
fn a_real_tick_reclaims_tmp_wt_landed_and_abandoned_empty_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let origin = root.join("origin");
    fs::create_dir_all(&origin).unwrap();
    run_git(&origin, &["init", "--initial-branch=main"]);
    identity(&origin);
    commit_file(&origin, "base.txt", "base\n", "base");
    let repo = root.join("repo");
    run_git(
        root,
        &["clone", origin.to_str().unwrap(), repo.to_str().unwrap()],
    );
    identity(&repo);

    // A fake clud home: `<home>/.clud/worktrees`, never the real one.
    let wt_root = crate::gc::worktree_root::ensure_worktree_root_at(&root.join("home")).unwrap();
    let merged = wt_root.join("repo-wt-merged");
    let empty_old = wt_root.join("repo-wt-empty-old");
    let empty_new = wt_root.join("repo-wt-empty-new");
    let dirty = wt_root.join("repo-wt-dirty");
    for (dir, branch) in [
        (&merged, "feat-merged"),
        (&empty_old, "feat-empty-old"),
        (&empty_new, "feat-empty-new"),
        (&dirty, "feat-dirty"),
    ] {
        run_git(
            &repo,
            &["worktree", "add", "-b", branch, dir.to_str().unwrap()],
        );
    }
    commit_file(&merged, "m.txt", "m\n", "merged");
    commit_file(&dirty, "d.txt", "d\n", "dirty");
    commit_file(&origin, "m.txt", "m\n", "merged (#1)");
    run_git(&repo, &["fetch", "origin"]);
    run_git(&repo, &["remote", "set-head", "origin", "main"]);
    fs::write(dirty.join("d.txt"), "d, edited\n").unwrap();
    age_checkout(&wt_root, &empty_old);

    // No record_repo_visit: the worktrees children alone seed the probe.
    let registry = Registry::open_at(&root.join("gc.redb")).unwrap();
    let pool_tx = spawn_purge_pool(2);
    let (completion_tx, rx) = mpsc::channel::<RegistryMsg>();
    let mut spare_reasons = SpareReasons::new();
    let config = RepoWorktreeGcConfig {
        mode: ReclaimMode::Delete,
        delete_remote: false,
        wt_root: Some(wt_root.clone()),
    };
    let dispatched = run_repo_worktree_phase(
        &registry,
        &pool_tx,
        &completion_tx,
        None,
        &mut spare_reasons,
        Vec::new(),
        config.clone(),
    );
    assert_eq!(dispatched, 0);
    apply_repo_snapshot(&rx, &mut spare_reasons);

    use RepoWorktreeState::{Pinned, Reclaimable};
    assert_eq!(
        state_for(&spare_reasons, &merged),
        (Reclaimable, "landed by patch match".to_string())
    );
    assert_eq!(
        state_for(&spare_reasons, &empty_old),
        (Reclaimable, "abandoned-empty".to_string())
    );
    let (state, reason) = state_for(&spare_reasons, &empty_new);
    assert_eq!(state, Pinned);
    assert!(reason.starts_with("grace until unix "), "{reason}");
    assert_eq!(
        state_for(&spare_reasons, &dirty),
        (Pinned, "dirty".to_string())
    );

    let dispatched = run_repo_worktree_phase(
        &registry,
        &pool_tx,
        &completion_tx,
        None,
        &mut spare_reasons,
        Vec::new(),
        config,
    );
    assert_eq!(dispatched, 2, "the landed and the abandoned-empty rows");
    let outcomes = expect_reclaims(&rx, &mut spare_reasons, 2);
    for (dir, branch) in [(&merged, "feat-merged"), (&empty_old, "feat-empty-old")] {
        assert!(
            matches!(outcome_for(&outcomes, dir), ReclaimOutcome::Removed { .. }),
            "{outcomes:?}"
        );
        assert!(!dir.exists(), "{} must be gone", dir.display());
        assert!(!has_branch(&repo, branch), "{branch} must be gone");
    }
    for (dir, branch) in [(&empty_new, "feat-empty-new"), (&dirty, "feat-dirty")] {
        assert!(dir.exists(), "{} must be kept", dir.display());
        assert!(has_branch(&repo, branch), "{branch} must be kept");
    }
    assert!(
        wt_root.is_dir(),
        "the worktrees root is never a removal target"
    );
}

/// Issue #1486: an `alloc_wt_path` reservation nobody used is an empty,
/// non-git `worktrees` child. Past the 24 h grace a real tick removes it as
/// `reserved-unused`; a fresh one is spared as `grace`; an old one that holds
/// a file and no receipt is spared as unknown and never deleted; one that gains a
/// file between the probe and the delete is spared by the pool's re-check.
#[test]
fn a_real_tick_reclaims_only_old_empty_reservations() {
    let tmp = tempfile::tempdir().unwrap();
    let wt_root =
        crate::gc::worktree_root::ensure_worktree_root_at(&tmp.path().join("home")).unwrap();
    let alloc = |suffix: &str| {
        crate::gc::worktree_root::alloc_wt_path_in(&wt_root, "repo", suffix).unwrap()
    };
    let old_empty = alloc("1");
    let fresh_empty = alloc("2");
    let old_full = alloc("3");
    let race = alloc("4");
    fs::write(old_full.join("work.txt"), "keep me\n").unwrap();
    let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
    for dir in [&old_empty, &old_full, &race] {
        filetime::set_file_mtime(dir, filetime::FileTime::from_system_time(two_days_ago)).unwrap();
    }

    let registry = Registry::open_at(&tmp.path().join("gc.redb")).unwrap();
    let pool_tx = spawn_purge_pool(2);
    let (completion_tx, rx) = mpsc::channel::<RegistryMsg>();
    let mut spare_reasons = SpareReasons::new();
    let config = RepoWorktreeGcConfig {
        mode: ReclaimMode::Delete,
        delete_remote: false,
        wt_root: Some(wt_root.clone()),
    };
    let phase = |spare_reasons: &mut SpareReasons| {
        run_repo_worktree_phase(
            &registry,
            &pool_tx,
            &completion_tx,
            None,
            spare_reasons,
            Vec::new(),
            config.clone(),
        )
    };
    assert_eq!(phase(&mut spare_reasons), 0);
    apply_repo_snapshot(&rx, &mut spare_reasons);

    use RepoWorktreeState::{Pinned, Reclaimable};
    for dir in [&old_empty, &race] {
        assert_eq!(
            state_for(&spare_reasons, dir),
            (Reclaimable, "reserved-unused".to_string())
        );
    }
    assert_eq!(
        state_for(&spare_reasons, &fresh_empty),
        (Pinned, "grace".to_string())
    );
    assert_eq!(
        state_for(&spare_reasons, &old_full),
        (Pinned, "unknown remnant provenance".to_string())
    );

    // A probe may hold a Git child cwd open; do not dispatch deletion yet.
    assert!(spare_reasons.begin_repo_probe());
    assert_eq!(phase(&mut spare_reasons), 0);
    spare_reasons.repo_probe_finished();

    // The race: an agent starts using the reservation after the probe.
    fs::write(race.join("new.txt"), "fresh work\n").unwrap();

    assert_eq!(
        phase(&mut spare_reasons),
        2,
        "the two cached reclaimable rows"
    );
    assert!(
        !spare_reasons.begin_repo_probe(),
        "a new probe must wait for pending deletions"
    );
    let outcomes = expect_reclaims(&rx, &mut spare_reasons, 2);
    assert!(
        matches!(
            outcome_for(&outcomes, &old_empty),
            ReclaimOutcome::Removed { .. }
        ),
        "{outcomes:?}"
    );
    assert!(!old_empty.exists(), "the unused reservation must be gone");
    assert_eq!(
        outcome_for(&outcomes, &race),
        &ReclaimOutcome::Spared("now not empty".to_string())
    );
    assert_eq!(
        fs::read_to_string(race.join("new.txt")).unwrap(),
        "fresh work\n"
    );
    assert!(fresh_empty.is_dir());
    assert_eq!(
        fs::read_to_string(old_full.join("work.txt")).unwrap(),
        "keep me\n"
    );
    assert!(wt_root.is_dir(), "the root is never a removal target");
}

#[test]
fn spared_and_failed_batches_refresh_before_the_next_reclaim() {
    for outcome in [
        ReclaimOutcome::Spared("now not empty".to_string()),
        ReclaimOutcome::Failed("sharing violation".to_string()),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Registry::open_at(&tmp.path().join("gc.redb")).unwrap();
        let mut state = SpareReasons::new();
        let path = tmp.path().join("reservation").to_string_lossy().to_string();
        state.replace_repo_worktrees(
            vec![RepoWorktreeRow {
                path: path.clone(),
                repo_root: String::new(),
                branch: None,
                tip: None,
                mtime_unix: 0,
                verdict: RepoWorktreeVerdict {
                    state: RepoWorktreeState::Reclaimable,
                    reason: "reserved-unused".to_string(),
                },
                reservation: true,
            }],
            0,
        );
        state.begin_repo_reclaim(path.clone());
        assert!(!state.begin_repo_probe());
        apply_repo_worktree_reclaimed(&mut state, &path, &outcome);
        assert!(state.repo_snapshot_needs_refresh());
        let (pool_tx, pool_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        assert_eq!(
            run_repo_worktree_phase(
                &registry,
                &pool_tx,
                &completion_tx,
                None,
                &mut state,
                Vec::new(),
                RepoWorktreeGcConfig {
                    mode: ReclaimMode::Delete,
                    delete_remote: false,
                    wt_root: None,
                },
            ),
            0
        );
        assert!(matches!(pool_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        apply_repo_snapshot(&completion_rx, &mut state);
        assert!(!state.repo_snapshot_needs_refresh());
        assert!(state.begin_repo_probe());
    }
}

//! Issue #1632: reclaim jobs are serialized per repo root and only per repo
//! root. These drive `serialize_by_repo` with an injected job body, so they
//! run on every platform without git.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use super::super::repo_worktree::{RepoWorktreeState, RepoWorktreeVerdict};
use super::*;

fn job(repo_root: &str, path: &str) -> ReclaimJob {
    ReclaimJob {
        row: RepoWorktreeRow {
            path: path.to_string(),
            repo_root: repo_root.to_string(),
            branch: Some("feat".to_string()),
            tip: None,
            mtime_unix: 0,
            verdict: RepoWorktreeVerdict {
                state: RepoWorktreeState::Reclaimable,
                reason: "landed".to_string(),
            },
            reservation: false,
        },
        delete_remote: false,
        session_cwds: Vec::new(),
        wt_root: None,
    }
}

/// Run `jobs` on one thread each, released together, and return the most
/// bodies that were ever inside `serialize_by_repo` at once.
fn max_overlap(locks: &Arc<RepoLocks>, jobs: Vec<ReclaimJob>) -> usize {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(jobs.len()));
    let handles: Vec<_> = jobs
        .into_iter()
        .map(|job| {
            let (locks, active, peak, start) = (
                Arc::clone(locks),
                Arc::clone(&active),
                Arc::clone(&peak),
                Arc::clone(&start),
            );
            thread::spawn(move || {
                start.wait();
                serialize_by_repo(&locks, &job, || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(100));
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    peak.load(Ordering::SeqCst)
}

#[test]
fn two_reclaims_of_one_repo_never_overlap() {
    let repo = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let root = repo.path().to_string_lossy().to_string();
    let locks = Arc::new(RepoLocks::default());
    let jobs = (0..4).map(|i| job(&root, &format!("wt-{i}"))).collect();
    assert_eq!(
        max_overlap(&locks, jobs),
        1,
        "#1632: git worktree remove/branch -D/prune on one repo must not run concurrently"
    );
    assert!(
        locks.map.lock().unwrap().is_empty(),
        "an idle repo keeps no lock entry"
    );
}

#[test]
fn two_spellings_of_one_repo_share_a_lock() {
    let repo = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let plain = repo.path().to_string_lossy().to_string();
    let dotted = repo.path().join(".").to_string_lossy().to_string();
    let locks = Arc::new(RepoLocks::default());
    let jobs = vec![job(&plain, "a"), job(&dotted, "b")];
    assert_eq!(max_overlap(&locks, jobs), 1);
}

#[test]
fn reclaims_of_different_repos_still_run_concurrently() {
    let a = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let b = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let locks = Arc::new(RepoLocks::default());
    // Each body waits until the other has started. Serialized, the first
    // would time out; concurrent, both see each other.
    let (tx_a, rx_a) = mpsc::channel::<()>();
    let (tx_b, rx_b) = mpsc::channel::<()>();
    let run = |root: &Path, tx: mpsc::Sender<()>, rx: mpsc::Receiver<()>| {
        let locks = Arc::clone(&locks);
        let job = job(&root.to_string_lossy(), "wt");
        thread::spawn(move || {
            serialize_by_repo(&locks, &job, || {
                tx.send(()).unwrap();
                rx.recv_timeout(Duration::from_secs(10)).is_ok()
            })
        })
    };
    let ha = run(a.path(), tx_a, rx_b);
    let hb = run(b.path(), tx_b, rx_a);
    assert!(ha.join().unwrap(), "repo A waited behind repo B");
    assert!(hb.join().unwrap(), "repo B waited behind repo A");
}

#[test]
fn reservation_rows_take_no_lock() {
    let locks = RepoLocks::default();
    let mut j = job("", "reserved");
    j.row.reservation = true;
    serialize_by_repo(&locks, &j, || {
        assert!(locks.map.lock().unwrap().is_empty());
    });
}

#[test]
fn shared_cli_and_daemon_executor_spares_container_bound_source() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("precious"), "work").unwrap();
    let job = job(&tmp.path().to_string_lossy(), &source.to_string_lossy());
    let result = run_reclaim_with_guard(
        &job,
        &|_| panic!("a bound source must be spared before Git or network probes"),
        &|| ProcessCwdSnapshot {
            available: true,
            cwds: Vec::new(),
        },
        &|_, _| Some("pinned: container bind references source tree"),
    );
    assert_eq!(
        result,
        ReclaimOutcome::Spared("pinned: container bind references source tree".into())
    );
    assert!(source.join("precious").exists());
}

/// Issue #2101: a `clud gc keep` that lands after dispatch still wins: the
/// pool spares the job before gathering any facts.
#[test]
fn a_kept_path_is_spared_on_the_pool_thread() {
    let home = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let wt_root = home.path().join("worktrees");
    let path = wt_root.join("repo-a").to_string_lossy().to_string();
    let keep = crate::gc::keep_list::keep_path_for(&wt_root);
    crate::gc::keep_list::update(&keep, &path, true).unwrap();
    let mut kept = job("/nonexistent-repo", &path);
    kept.wt_root = Some(wt_root);
    let outcome = run_reclaim_with_guard(
        &kept,
        &|_| panic!("a kept job must not look up PRs"),
        &|| panic!("a kept job must not sample processes"),
        &|_, _| None,
    );
    assert_eq!(
        outcome,
        ReclaimOutcome::Spared(crate::gc::keep_list::KEPT_REASON.to_string())
    );
}

//! Issue #2103 item 3: a resumable repo-worktree scan.
//!
//! The probe used to walk every known repo in one pass and replace the whole
//! snapshot, so a daemon killed mid-scan started over, and a repo whose `git`
//! or `gh` call timed out silently vanished from `gc list`. The queue fixes
//! both. It is a durable, ordered list of repo roots in
//! `<state>/gc/scan-queue.json`, rewritten atomically after every item: a
//! scanned item moves to the back, so a restart resumes with the repos not
//! yet scanned. A pass stops at its time budget and the rest wait for the
//! next tick, keeping their previous rows. A repo whose probe hit a command
//! timeout keeps its previous rows, relabelled `pinned` /
//! `scan timed out ×N`, so it is never reclaimed on stale evidence; the
//! count resets on its next clean scan.
//!
//! [`run_pass`] is pure over an injected probe and clock; the probe thread
//! supplies the real `git`/`gh` probe.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::repo_worktree::{RepoWorktreeState, RepoWorktreeVerdict};
use super::repo_worktree_probe::RepoWorktreeRow;

/// Wall-clock budget for one pass. The tick is hourly by default.
pub(super) const PASS_BUDGET_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Item {
    pub(super) repo_root: String,
    #[serde(default)]
    pub(super) timeouts: u32,
    /// Row paths this root produced on its last clean scan, so its previous
    /// rows can be carried forward (a row's `repo_root` is the main checkout,
    /// which need not be the queued root).
    #[serde(default)]
    pub(super) paths: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ScanQueue {
    #[serde(default)]
    pub(super) items: Vec<Item>,
}

impl ScanQueue {
    /// Keep known roots in their queue order, drop vanished ones, and append
    /// new ones at the front so a newly seen repo is scanned first.
    pub(super) fn sync(&mut self, roots: &[String]) {
        let wanted: BTreeSet<&str> = roots.iter().map(String::as_str).collect();
        self.items
            .retain(|item| wanted.contains(item.repo_root.as_str()));
        let known: BTreeSet<String> = self.items.iter().map(|i| i.repo_root.clone()).collect();
        let mut fresh: Vec<Item> = Vec::new();
        for root in roots {
            if !known.contains(root) && !fresh.iter().any(|i| &i.repo_root == root) {
                fresh.push(Item {
                    repo_root: root.clone(),
                    timeouts: 0,
                    paths: Vec::new(),
                });
            }
        }
        fresh.append(&mut self.items);
        self.items = fresh;
    }
}

/// `<state>/gc/scan-queue.json`.
pub(super) fn queue_path(state: &Path) -> PathBuf {
    state.join("gc").join("scan-queue.json")
}

/// Missing or corrupt reads as empty: the next sync rebuilds it.
pub(super) fn load(path: &Path) -> ScanQueue {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic write (temp + rename), so a kill never leaves a torn queue.
pub(super) fn save(path: &Path, queue: &ScanQueue) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string(queue).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// What probing one root produced.
pub(super) struct Probed {
    pub(super) rows: Vec<RepoWorktreeRow>,
    /// A `git`/`gh` call was killed at its deadline.
    pub(super) timed_out: bool,
}

/// The reason a timed-out repo's rows carry.
pub(super) fn timed_out_reason(count: u32) -> String {
    format!("scan timed out ×{count}")
}

/// One pass: probe queued roots front to back until `out_of_time` says
/// stop, persisting the queue after every item through `persist`. Returns
/// the merged rows: fresh rows for clean scans, relabelled previous rows for
/// timed-out ones, and previous rows for every root this pass did not reach.
pub(super) fn run_pass(
    queue: &mut ScanQueue,
    previous: &[RepoWorktreeRow],
    probe: &mut dyn FnMut(&str) -> Probed,
    out_of_time: &mut dyn FnMut() -> bool,
    persist: &mut dyn FnMut(&ScanQueue),
) -> Vec<RepoWorktreeRow> {
    let mut out = Vec::new();
    let mut scanned = 0usize;
    let total = queue.items.len();
    while scanned < total && !out_of_time() {
        let mut item = queue.items.remove(0);
        let probed = probe(&item.repo_root);
        if probed.timed_out {
            item.timeouts = item.timeouts.saturating_add(1);
            let reason = timed_out_reason(item.timeouts);
            out.extend(
                previous_rows(previous, &item.paths).map(|row| RepoWorktreeRow {
                    verdict: RepoWorktreeVerdict {
                        state: RepoWorktreeState::Pinned,
                        reason: reason.clone(),
                    },
                    ..row.clone()
                }),
            );
        } else {
            item.timeouts = 0;
            item.paths = probed.rows.iter().map(|r| r.path.clone()).collect();
            out.extend(probed.rows);
        }
        queue.items.push(item);
        scanned += 1;
        persist(queue);
    }
    // Not reached this pass: they sit at the front now, keep their rows.
    for item in &queue.items[..total - scanned] {
        out.extend(previous_rows(previous, &item.paths).cloned());
    }
    out
}

fn previous_rows<'a>(
    previous: &'a [RepoWorktreeRow],
    paths: &'a [String],
) -> impl Iterator<Item = &'a RepoWorktreeRow> + 'a {
    previous
        .iter()
        .filter(move |row| !row.reservation && paths.contains(&row.path))
}

/// The probe thread's entry point. With no queue file (unit tests) it falls
/// back to one full, unqueued pass.
pub(super) fn probe_all(
    queue_file: Option<&Path>,
    previous: &[RepoWorktreeRow],
    repo_roots: &[String],
    live_cwds: &[PathBuf],
    procs: &super::repo_worktree_reclaim::ProcessCwdSnapshot,
    wt_root: Option<&Path>,
) -> Vec<RepoWorktreeRow> {
    use super::repo_worktree_probe as probe;
    let Some(queue_file) = queue_file else {
        return probe::probe_repos(repo_roots, live_cwds, procs, wt_root);
    };
    let mut roots = repo_roots.to_vec();
    if let Some(root) = wt_root {
        roots.extend(probe::worktree_root_children(root));
    }
    // #2103: submodules of known repos, so their linked worktrees are listed.
    let submodules: Vec<String> = roots
        .iter()
        .flat_map(|root| super::shim_hints::submodule_checkouts(Path::new(root)))
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !roots.contains(p))
        .collect();
    for sub in submodules {
        if !roots.contains(&sub) {
            roots.push(sub);
        }
    }
    let mut queue = load(queue_file);
    queue.sync(&roots);
    let mut seen_roots = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(PASS_BUDGET_SECS);
    let rows = run_pass(
        &mut queue,
        previous,
        &mut |root| {
            let before = super::extern_repo::probe_timeouts_on_this_thread();
            let rows =
                probe::probe_one_root(Path::new(root), live_cwds, procs, wt_root, &mut seen_roots);
            Probed {
                rows,
                timed_out: super::extern_repo::probe_timeouts_on_this_thread() > before,
            }
        },
        &mut || std::time::Instant::now() >= deadline,
        &mut |q| {
            if let Err(err) = save(queue_file, q) {
                eprintln!("[clud] gc: scan queue not saved: {err}");
            }
        },
    );
    probe::with_reservations(probe::dedupe_rows(rows), live_cwds, procs, wt_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(repo: &str, path: &str, state: RepoWorktreeState) -> RepoWorktreeRow {
        RepoWorktreeRow {
            path: path.to_string(),
            repo_root: repo.to_string(),
            branch: None,
            tip: None,
            mtime_unix: 0,
            verdict: RepoWorktreeVerdict {
                state,
                reason: "fresh".to_string(),
            },
            reservation: false,
        }
    }

    fn queue(roots: &[&str]) -> ScanQueue {
        let mut q = ScanQueue::default();
        q.sync(&roots.iter().map(|r| r.to_string()).collect::<Vec<_>>());
        q
    }

    #[test]
    fn a_killed_pass_resumes_with_the_repos_it_did_not_reach() {
        let mut q = queue(&["/a", "/b", "/c"]);
        let mut saved = Vec::new();
        let mut budget = 1u32;
        run_pass(
            &mut q,
            &[],
            &mut |root| Probed {
                rows: vec![row(
                    root,
                    &format!("{root}/wt"),
                    RepoWorktreeState::Reclaimable,
                )],
                timed_out: false,
            },
            &mut || {
                let done = budget == 0;
                budget = budget.saturating_sub(1);
                done
            },
            &mut |q| saved.push(q.clone()),
        );
        assert_eq!(saved.len(), 1, "persisted after the one scanned item");
        let order: Vec<_> = saved[0]
            .items
            .iter()
            .map(|i| i.repo_root.as_str())
            .collect();
        assert_eq!(order, ["/b", "/c", "/a"], "a restart starts at /b");
    }

    #[test]
    fn a_timed_out_repo_keeps_its_rows_pinned_and_counts_up() {
        let mut q = queue(&["/a"]);
        q.items[0].paths = vec!["/a/wt".into()];
        let previous = [row("/main-a", "/a/wt", RepoWorktreeState::Reclaimable)];
        let mut probe = |_: &str| Probed {
            rows: Vec::new(),
            timed_out: true,
        };
        for expected in 1..=2 {
            let rows = run_pass(&mut q, &previous, &mut probe, &mut || false, &mut |_| {});
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].verdict.state, RepoWorktreeState::Pinned);
            assert_eq!(rows[0].verdict.reason, timed_out_reason(expected));
        }
        let rows = run_pass(
            &mut q,
            &previous,
            &mut |root| Probed {
                rows: vec![row(root, "/a/wt", RepoWorktreeState::Reclaimable)],
                timed_out: false,
            },
            &mut || false,
            &mut |_| {},
        );
        assert_eq!(rows[0].verdict.state, RepoWorktreeState::Reclaimable);
        assert_eq!(q.items[0].timeouts, 0, "a clean scan resets the count");
    }

    #[test]
    fn unreached_repos_keep_their_previous_rows_and_new_repos_go_first() {
        let mut q = queue(&["/a", "/b"]);
        q.items[1].paths = vec!["/b/wt".into()];
        let previous = [row("/main-b", "/b/wt", RepoWorktreeState::Pinned)];
        let rows = run_pass(
            &mut q,
            &previous,
            &mut |_| panic!("no time to probe"),
            &mut || true,
            &mut |_| {},
        );
        assert_eq!(rows, previous.to_vec());
        q.sync(&["/a".into(), "/b".into(), "/new".into()]);
        assert_eq!(q.items[0].repo_root, "/new");
        q.sync(&["/new".into()]);
        assert_eq!(q.items.len(), 1, "vanished roots are dropped");
    }

    #[test]
    fn queue_round_trips_through_disk() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = queue_path(dir.path());
        let q = queue(&["/a", "/b"]);
        save(&path, &q).unwrap();
        assert_eq!(load(&path), q);
        std::fs::write(&path, "{bad").unwrap();
        assert_eq!(load(&path), ScanQueue::default());
    }
}

use super::*;

/// `spare_reasons` supplies the extern-repo verdicts this function consumes.
/// It never computes one: issue #946 moved that to `spawn_extern_probe`, on
/// its own thread, because the probe spawns up to three `git` processes per
/// checkout and this runs on the thread that owns redb.
pub(super) fn partition_purgeable(
    candidates: Vec<TrackedEntry>,
    live_cwds: Vec<PathBuf>,
    spare_reasons: &mut SpareReasons,
) -> (Vec<TrackedEntry>, usize) {
    let live_locks = collect_live_lock_paths();
    let live_cwds = canonicalize_live_cwds(live_cwds);
    let mut purgeable = Vec::new();
    let mut skipped = 0usize;
    for candidate in candidates {
        if entry_is_live(&candidate, &live_locks, &live_cwds) {
            // Not recorded: the next probe snapshot replaces the cache
            // wholesale, so a marker written here would not survive. `List`
            // derives live-cwd containment itself instead — see
            // `entry_path_contains_live_cwd_path`.
            skipped += 1;
            continue;
        }
        if entry_is_cacheable(&candidate) {
            // Issue #946: the verdict is *looked up*, never computed here.
            // Computing it means up to three `git` spawns on the thread that
            // owns redb; the probe now runs on its own thread and publishes
            // results via `RegistryMsg::ExternVerdicts`.
            //
            // A row with no verdict yet — a fresh daemon, or one added since
            // the last probe — is spared for this pass. Falling back to the
            // mtime-only verdict would delete a checkout no probe has ever
            // inspected, which is precisely the data-loss the extern-repo
            // guard exists to prevent.
            let purge = spare_reasons
                .get(&candidate.path)
                .map(|cached| cached.purge())
                .unwrap_or(false);
            if !purge {
                skipped += 1;
                continue;
            }
        }
        purgeable.push(candidate);
    }
    (purgeable, skipped)
}

pub(super) fn dry_run_purge_entries(
    candidates: Vec<TrackedEntry>,
    live_cwds: Vec<PathBuf>,
    spare_reasons: &mut SpareReasons,
) -> GcReply {
    let (purgeable, skipped) = partition_purgeable(candidates, live_cwds, spare_reasons);
    GcReply::PurgeOk {
        removed: purgeable.len(),
        skipped,
    }
}

/// Issue #268: fan out each purgeable entry to the purge pool and
/// return immediately with `PurgeStarted`. The pool threads each run
/// `remove_entry_filesystem` in parallel and report completion via
/// `RegistryMsg::PurgeCompletion`, which the registry worker applies
/// to redb asynchronously. Bias: delete first, update index after —
/// the redb writer never blocks on filesystem work.
pub(super) fn dispatch_purge_entries(
    pool_tx: &mpsc::Sender<PurgeJob>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    candidates: Vec<TrackedEntry>,
    live_cwds: Vec<PathBuf>,
    spare_reasons: &mut SpareReasons,
) -> GcReply {
    let (purgeable, skipped) = partition_purgeable(candidates, live_cwds, spare_reasons);
    let mut dispatched = 0usize;
    for entry in purgeable {
        let job = PurgeJob {
            work: PurgeWork::Tracked(entry),
            completion_tx: completion_tx.clone(),
        };
        if pool_tx.send(job).is_err() {
            // Pool hung up — likely daemon teardown. Report what we
            // managed to enqueue plus an explanatory error so the
            // caller doesn't silently think the rest of the purge is
            // still in flight.
            return GcReply::Error {
                message: format!(
                    "gc purge pool stopped after dispatching {dispatched} of {} entr{}",
                    dispatched + 1,
                    if dispatched == 0 { "y" } else { "ies" }
                ),
            };
        }
        dispatched += 1;
    }
    GcReply::PurgeStarted {
        dispatched,
        skipped,
    }
}

pub(super) fn canonicalize_live_cwds(live_cwds: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = live_cwds
        .into_iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
    out.sort();
    out.dedup();
    out
}

pub(super) fn entry_is_live(
    entry: &TrackedEntry,
    live_locks: &HashSet<String>,
    live_cwds: &[PathBuf],
) -> bool {
    if entry.kind == "trash" {
        return false;
    }
    if entry.kind == "worktree" && live_locks.contains(&entry.path) {
        return true;
    }
    entry_path_contains_live_cwd(entry, live_cwds)
}

pub(super) fn entry_path_contains_live_cwd(entry: &TrackedEntry, live_cwds: &[PathBuf]) -> bool {
    entry_path_contains_live_cwd_path(&entry.path, live_cwds)
}

/// Path-only form, for callers that hold a registry row rather than a
/// `TrackedEntry`. Pure path arithmetic plus one `canonicalize` — no
/// subprocess, so it is safe on the registry worker thread.
pub(super) fn entry_path_contains_live_cwd_path(path: &str, live_cwds: &[PathBuf]) -> bool {
    let Ok(entry_path) = std::fs::canonicalize(path) else {
        return false;
    };
    live_cwds
        .iter()
        .any(|cwd| cwd == &entry_path || cwd.starts_with(&entry_path))
}

/// Whether this kind produces a verdict worth caching for `gc list`.
/// Cheap by design — the same question as [`entry_purge_verdict`] but
/// without computing the answer, for callers that only need to know
/// whether a cache slot applies.
pub(super) fn entry_is_cacheable(entry: &TrackedEntry) -> bool {
    matches!(
        entry.kind.as_str(),
        EXTERN_REPO_KIND | SIBLING_CLONE_KIND | WORKTREE_KIND
    )
}

pub(super) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

//! Issue #896: give every `clud gc list` row a *state* and a *reason*.
//!
//! The extern-repo git-dirty guard made GC refuse to reclaim a checkout
//! holding local work. That is correct, but it means such a row sits in the
//! registry forever looking exactly like garbage GC keeps failing to
//! collect. Surfacing why a row is retained is what makes the retained set
//! legible.
//!
//! **`gc list` must never run the extern-repo git probe.** It is a hot
//! client op — the launch path calls into the same registry worker — and
//! that probe costs up to three subprocesses per checkout. So the verdict
//! is *not* recomputed here. Since #946 the probe runs on its own thread
//! (`spawn_extern_probe`) and publishes a complete snapshot back to the
//! worker as `RegistryMsg::ExternVerdicts`; this module holds that snapshot.
//!
//! That does not make `List` subprocess-free, and it would be dishonest to
//! imply so: it still calls `collect_live_lock_paths`, which shells out to
//! `git worktree list --porcelain` via the *unbounded* `worktrees::run_git`.
//! That is a separate, pre-existing path from the probe #946 moved.

use std::collections::{HashMap, HashSet};

use super::extern_repo::{PurgeClass, PurgeDecision};
use super::repo_worktree_probe::RepoWorktreeRow;

/// A purge verdict recorded by the tick, for later display by `gc list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CachedDecision {
    pub(super) class: PurgeClass,
    pub(super) reason: &'static str,
    /// When the tick computed this. Surfaced in `--json` so a consumer can
    /// see how stale the verdict is (worst case one tick, default 1 h).
    pub(super) evaluated_unix: i64,
}

impl CachedDecision {
    /// Whether this verdict authorizes a purge. Derived from the class so
    /// there is one source of truth: only `Reclaimable` ever purges, and
    /// every spare flavour (`Spared`, `Pinned`, `Dangling`) does not.
    pub(super) fn purge(&self) -> bool {
        matches!(self.class, PurgeClass::Reclaimable)
    }
}

/// Path → last recorded verdict. Owned by the registry worker loop, which
/// is also the thread that runs the periodic tick, so no lock is needed.
#[derive(Debug, Default)]
pub(super) struct SpareReasons {
    entries: HashMap<String, CachedDecision>,
    /// Issue #946: whether an off-worker probe is currently recomputing the
    /// snapshot. Owned here rather than in a `static` so it is per-worker
    /// (a test binary runs many) and cannot be left set by an unrelated
    /// thread — the same reason the cache itself is loop-owned.
    probe_in_flight: bool,
    /// Issue #1591: the latest repo-worktree snapshot, computed off-worker
    /// by `spawn_repo_worktree_probe`, plus when it was installed. `gc list`
    /// displays it; the next tick selects reclaims from it (#1603), each
    /// re-verified on the purge pool before anything is deleted.
    repo_worktrees: Vec<RepoWorktreeRow>,
    repo_worktrees_evaluated_unix: Option<i64>,
    repo_probe_in_flight: bool,
    repo_snapshot_needs_refresh: bool,
    /// Issue #1603: paths with a reclaim queued or running on the pool.
    repo_reclaims_in_flight: HashSet<String>,
    /// Issue #2101: rows the purge pool's delete-time re-check spared, with
    /// its reason. Kept apart from `entries`, which each extern snapshot
    /// replaces wholesale; cleared when the row is finally removed.
    delete_spares: HashMap<String, &'static str>,
}

impl SpareReasons {
    /// Issue #1591: install a complete repo-worktree snapshot, releasing the
    /// probe slot. Wholesale replacement, for the same reason as
    /// `replace_extern`.
    pub(super) fn replace_repo_worktrees(&mut self, rows: Vec<RepoWorktreeRow>, now: i64) {
        self.repo_probe_in_flight = false;
        self.repo_snapshot_needs_refresh = false;
        self.repo_worktrees = rows;
        self.repo_worktrees_evaluated_unix = Some(now);
    }

    pub(super) fn repo_worktrees(&self) -> (&[RepoWorktreeRow], Option<i64>) {
        (&self.repo_worktrees, self.repo_worktrees_evaluated_unix)
    }

    /// Issue #1603: a reclaim of `path` was dispatched to the pool.
    pub(super) fn begin_repo_reclaim(&mut self, path: String) {
        self.repo_snapshot_needs_refresh = true;
        self.repo_reclaims_in_flight.insert(path);
    }

    pub(super) fn repo_reclaim_in_flight(&self, path: &str) -> bool {
        self.repo_reclaims_in_flight.contains(path)
    }

    pub(super) fn repo_reclaim_finished(&mut self, path: &str) {
        self.repo_reclaims_in_flight.remove(path);
    }

    pub(super) fn repo_reclaim_spared(&mut self, path: &str, reason: String) {
        if let Some(row) = self.repo_worktrees.iter_mut().find(|row| row.path == path) {
            row.verdict.state = super::repo_worktree::RepoWorktreeState::Pinned;
            row.verdict.reason = reason;
        }
    }

    /// Drop a removed worktree's row so `gc list` stops showing it before the
    /// next probe replaces the snapshot.
    pub(super) fn forget_repo_worktree(&mut self, path: &str) {
        self.repo_worktrees.retain(|r| r.path != path);
    }

    pub(super) fn repo_snapshot_needs_refresh(&self) -> bool {
        self.repo_snapshot_needs_refresh
    }

    pub(super) fn repo_probe_in_flight(&self) -> bool {
        self.repo_probe_in_flight
    }

    /// Probes and reclaims cannot overlap: a Git probe holds its cwd on Windows.
    pub(super) fn begin_repo_probe(&mut self) -> bool {
        if !self.repo_reclaims_in_flight.is_empty() {
            return false;
        }
        !std::mem::replace(&mut self.repo_probe_in_flight, true)
    }

    pub(super) fn repo_probe_finished(&mut self) {
        self.repo_probe_in_flight = false;
    }

    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn record(&mut self, path: String, decision: PurgeDecision, evaluated_unix: i64) {
        self.entries.insert(
            path,
            CachedDecision {
                class: decision.class,
                reason: decision.reason,
                evaluated_unix,
            },
        );
    }

    /// Issue #946: install a complete snapshot of extern-repo verdicts
    /// computed off-worker. Wholesale replacement rather than a merge —
    /// the probe was handed every extern-repo row, so anything absent from
    /// the reply no longer exists and its verdict must not linger.
    pub(super) fn replace_extern(
        &mut self,
        verdicts: Vec<(String, PurgeDecision)>,
        evaluated_unix: i64,
    ) {
        self.probe_in_flight = false;
        self.entries.clear();
        for (path, decision) in verdicts {
            self.record(path, decision, evaluated_unix);
        }
    }

    pub(super) fn get(&self, path: &str) -> Option<&CachedDecision> {
        self.entries.get(path)
    }

    /// Issue #2101: the delete-time re-check spared `path`.
    pub(super) fn record_delete_spare(&mut self, path: String, reason: &'static str) {
        self.delete_spares.insert(path, reason);
    }

    /// Issue #2101: `path` was removed; its delete-time spare no longer applies.
    pub(super) fn clear_delete_spare(&mut self, path: &str) {
        self.delete_spares.remove(path);
    }

    pub(super) fn delete_spare(&self, path: &str) -> Option<&'static str> {
        self.delete_spares.get(path).copied()
    }

    /// Claim the right to start a probe. `false` means one is already
    /// running; a second would duplicate every `git` spawn for no benefit.
    pub(super) fn begin_probe(&mut self) -> bool {
        if self.probe_in_flight {
            return false;
        }
        self.probe_in_flight = true;
        true
    }

    pub(super) fn probe_finished(&mut self) {
        self.probe_in_flight = false;
    }

    pub(super) fn probe_in_flight(&self) -> bool {
        self.probe_in_flight
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Displayed state of a registry row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryState {
    /// Normal: nothing is holding it, GC will take it on a future tick.
    Reclaimable,
    /// Deliberately retained. Healthy, but it will not be auto-reclaimed.
    Pinned,
    /// The on-disk path is gone — a registry row pointing at nothing.
    Dangling,
}

impl EntryState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Reclaimable => "reclaimable",
            Self::Pinned => "pinned",
            Self::Dangling => "dangling",
        }
    }
}

/// Pure state derivation, so the precedence between "gone", "live" and
/// "pinned by the tick" is unit-testable without a daemon, a filesystem or
/// a git checkout.
///
/// Precedence, most severe first:
///
/// | condition                     | state       | reason                    |
/// |-------------------------------|-------------|---------------------------|
/// | path missing                  | Dangling    | `dangling: path missing`  |
/// | live session holds it         | Pinned      | `spared: live session`    |
/// | spared at delete time (#2101) | Pinned      | the re-check's reason     |
/// | tick class `Dangling`         | Dangling    | the tick's own reason     |
/// | tick class `Pinned`           | Pinned      | the tick's own reason     |
/// | tick class `Spared`           | Reclaimable | the tick's own reason     |
/// | tick class `Reclaimable`      | Reclaimable | none                      |
/// | never evaluated               | Reclaimable | none                      |
///
/// Note the `Spared` row: a checkout that is merely too recently touched
/// *will* be reclaimed once it ages in, so it stays an ordinary reclaimable
/// row. Painting it yellow would make most of a working tree yellow and
/// destroy exactly the signal #896 is trying to add — but its reason is
/// still carried, so `--json` can explain the delay.
///
/// A delete-time spare (#2101) is a fresher verdict than the tick's: the purge
/// pool re-checked the row immediately before deleting it and kept it, so a
/// `reclaimable` label would be a promise GC keeps breaking.
pub(super) fn derive_state(
    path_exists: bool,
    live_locked: bool,
    delete_spare: Option<&'static str>,
    cached: Option<&CachedDecision>,
) -> (EntryState, Option<&'static str>) {
    if !path_exists {
        return (EntryState::Dangling, Some("dangling: path missing"));
    }
    if live_locked {
        return (EntryState::Pinned, Some("spared: live session"));
    }
    if let Some(reason) = delete_spare {
        return (EntryState::Pinned, Some(reason));
    }
    match cached {
        Some(d) => match d.class {
            PurgeClass::Dangling => (EntryState::Dangling, Some(d.reason)),
            PurgeClass::Pinned => (EntryState::Pinned, Some(d.reason)),
            PurgeClass::Spared => (EntryState::Reclaimable, Some(d.reason)),
            PurgeClass::Reclaimable => (EntryState::Reclaimable, None),
        },
        // Never evaluated (fresh daemon, first tick pending) — a plain row
        // rather than a fabricated verdict.
        None => (EntryState::Reclaimable, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(class: PurgeClass, reason: &'static str) -> CachedDecision {
        CachedDecision {
            class,
            reason,
            evaluated_unix: 1_000,
        }
    }

    /// Assert state **and** reason, not just state — a pinned row that
    /// cannot say why is the exact problem #896 exists to fix.
    #[test]
    fn state_matrix_covers_every_precedence_step() {
        // Missing path outranks everything, including a live lock.
        assert_eq!(
            derive_state(
                false,
                true,
                None,
                Some(&cached(PurgeClass::Pinned, "pinned: whatever"))
            ),
            (EntryState::Dangling, Some("dangling: path missing"))
        );
        // A live session outranks a cached verdict.
        assert_eq!(
            derive_state(
                true,
                true,
                None,
                Some(&cached(
                    PurgeClass::Reclaimable,
                    "reclaimable: clean and pushed"
                ))
            ),
            (EntryState::Pinned, Some("spared: live session"))
        );
        // A recorded pin surfaces the tick's own reason verbatim.
        assert_eq!(
            derive_state(
                true,
                false,
                None,
                Some(&cached(
                    PurgeClass::Pinned,
                    "pinned: uncommitted or unpushed work"
                ))
            ),
            (
                EntryState::Pinned,
                Some("pinned: uncommitted or unpushed work")
            )
        );
        // A cached `Dangling` verdict stays dangling.
        assert_eq!(
            derive_state(
                true,
                false,
                None,
                Some(&cached(PurgeClass::Dangling, "dangling: path missing"))
            ),
            (EntryState::Dangling, Some("dangling: path missing"))
        );
        // Evaluated and reclaimable → plain row, no reason, no color.
        assert_eq!(
            derive_state(
                true,
                false,
                None,
                Some(&cached(
                    PurgeClass::Reclaimable,
                    "reclaimable: clean and pushed"
                ))
            ),
            (EntryState::Reclaimable, None)
        );
        // Never evaluated (fresh daemon) → also plain, not a fake verdict.
        assert_eq!(
            derive_state(true, false, None, None),
            (EntryState::Reclaimable, None)
        );
    }

    /// A checkout that is merely too recently touched **will** be reclaimed
    /// once it ages in, so it must not be painted as pinned. With the 24 h
    /// default that is most of a working tree; collapsing it into `pinned`
    /// turns the list into a yellow wall and destroys the signal #896 adds.
    /// The reason still rides along so `--json` can explain the delay.
    #[test]
    fn a_merely_too_recent_checkout_stays_reclaimable() {
        let (state, reason) = derive_state(
            true,
            false,
            None,
            Some(&cached(PurgeClass::Spared, "spared: recently active")),
        );
        assert_eq!(state, EntryState::Reclaimable);
        assert_eq!(reason, Some("spared: recently active"));
    }

    #[test]
    /// Issue #946: the probe is handed *every* extern-repo row, so its reply
    /// is the complete truth. Installing it replaces the cache wholesale —
    /// a row absent from the new snapshot no longer exists, and leaving its
    /// verdict behind would have `gc list` explain a row that is gone. This
    /// is also what bounds the cache: it can never exceed the row count.
    fn snapshot_replaces_the_cache_wholesale() {
        let mut cache = SpareReasons::new();
        let pinned = PurgeDecision {
            purge: false,
            reason: "pinned: uncommitted or unpushed work",
            class: PurgeClass::Pinned,
        };
        let clean = PurgeDecision {
            purge: true,
            reason: "reclaimable: clean and pushed",
            class: PurgeClass::Reclaimable,
        };

        cache.replace_extern(
            vec![("/a".to_string(), pinned), ("/b".to_string(), pinned)],
            100,
        );
        assert!(cache.get("/a").is_some());
        assert!(cache.get("/b").is_some());

        // Next snapshot: /b is gone from the registry, /a is now clean.
        cache.replace_extern(vec![("/a".to_string(), clean)], 200);
        assert!(cache.get("/b").is_none(), "vanished row must not linger");
        let a = cache.get("/a").expect("still tracked");
        assert!(a.purge(), "the newer verdict must win");
        assert_eq!(a.evaluated_unix, 200);
    }

    /// Applying a snapshot also releases the probe slot, so the next tick
    /// can start a fresh probe.
    #[test]
    fn applying_a_snapshot_releases_the_probe_slot() {
        let mut cache = SpareReasons::new();
        assert!(cache.begin_probe());
        cache.replace_extern(Vec::new(), 1);
        assert!(
            cache.begin_probe(),
            "a delivered snapshot must free the slot"
        );
    }

    /// Issue #2101: a row the purge pool re-checked and kept is pinned with
    /// that reason, not advertised as reclaimable; a live lock still wins.
    #[test]
    fn a_row_spared_at_delete_time_is_pinned_with_its_reason() {
        let reason = "pinned: uncommitted or unpushed work";
        assert_eq!(
            derive_state(true, false, Some(reason), None),
            (EntryState::Pinned, Some(reason))
        );
        assert_eq!(
            derive_state(
                true,
                false,
                Some(reason),
                Some(&cached(
                    PurgeClass::Reclaimable,
                    "reclaimable: clean and pushed"
                ))
            ),
            (EntryState::Pinned, Some(reason))
        );
        assert_eq!(
            derive_state(true, true, Some(reason), None),
            (EntryState::Pinned, Some("spared: live session"))
        );
        let mut cache = SpareReasons::new();
        cache.record_delete_spare("/w".to_string(), reason);
        cache.replace_extern(Vec::new(), 1);
        assert_eq!(cache.delete_spare("/w"), Some(reason), "snapshots keep it");
        cache.clear_delete_spare("/w");
        assert_eq!(cache.delete_spare("/w"), None);
    }

    #[test]
    fn state_names_are_the_documented_json_values() {
        assert_eq!(EntryState::Reclaimable.as_str(), "reclaimable");
        assert_eq!(EntryState::Pinned.as_str(), "pinned");
        assert_eq!(EntryState::Dangling.as_str(), "dangling");
    }
}

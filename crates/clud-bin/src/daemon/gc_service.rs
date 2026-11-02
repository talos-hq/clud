//! GC service running inside the always-on session daemon.
//!
//! Owns the redb registry exclusively (issue #135). All `clud gc *`
//! IPC ops on the session daemon's TCP listener get routed to a single
//! registry worker thread; the worker is the sole reader/writer of
//! `~/.clud/data.redb`. This module replaces the standalone `gc_daemon`
//! process that shipped in Phase 1 of #135 — there is now exactly one
//! background daemon per user (see [docs/architecture/gc-and-registry.md]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::gc::{
    reconcile_registered_dir, reconcile_repo_root, InsertInput, Registry, TrackedEntry,
    EXTERN_REPO_KIND, SIBLING_CLONE_KIND, WORKTREE_KIND,
};
use crate::worktrees;

use super::activity::DaemonActivity;
use super::types::{GcOp, GcReply, GcWatchRoot, ListRow};

mod act_containers;
mod extern_repo;
mod filesystem;
mod fs_backstop;
mod legacy_worktree_root;
mod list_state;
mod repo_worktree;
pub(crate) mod repo_worktree_cli;
mod repo_worktree_probe;
mod repo_worktree_reclaim;
mod repo_worktree_reclaim_exec;
mod scan_queue;
mod shim_hints;
mod source_guard;
mod worktree_schedule_exec;

use super::watch_service as gc_watch_service;

use extern_repo::{extern_repo_purge_verdict, extern_repo_stale_after, PurgeDecision};
use filesystem::{
    collect_live_lock_paths, reap_trash_entries, reap_unregistered_rm_trash,
    remove_entry_and_delete_row, remove_entry_filesystem,
};
use list_state::{derive_state, EntryState, SpareReasons};
use repo_worktree_probe::{RepoWorktreeRow, REPO_WORKTREE_KIND};
use repo_worktree_reclaim::{
    reclaim_mode_from_raw, reclaim_selection, ReclaimMode, ReclaimSelection,
};
use repo_worktree_reclaim_exec::{ReclaimJob, ReclaimOutcome};

/// How long a connection thread waits for the registry worker before
/// giving up. Since #268 the worker no longer runs `remove_dir_all`
/// inline — bulk purges fan out to the purge pool and reply
/// `PurgeStarted` within milliseconds — so this timeout protects only
/// against a genuinely wedged worker thread.
pub(super) const WORKER_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

const ENV_GC_TICK_SECS: &str = "CLUD_GC_TICK_SECS";
const ENV_GC_EXTERN_REPO_MAX_AGE_SECS: &str = "CLUD_GC_EXTERN_REPO_MAX_AGE_SECS";
const ENV_GC_WARN_FREE_GB: &str = "CLUD_GC_WARN_FREE_GB";
const ENV_GC_AUTO_PURGE_FREE_GB: &str = "CLUD_GC_AUTO_PURGE_FREE_GB";
const ENV_GC_MIN_AGE_HOURS: &str = "CLUD_GC_MIN_AGE_HOURS";
const ENV_GC_AUTO_PURGE_ENABLED: &str = "CLUD_GC_AUTO_PURGE_ENABLED";
/// Issue #268: per-daemon cap on parallel `remove_dir_all` workers
/// servicing the purge pool. Defaults to `min(num_cpus, 8)`. Setting
/// `0` falls back to the default (not synchronous-mode); to truly
/// disable parallelism the user should set it to `1`.
const ENV_GC_PURGE_CONCURRENCY: &str = "CLUD_GC_PURGE_CONCURRENCY";
const DEFAULT_GC_TICK_SECS: u64 = 3600;
const DEFAULT_EXTERN_REPO_STALE_AFTER_SECS: u64 = 24 * 60 * 60;
const DEFAULT_GC_WARN_FREE_GB: u64 = 10;
const DEFAULT_GC_AUTO_PURGE_FREE_GB: u64 = 5;
const DEFAULT_GC_MIN_AGE_HOURS: u64 = 24;
const DEFAULT_GC_AUTO_PURGE_ENABLED: bool = true;
const DEFAULT_GC_PURGE_CONCURRENCY_CAP: usize = 8;
const PERIODIC_GC_WORKTREE_STALE_AFTER: &str = "48h";
const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;

/// One client-initiated GC op handed from a connection thread (or HTTP
/// handler) to the registry worker. The worker processes it inline and
/// fires `reply_tx` exactly once with the resulting `GcReply`.
pub(super) struct GcRequestMsg {
    pub(super) op: GcOp,
    pub(super) reply_tx: mpsc::SyncSender<GcReply>,
}

/// Issue #268: the worker's mpsc carries two kinds of message. Client
/// ops (`Op`) are fast — they touch redb only — and reply on
/// `GcRequestMsg::reply_tx`. Purge completions (`PurgeCompletion`) are
/// fire-and-forget callbacks from the purge-pool threads: they tell the
/// worker that the filesystem half of one entry's deletion has finished
/// (success or failure) so the worker can run `registry.delete(id)`
/// — also fast — and log the outcome.
pub(super) enum RegistryMsg {
    Op(GcRequestMsg),
    PurgeCompletion(PurgeCompletion),
    /// A daemon watch root needs its insert-only reconciliation pass. The
    /// notification thread never touches redb directly.
    WatchRescan(Vec<GcWatchRoot>),
    /// Issue #946: extern-repo purge verdicts computed off-worker. The probe
    /// spawns up to three `git` processes per checkout, which must never run
    /// on the thread that owns redb — see `spawn_extern_probe`. The payload
    /// is a *complete* snapshot of every extern-repo row the probe was
    /// handed, so the worker replaces the cache wholesale rather than
    /// merging (which is what keeps it from growing without bound).
    ExternVerdicts(Vec<(String, PurgeDecision)>),
    /// Issue #1591: a complete snapshot of repo-worktree verdicts, computed
    /// off-worker by `spawn_repo_worktree_probe`. Feeds `gc list` and, one
    /// tick later, reclaim selection (#1603).
    RepoWorktreeVerdicts(Vec<RepoWorktreeRow>),
    /// Issue #1603: one repo-worktree reclaim finished on the purge pool
    /// (removed, spared at delete time, or failed).
    RepoWorktreeReclaimed {
        path: String,
        outcome: ReclaimOutcome,
    },
    /// Issue #2103: repos the `git`/`gh` shim saw, as `repo_root -> cwd`,
    /// read off-worker by the repo-worktree probe (`shim_hints`).
    RepoHints(std::collections::BTreeMap<String, String>),
}

/// Issue #268: result of one entry's parallel filesystem deletion,
/// delivered back to the registry worker so it can drop the
/// corresponding redb row on success or log+keep on failure.
pub(super) struct PurgeCompletion {
    pub(super) id: i64,
    pub(super) path: String,
    pub(super) kind: String,
    pub(super) result: Result<(), String>,
    /// Set when the pool re-checked the entry at delete time and decided
    /// against it. The row is kept, not dropped — see `reverify_before_delete`.
    pub(super) spared: Option<&'static str>,
}

/// One unit of work for the purge pool — the entry to remove plus the
/// channel a completion message should be sent back on. `completion_tx`
/// is a clone of the registry worker's own `RegistryMsg` sender; the
/// pool thread sends `RegistryMsg::PurgeCompletion(..)` into it, so
/// completions land in the same queue the worker is already draining.
struct PurgeJob {
    work: PurgeWork,
    completion_tx: mpsc::Sender<RegistryMsg>,
}

/// What a purge-pool thread is asked to do.
enum PurgeWork {
    /// A registry row: `remove_dir_all` after `reverify_before_delete`.
    Tracked(TrackedEntry),
    /// Issue #1603: a repo worktree, which has no registry row. Re-verified
    /// and removed through git by `repo_worktree_reclaim_exec::run_reclaim`.
    RepoWorktree(ReclaimJob),
}

type LiveCwdsProvider = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync + 'static>;

fn purge_concurrency_from_env() -> usize {
    purge_concurrency_from_raw(std::env::var(ENV_GC_PURGE_CONCURRENCY).ok().as_deref())
}

fn purge_concurrency_from_raw(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(default_purge_concurrency)
}

fn default_purge_concurrency() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cpus.clamp(1, DEFAULT_GC_PURGE_CONCURRENCY_CAP)
}

/// Spawn N long-lived pool threads, each pulling `PurgeJob` items off
/// a shared mpsc and running `remove_entry_filesystem` in parallel.
/// Returns the sender every dispatch site uses to enqueue jobs.
/// Dropping the sender signals the pool threads to exit.
fn spawn_purge_pool(concurrency: usize) -> mpsc::Sender<PurgeJob> {
    let (tx, rx) = mpsc::channel::<PurgeJob>();
    let shared_rx = Arc::new(Mutex::new(rx));
    for i in 0..concurrency {
        let rx = Arc::clone(&shared_rx);
        let _ = thread::Builder::new()
            .name(format!("clud-gc-purge-{i}"))
            .spawn(move || purge_pool_worker(rx));
    }
    tx
}

fn purge_pool_worker(rx: Arc<Mutex<mpsc::Receiver<PurgeJob>>>) {
    loop {
        let job = {
            let guard = match rx.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.recv()
        };
        let Ok(job) = job else { return };
        let entry = match job.work {
            PurgeWork::Tracked(entry) => entry,
            PurgeWork::RepoWorktree(reclaim) => {
                let outcome = repo_worktree_reclaim_exec::run_reclaim_serialized(
                    &reclaim,
                    &repo_worktree_probe::lookup_prs,
                );
                let _ = job.completion_tx.send(RegistryMsg::RepoWorktreeReclaimed {
                    path: reclaim.row.path.clone(),
                    outcome,
                });
                continue;
            }
        };
        // Issue #946: the verdict that authorized this delete was computed by
        // the probe thread, up to a tick ago. Re-check it here, immediately
        // before `remove_dir_all` and still off the registry worker, so a
        // checkout the developer came back to in the meantime is not deleted
        // on a stale "clean and pushed". Before the probe was decoupled this
        // was implicit: the verdict was computed inline microseconds earlier.
        let spared = reverify_before_delete(&entry);
        let result = if spared.is_some() {
            Ok(())
        } else {
            remove_entry_filesystem(
                &entry,
                "automatic purge; applicable eligibility and source-use guards passed",
            )
        };
        let completion = PurgeCompletion {
            id: entry.id,
            path: entry.path.clone(),
            kind: entry.kind.clone(),
            result,
            spared,
        };
        // If the worker has hung up we just drop the completion;
        // there's nothing left to apply it against.
        let _ = job
            .completion_tx
            .send(RegistryMsg::PurgeCompletion(completion));
    }
}

pub(super) fn spawn_registry_worker_for_state(
    state_dir: PathBuf,
    periodic_maintenance_enabled: bool,
    activity: Option<DaemonActivity>,
) -> std::io::Result<mpsc::Sender<RegistryMsg>> {
    let tick_cadence = periodic_maintenance_enabled
        .then(gc_tick_cadence_from_env)
        .flatten();
    let reconcile_dir = state_dir.clone();
    let evict_dir = state_dir.clone();
    let config = RegistryWorkerConfig {
        tick_cadence,
        live_cwds_provider: Arc::new(move || super::sessions::list_live_session_cwds(&state_dir)),
        session_state_dir: Some(reconcile_dir),
        activity,
    };
    match Registry::open_default() {
        Ok(registry) => spawn_registry_worker_with_config(registry, config),
        // Issue #2095: another process (typically a superseded daemon) holds
        // the redb lock. Giving up here left this daemon without GC for its
        // whole lifetime, so nothing was ever reclaimed. Keep retrying.
        Err(err) => {
            eprintln!(
                "[clud] note: gc registry unavailable ({err}); retrying every {}s",
                REGISTRY_OPEN_RETRY.as_secs()
            );
            // Issue #2142: when the holder is a stale clud daemon, ask it to
            // stop on the worker thread (never on the startup path) and
            // retry the open for a bounded time before the 30s cadence.
            let mut first = Some(evict_dir);
            let open = move || match first.take() {
                Some(dir) => super::registry_holder::open_after_eviction(
                    Registry::open_default,
                    || super::registry_holder::evict_stale_holders(&dir),
                    super::registry_holder::EVICT_WAIT,
                ),
                None => Registry::open_default(),
            };
            spawn_deferred_registry_worker(open, config, REGISTRY_OPEN_RETRY)
        }
    }
}

/// How often a daemon whose registry open failed tries again (#2095).
const REGISTRY_OPEN_RETRY: Duration = Duration::from_secs(30);

/// Issue #2095: start the worker without a registry. GC ops are answered
/// with an error until `open` succeeds, then the normal loop takes over the
/// same channel.
fn spawn_deferred_registry_worker<F>(
    open: F,
    config: RegistryWorkerConfig,
    retry: Duration,
) -> std::io::Result<mpsc::Sender<RegistryMsg>>
where
    F: FnMut() -> Result<Registry, crate::gc::GcError> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<RegistryMsg>();
    let completion_tx = tx.clone();
    thread::Builder::new()
        .name("clud-gc-registry-worker".to_string())
        .spawn(move || {
            let Some(registry) = wait_for_registry(&rx, open, retry) else {
                return;
            };
            eprintln!("[clud] note: gc registry opened after retry");
            let pool_tx = spawn_purge_pool(purge_concurrency_from_env());
            run_worker_loop(registry, rx, completion_tx, pool_tx, config);
        })?;
    Ok(tx)
}

/// Retry `open` every `retry`, answering each GC op in between with an
/// error. `None` once every sender is gone.
fn wait_for_registry<F>(
    rx: &mpsc::Receiver<RegistryMsg>,
    mut open: F,
    retry: Duration,
) -> Option<Registry>
where
    F: FnMut() -> Result<Registry, crate::gc::GcError>,
{
    loop {
        let message = match open() {
            Ok(registry) => return Some(registry),
            Err(err) => format!("gc registry unavailable ({err}); the daemon is retrying"),
        };
        let deadline = Instant::now() + retry;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(RegistryMsg::Op(request)) => {
                    let _ = request.reply_tx.send(GcReply::Error {
                        message: message.clone(),
                    });
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

/// Spawns the registry worker on a pre-constructed `Registry`. Tests use this to bind a worker to an isolated `redb`
/// file without depending on the process-global `CLUD_DATA_DB` env var.
#[cfg(test)]
pub(super) fn spawn_registry_worker_with(
    registry: Registry,
) -> std::io::Result<mpsc::Sender<RegistryMsg>> {
    spawn_registry_worker_with_live_cwds(registry, Arc::new(Vec::<PathBuf>::new))
}

#[cfg(test)]
fn spawn_registry_worker_with_live_cwds(
    registry: Registry,
    live_cwds_provider: LiveCwdsProvider,
) -> std::io::Result<mpsc::Sender<RegistryMsg>> {
    spawn_registry_worker_with_live_cwds_and_cadence(
        registry,
        live_cwds_provider,
        gc_tick_cadence_from_env(),
        None,
        None,
    )
}

#[cfg(test)]
fn spawn_registry_worker_with_live_cwds_and_cadence(
    registry: Registry,
    live_cwds_provider: LiveCwdsProvider,
    tick_cadence: Option<Duration>,
    session_state_dir: Option<PathBuf>,
    activity: Option<DaemonActivity>,
) -> std::io::Result<mpsc::Sender<RegistryMsg>> {
    spawn_registry_worker_with_config(
        registry,
        RegistryWorkerConfig {
            tick_cadence,
            live_cwds_provider,
            session_state_dir,
            activity,
        },
    )
}

fn spawn_registry_worker_with_config(
    registry: Registry,
    config: RegistryWorkerConfig,
) -> std::io::Result<mpsc::Sender<RegistryMsg>> {
    let (tx, rx) = mpsc::channel::<RegistryMsg>();
    let pool_tx = spawn_purge_pool(purge_concurrency_from_env());
    let completion_tx = tx.clone();
    thread::Builder::new()
        .name("clud-gc-registry-worker".to_string())
        .spawn(move || run_worker_loop(registry, rx, completion_tx, pool_tx, config))?;
    Ok(tx)
}

fn gc_tick_cadence_from_env() -> Option<Duration> {
    let raw = std::env::var(ENV_GC_TICK_SECS).ok();
    gc_tick_cadence_from_raw(raw.as_deref())
}

fn gc_tick_cadence_from_raw(raw: Option<&str>) -> Option<Duration> {
    let secs = raw
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_GC_TICK_SECS);
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GcDiskWatchdogConfig {
    warn_free_bytes: u64,
    auto_purge_free_bytes: u64,
    min_age: Duration,
    auto_purge_enabled: bool,
}

impl GcDiskWatchdogConfig {
    fn from_env() -> Self {
        let warn_free_gb = std::env::var(ENV_GC_WARN_FREE_GB).ok();
        let auto_purge_free_gb = std::env::var(ENV_GC_AUTO_PURGE_FREE_GB).ok();
        let min_age_hours = std::env::var(ENV_GC_MIN_AGE_HOURS).ok();
        let auto_purge_enabled = std::env::var(ENV_GC_AUTO_PURGE_ENABLED).ok();
        gc_disk_watchdog_config_from_raw(
            warn_free_gb.as_deref(),
            auto_purge_free_gb.as_deref(),
            min_age_hours.as_deref(),
            auto_purge_enabled.as_deref(),
        )
    }
}

fn gc_disk_watchdog_config_from_raw(
    warn_free_gb: Option<&str>,
    auto_purge_free_gb: Option<&str>,
    min_age_hours: Option<&str>,
    auto_purge_enabled: Option<&str>,
) -> GcDiskWatchdogConfig {
    GcDiskWatchdogConfig {
        warn_free_bytes: parse_gb_to_bytes(warn_free_gb, DEFAULT_GC_WARN_FREE_GB),
        auto_purge_free_bytes: parse_gb_to_bytes(auto_purge_free_gb, DEFAULT_GC_AUTO_PURGE_FREE_GB),
        min_age: parse_hours_to_duration(min_age_hours, DEFAULT_GC_MIN_AGE_HOURS),
        auto_purge_enabled: parse_bool_setting(auto_purge_enabled, DEFAULT_GC_AUTO_PURGE_ENABLED),
    }
}

fn parse_gb_to_bytes(raw: Option<&str>, default_gb: u64) -> u64 {
    raw.and_then(|value| {
        let gb = value.trim().parse::<f64>().ok()?;
        if !gb.is_finite() || gb < 0.0 {
            return None;
        }
        let bytes = gb * BYTES_PER_GB as f64;
        if bytes >= u64::MAX as f64 {
            Some(u64::MAX)
        } else {
            Some(bytes.round() as u64)
        }
    })
    .unwrap_or_else(|| default_gb.saturating_mul(BYTES_PER_GB))
}

fn parse_hours_to_duration(raw: Option<&str>, default_hours: u64) -> Duration {
    let hours = raw
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default_hours);
    Duration::from_secs(hours.saturating_mul(60 * 60))
}

fn parse_bool_setting(raw: Option<&str>, default: bool) -> bool {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => default,
    }
}

struct RegistryWorkerConfig {
    tick_cadence: Option<Duration>,
    live_cwds_provider: LiveCwdsProvider,
    session_state_dir: Option<PathBuf>,
    activity: Option<DaemonActivity>,
}

fn run_worker_loop(
    registry: Registry,
    rx: mpsc::Receiver<RegistryMsg>,
    completion_tx: mpsc::Sender<RegistryMsg>,
    pool_tx: mpsc::Sender<PurgeJob>,
    config: RegistryWorkerConfig,
) {
    let session_state_dir = config.session_state_dir.as_deref();
    let mut watch_service = None;
    // Issue #1485: create ~/.clud/worktrees at daemon start (no-op in tests).
    let _ = crate::gc::worktree_root::ensure_worktree_root();
    let Some(tick_cadence) = config.tick_cadence else {
        // No periodic tick in this mode, so the startup probe below is the
        // only one that ever runs. Without it extern-repo rows would have no
        // verdict at all and could never be reclaimed by any path, since
        // "no verdict" means spare.
        let mut spare_reasons = SpareReasons::new();
        prime_extern_verdicts(
            &registry,
            &completion_tx,
            config.activity.as_ref(),
            &mut spare_reasons,
            &config.live_cwds_provider,
        );
        while let Ok(msg) = rx.recv() {
            handle_registry_msg(
                &registry,
                &pool_tx,
                &completion_tx,
                &mut watch_service,
                msg,
                &config.live_cwds_provider,
                &mut spare_reasons,
            );
        }
        return;
    };

    // Owned by this loop, which is also the thread that runs the periodic
    // tick — so the cache the tick fills is the same one `GcOp::List`
    // reads, with no lock and no cross-thread staleness (issue #896).
    let mut spare_reasons = SpareReasons::new();
    // Issue #946: a row with no verdict is spared, so waiting for the first
    // tick (1 h by default) would leave extern-repo GC inert for that long
    // after every daemon start — and the daemon restarts on every clud
    // upgrade. Prime the cache immediately instead.
    prime_extern_verdicts(
        &registry,
        &completion_tx,
        config.activity.as_ref(),
        &mut spare_reasons,
        &config.live_cwds_provider,
    );
    let mut next_tick = Instant::now() + tick_cadence;
    loop {
        let timeout = next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(msg) => {
                handle_registry_msg(
                    &registry,
                    &pool_tx,
                    &completion_tx,
                    &mut watch_service,
                    msg,
                    &config.live_cwds_provider,
                    &mut spare_reasons,
                );
                if Instant::now() >= next_tick {
                    let _job_guard = config.activity.as_ref().map(DaemonActivity::start_job);
                    run_periodic_purge_tick(
                        &registry,
                        &pool_tx,
                        &completion_tx,
                        &config.live_cwds_provider,
                        session_state_dir,
                        config.activity.as_ref(),
                        &mut spare_reasons,
                    );
                    next_tick = Instant::now() + tick_cadence;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _job_guard = config.activity.as_ref().map(DaemonActivity::start_job);
                run_periodic_purge_tick(
                    &registry,
                    &pool_tx,
                    &completion_tx,
                    &config.live_cwds_provider,
                    session_state_dir,
                    config.activity.as_ref(),
                    &mut spare_reasons,
                );
                next_tick = Instant::now() + tick_cadence;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn handle_registry_msg(
    registry: &Registry,
    pool_tx: &mpsc::Sender<PurgeJob>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    watch_service: &mut Option<gc_watch_service::WatchService>,
    msg: RegistryMsg,
    live_cwds_provider: &LiveCwdsProvider,
    spare_reasons: &mut SpareReasons,
) {
    match msg {
        RegistryMsg::Op(req) => {
            let reply = match req.op {
                GcOp::Watch {
                    kind,
                    watch_dir,
                    repo_root,
                } => {
                    let service = watch_service
                        .get_or_insert_with(|| gc_watch_service::spawn(completion_tx.clone()));
                    let _ = service.register(GcWatchRoot {
                        kind,
                        watch_dir,
                        repo_root,
                    });
                    GcReply::WatchOk
                }
                op => process_op(
                    registry,
                    pool_tx,
                    completion_tx,
                    op,
                    live_cwds_provider(),
                    spare_reasons,
                ),
            };
            // Hung-up callers are fine — the worker keeps serving the rest.
            let _ = req.reply_tx.send(reply);
        }
        RegistryMsg::PurgeCompletion(c) => apply_purge_completion(registry, spare_reasons, c),
        RegistryMsg::ExternVerdicts(verdicts) => {
            spare_reasons.replace_extern(verdicts, now_unix());
        }
        RegistryMsg::RepoWorktreeVerdicts(rows) => {
            spare_reasons.replace_repo_worktrees(rows, now_unix());
        }
        RegistryMsg::RepoWorktreeReclaimed { path, outcome } => {
            apply_repo_worktree_reclaimed(spare_reasons, &path, &outcome);
        }
        RegistryMsg::RepoHints(repos) => record_repo_hints(registry, &repos, now_unix()),
        RegistryMsg::WatchRescan(roots) => {
            for root in roots {
                let _ = reconcile_registered_dir(
                    registry,
                    &root.kind,
                    Path::new(&root.watch_dir),
                    root.repo_root.as_deref().map(Path::new),
                );
            }
        }
    }
}

/// Apply one parallel-purge completion: on success, drop the redb row
/// for the removed entry; on failure, log and keep the row so the next
/// purge attempt retries. Stale rows during/after a partial purge are
/// acceptable — eventual consistency by design (#268).
/// Last-moment safety re-check, run on the purge-pool thread. Returns the
/// spare reason when the entry must NOT be deleted after all.
///
/// Extern and sibling clone verdicts encode "no uncommitted or unpushed
/// work", which a developer can invalidate at any moment. All source kinds,
/// including legacy worktree rows, also receive fresh source-use checks.
/// Cache/trash entries keep their existing eligibility policy.
fn reverify_before_delete(entry: &TrackedEntry) -> Option<&'static str> {
    reverify_before_delete_with(entry, &source_guard::source_in_use)
}

fn reverify_before_delete_with(
    entry: &TrackedEntry,
    source_probe: &dyn Fn(&Path) -> Option<&'static str>,
) -> Option<&'static str> {
    if entry_is_cacheable(entry) {
        let decision = extern_repo_purge_verdict(entry, extern_repo_stale_after());
        if !decision.purge {
            return Some(decision.reason);
        }
    } else if entry.kind == WORKTREE_KIND {
        let path = Path::new(&entry.path);
        if let Some(reason) = source_probe(path) {
            return Some(reason);
        }
        let decision = extern_repo::extern_repo_purge_decision(
            path.is_dir(),
            true,
            true,
            extern_repo::git_work_state(path),
        );
        return (!decision.purge).then_some(decision.reason);
    } else {
        return None;
    }
    source_probe(Path::new(&entry.path))
}

/// Issue #1603: log one reclaim outcome, free its in-flight slot and, when
/// the directory is gone, drop its row from the `gc list` snapshot.
fn apply_repo_worktree_reclaimed(
    spare_reasons: &mut SpareReasons,
    path: &str,
    outcome: &ReclaimOutcome,
) {
    spare_reasons.repo_reclaim_finished(path);
    match outcome {
        ReclaimOutcome::Removed { notes } => {
            spare_reasons.forget_repo_worktree(path);
            eprintln!(
                "[clud] gc: removed repo worktree {path} ({})",
                notes.join("; ")
            );
        }
        ReclaimOutcome::Spared(reason) => {
            eprintln!("[clud] gc: spared repo worktree {path} at delete time: {reason}");
        }
        ReclaimOutcome::Failed(err) => {
            eprintln!("[clud] gc: failed to remove repo worktree {path}: {err}");
        }
    }
}

fn apply_purge_completion(
    registry: &Registry,
    spare_reasons: &mut SpareReasons,
    c: PurgeCompletion,
) {
    if let Some(reason) = c.spared {
        eprintln!(
            "[clud] gc purge: spared {} ({}) at delete time — {reason}",
            c.path, c.kind
        );
        // Issue #2101: `gc list` shows this row as pinned, with this reason.
        spare_reasons.record_delete_spare(c.path, reason);
        return;
    }
    spare_reasons.clear_delete_spare(&c.path);
    match c.result {
        Ok(()) => match registry.delete(c.id) {
            Ok(()) => eprintln!("[gc] purge: removed {} ({})", c.path, c.kind),
            Err(err) => eprintln!(
                "[gc] purge: removed dir but failed to delete redb row id={} path={} ({}): {err}",
                c.id, c.path, c.kind
            ),
        },
        Err(message) => {
            eprintln!(
                "[gc] purge: failed to remove {} ({}): {message}",
                c.path, c.kind
            );
        }
    }
}

fn run_periodic_purge_tick(
    registry: &Registry,
    pool_tx: &mpsc::Sender<PurgeJob>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    live_cwds_provider: &LiveCwdsProvider,
    session_state_dir: Option<&Path>,
    activity: Option<&DaemonActivity>,
    spare_reasons: &mut SpareReasons,
) -> usize {
    let config = GcDiskWatchdogConfig::from_env();
    let repo_worktrees = RepoWorktreeGcConfig::from_env_and_settings();
    run_periodic_purge_tick_with_free_space(
        registry,
        pool_tx,
        completion_tx,
        live_cwds_provider,
        &config,
        &free_space_bytes_for_path,
        PeriodicPurgeContext {
            session_state_dir,
            activity,
            spare_reasons,
            repo_worktrees,
        },
    )
}

/// Run one periodic GC tick, returning the number of purge jobs dispatched to
/// the pool across all three purge paths (disk watchdog, worktree, extern
/// repo).
///
/// The daemon's tick loop ignores the count — completions are applied by the
/// registry-writer thread whenever they arrive. It exists so a caller that
/// must know when the purge has finished can wait for exactly that many
/// completions instead of inferring completion from a quiet interval, which
/// is timing-dependent and was the source of issue #560's Windows flake.
struct PeriodicPurgeContext<'a> {
    session_state_dir: Option<&'a Path>,
    activity: Option<&'a DaemonActivity>,
    /// Verdicts recorded by this tick, for `gc list` to explain retained
    /// rows without re-probing git (issue #896).
    spare_reasons: &'a mut SpareReasons,
    /// Issue #1603: repo-worktree mode and the remote-branch opt-in,
    /// resolved by the caller so tests never read `~/.clud`.
    repo_worktrees: RepoWorktreeGcConfig,
}

fn run_periodic_purge_tick_with_free_space<F>(
    registry: &Registry,
    pool_tx: &mpsc::Sender<PurgeJob>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    live_cwds_provider: &LiveCwdsProvider,
    disk_config: &GcDiskWatchdogConfig,
    free_space: &F,
    context: PeriodicPurgeContext<'_>,
) -> usize
where
    F: Fn(&Path) -> Result<u64, String> + ?Sized,
{
    let PeriodicPurgeContext {
        session_state_dir,
        activity,
        spare_reasons,
        repo_worktrees,
    } = context;

    let mut dispatched = run_disk_watchdog_tick(
        registry,
        pool_tx,
        completion_tx,
        live_cwds_provider,
        disk_config,
        free_space,
        spare_reasons,
    );

    let worktree_reply = process_op(
        registry,
        pool_tx,
        completion_tx,
        GcOp::Purge {
            duration: Some(PERIODIC_GC_WORKTREE_STALE_AFTER.to_string()),
            kind: Some(WORKTREE_KIND.to_string()),
            dry_run: false,
        },
        live_cwds_provider(),
        spare_reasons,
    );
    dispatched += log_periodic_purge_reply(WORKTREE_KIND, worktree_reply);

    let extern_reply = process_op(
        registry,
        pool_tx,
        completion_tx,
        GcOp::Purge {
            duration: None,
            kind: Some(EXTERN_REPO_KIND.to_string()),
            dry_run: false,
        },
        live_cwds_provider(),
        spare_reasons,
    );
    dispatched += log_periodic_purge_reply(EXTERN_REPO_KIND, extern_reply);

    // #1666: creation-ledger rows expire with the session's other GC state
    // (its temp dir and trash keep the same 72h window).
    let ledger_keep = crate::gc::session_tmp::STALE_THRESHOLD.as_secs() as i64;
    match registry.expire_created(now_unix().saturating_sub(ledger_keep)) {
        Ok(0) => {}
        Ok(n) => eprintln!("[clud] gc tick: expired {n} creation-ledger row(s)"),
        Err(err) => eprintln!("[clud] gc tick: creation-ledger expiry failed: {err}"),
    }

    // Issue #946: refresh the verdicts the pass above consumed, off-worker.
    // Listing is the one part only this thread can do (it owns redb); the
    // probe's `git` spawns happen on their own thread and land back as
    // `RegistryMsg::ExternVerdicts`. Results are therefore one tick behind,
    // which is the trade: a checkout becomes reclaimable on the tick after
    // it is first seen clean, rather than stalling every client op.
    match registry.list(None) {
        Ok(rows) => spawn_extern_probe(
            rows.into_iter().filter(entry_is_cacheable).collect(),
            completion_tx,
            activity,
            spare_reasons,
        ),
        Err(err) => eprintln!("[clud] gc tick: extern-repo list failed: {err}"),
    }
    // Issues #1591/#1603: act on the previous snapshot's reclaimable rows,
    // then refresh the snapshot off-worker.
    dispatched += run_repo_worktree_phase(
        registry,
        pool_tx,
        completion_tx,
        activity,
        spare_reasons,
        live_cwds_provider(),
        repo_worktrees,
    );

    filesystem::forget_dangling_source_rows_logged(registry);
    match reap_trash_entries(registry) {
        Ok((removed, failed)) => {
            if removed > 0 || failed > 0 {
                eprintln!("[clud] gc tick: trash removed {removed}, failed {failed}");
            }
        }
        Err(message) => {
            eprintln!("[clud] gc tick: trash error: {message}");
        }
    }
    if let Ok(trash_root) = crate::daemon::default_trash_dir() {
        let removed = reap_unregistered_rm_trash(&trash_root, std::time::SystemTime::now());
        if removed > 0 {
            eprintln!("[clud] gc tick: unregistered rm trash removed {removed}");
        }
    }

    // Issue #549: retire session records whose worker and root are both dead
    // (crash leftovers) so the 2 s proc sampler and per-list liveness probes
    // stop paying for them forever. Synchronous rename/delete only — never
    // routed through the purge pool, and never terminates a process.
    if let Some(dir) = session_state_dir {
        match super::sessions::reconcile_session_records(dir) {
            Ok((tombstoned, deleted)) => {
                if tombstoned > 0 || deleted > 0 {
                    eprintln!(
                        "[clud] gc tick: sessions tombstoned {tombstoned}, deleted {deleted}"
                    );
                }
            }
            Err(message) => {
                eprintln!("[clud] gc tick: sessions error: {message}");
            }
        }
    }

    // Issue #423: daily 7-day sweep of stale uv-cache envs. The
    // helper handles its own 24h sentinel under ~/.clud/state/ so
    // this call is cheap on every tick (one stat + age compare).
    crate::daemon::uv_cache_sweep::maybe_sweep_uv_cache();

    // Issues #509/#510: the session-temp (48h) and target/ (opt-in) sweeps
    // walk the filesystem and can take a while, so they run on a detached
    // background thread rather than blocking this tick loop. They prioritize
    // by disk pressure and system load — see `spawn_maintenance_sweeps`.
    spawn_maintenance_sweeps(disk_config, activity.cloned());

    dispatched
}

/// Kick off the first probe so extern-repo verdicts exist before the first
/// tick. Failures are logged, not fatal: the next tick retries.
fn prime_extern_verdicts(
    registry: &Registry,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    activity: Option<&DaemonActivity>,
    spare_reasons: &mut SpareReasons,
    live_cwds_provider: &LiveCwdsProvider,
) {
    match registry.list(None) {
        Ok(rows) => spawn_extern_probe(
            rows.into_iter().filter(entry_is_cacheable).collect(),
            completion_tx,
            activity,
            spare_reasons,
        ),
        Err(err) => eprintln!("[clud] gc: initial extern-repo list failed: {err}"),
    }
    spawn_repo_worktree_probe(
        registry,
        completion_tx,
        activity,
        spare_reasons,
        live_cwds_provider(),
        repo_worktree_mode_from_env(),
        production_worktree_root(),
    );
}

/// `CLUD_GC_REPO_WORKTREES`: `0` turns repo-worktree GC (#1591) off
/// entirely, `observe` probes and logs without deleting, unset/`1` deletes
/// (#1603). See `reclaim_mode_from_raw`.
const ENV_GC_REPO_WORKTREES: &str = "CLUD_GC_REPO_WORKTREES";
/// Overrides the `gc.delete_remote_branches` setting (#1603).
const ENV_GC_DELETE_REMOTE_BRANCHES: &str = "CLUD_GC_DELETE_REMOTE_BRANCHES";

fn repo_worktree_mode_from_env() -> ReclaimMode {
    reclaim_mode_from_raw(std::env::var(ENV_GC_REPO_WORKTREES).ok().as_deref())
}

/// Issue #1603: how the tick treats repo worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoWorktreeGcConfig {
    mode: ReclaimMode,
    delete_remote: bool,
    /// #1485: `~/.clud/worktrees`, whose children seed the probe and alone
    /// qualify for the abandoned-empty rule. `None` in tests.
    wt_root: Option<PathBuf>,
}

/// #1485: the production worktree root. `None` under `cfg(test)`, so no
/// unit test can probe (let alone reclaim) the developer's real
/// `~/.clud/worktrees`; tests inject a tempdir root instead.
/// Issue #2103: register each repo the shim saw. A repo already known keeps
/// its row untouched (`Registry::note_repo`).
fn record_repo_hints(
    registry: &Registry,
    repos: &std::collections::BTreeMap<String, String>,
    now: i64,
) {
    let mut added = 0usize;
    for (repo_root, cwd) in repos {
        match registry.note_repo(repo_root, cwd, now) {
            Ok(true) => added += 1,
            Ok(false) => {}
            Err(err) => eprintln!("[clud] gc: shim repo hint {repo_root} failed: {err}"),
        }
    }
    if added > 0 {
        eprintln!("[clud] gc: registered {added} repo(s) seen by the git/gh shim");
    }
}

/// The shim log to read repo hints from; `None` in tests, which must never
/// read `~/.clud`.
fn production_hint_source() -> Option<shim_hints::HintSource> {
    if cfg!(test) {
        None
    } else {
        crate::daemon::default_state_dir()
            .ok()
            .map(|state| shim_hints::HintSource::under(&state))
    }
}

fn production_scan_queue_file() -> Option<PathBuf> {
    if cfg!(test) {
        None
    } else {
        crate::daemon::default_state_dir()
            .ok()
            .map(|state| scan_queue::queue_path(&state))
    }
}

fn production_worktree_root() -> Option<PathBuf> {
    if cfg!(test) {
        None
    } else {
        crate::gc::worktree_root::worktree_root()
    }
}

impl RepoWorktreeGcConfig {
    /// Production only: reads the env and `~/.clud/settings.json`. A
    /// settings read error means no remote deletion.
    fn from_env_and_settings() -> Self {
        let delete_remote = match std::env::var(ENV_GC_DELETE_REMOTE_BRANCHES).ok() {
            Some(raw) => parse_bool_setting(Some(&raw), false),
            None => crate::clud_settings::load_gc_delete_remote_branches().unwrap_or(false),
        };
        Self {
            mode: repo_worktree_mode_from_env(),
            delete_remote,
            wt_root: production_worktree_root(),
        }
    }
}

/// Issue #1603: dispatch every reclaimable row of the cached snapshot (taken
/// by the previous probe) to the purge pool, which re-verifies each one from
/// scratch before touching it; then start the next probe. Returns the number
/// of reclaims dispatched. Runs on the registry worker, so it only reads the
/// cache and does path arithmetic — no git.
fn run_repo_worktree_phase(
    registry: &Registry,
    pool_tx: &mpsc::Sender<PurgeJob>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    activity: Option<&DaemonActivity>,
    spare_reasons: &mut SpareReasons,
    live_cwds: Vec<PathBuf>,
    config: RepoWorktreeGcConfig,
) -> usize {
    if spare_reasons.repo_probe_in_flight() {
        return 0;
    }
    let live = canonicalize_live_cwds(live_cwds.clone());
    // A completed batch must refresh even when failed/spared rows remain.
    let rows: Vec<RepoWorktreeRow> = if spare_reasons.repo_snapshot_needs_refresh() {
        Vec::new()
    } else {
        spare_reasons.repo_worktrees().0.to_vec()
    };
    let keep = keep_list_for(config.wt_root.as_deref());
    let mut dispatched = 0usize;
    for row in rows {
        if keep.keeps(&row.path) {
            continue;
        }
        let live_now = entry_path_contains_live_cwd_path(&row.path, &live);
        let in_flight = spare_reasons.repo_reclaim_in_flight(&row.path);
        match reclaim_selection(config.mode, row.verdict.state, live_now, in_flight) {
            ReclaimSelection::Spare(_) => {}
            ReclaimSelection::WouldRemove => eprintln!(
                "[clud] gc tick: observe mode, would remove repo worktree {} ({})",
                row.path, row.verdict.reason
            ),
            ReclaimSelection::Dispatch => {
                let path = row.path.clone();
                let job = PurgeJob {
                    work: PurgeWork::RepoWorktree(ReclaimJob {
                        row,
                        delete_remote: config.delete_remote,
                        session_cwds: live_cwds.clone(),
                        wt_root: config.wt_root.clone(),
                    }),
                    completion_tx: completion_tx.clone(),
                };
                if pool_tx.send(job).is_err() {
                    break;
                }
                spare_reasons.begin_repo_reclaim(path);
                dispatched += 1;
            }
        }
    }
    spawn_repo_worktree_probe(
        registry,
        completion_tx,
        activity,
        spare_reasons,
        live_cwds,
        config.mode,
        config.wt_root,
    );
    dispatched
}

/// Issue #1591: compute squash-aware verdicts for every worktree of every
/// visited repo **off** the registry worker (#946) and deliver the complete
/// snapshot back as `RegistryMsg::RepoWorktreeVerdicts`. The worker only
/// reads the repo-visit list (it owns redb); every `git`/`gh` spawn and the
/// process-table scan happen on the probe thread. Read-only: the snapshot
/// feeds `gc list` and the next tick's `run_repo_worktree_phase`.
fn spawn_repo_worktree_probe(
    registry: &Registry,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    activity: Option<&DaemonActivity>,
    spare_reasons: &mut SpareReasons,
    live_cwds: Vec<PathBuf>,
    mode: ReclaimMode,
    wt_root: Option<PathBuf>,
) {
    if mode == ReclaimMode::Off {
        return;
    }
    let repo_roots: Vec<String> = match registry.list_repo_visits() {
        Ok(rows) => rows.into_iter().map(|r| r.repo_root).collect(),
        Err(err) => {
            eprintln!("[clud] gc: repo-visit list failed: {err}");
            return;
        }
    };
    if !spare_reasons.begin_repo_probe() {
        return;
    }
    let tx = completion_tx.clone();
    let activity = activity.cloned();
    let hint_source = production_hint_source();
    let queue_file = production_scan_queue_file();
    let crawl_file = (!cfg!(test))
        .then(crate::daemon::default_state_dir)
        .and_then(Result::ok)
        .map(|state| fs_backstop::crawl_path(&state));
    let previous = spare_reasons.repo_worktrees().0.to_vec();
    let spawned = thread::Builder::new()
        .name("clud-gc-repo-worktree-probe".to_string())
        .spawn(move || {
            let _job_guard = activity.as_ref().map(DaemonActivity::start_job);
            // #2103: repos only the shim has seen join this same scan.
            let repo_roots = with_shim_hints(repo_roots, hint_source.as_ref(), &tx);
            // #2103 item 5: repos no shim call named (absolute git, `cp -a`).
            let repo_roots = with_fs_backstop(repo_roots, crawl_file.as_deref(), &tx);
            let procs = repo_worktree_probe::collect_process_cwds();
            migrate_legacy_worktree_root(&live_cwds, &procs);
            // #2103: resumable, per-item persisted, time-budgeted.
            let rows = scan_queue::probe_all(
                queue_file.as_deref(),
                &previous,
                &repo_roots,
                &live_cwds,
                &procs,
                wt_root.as_deref(),
            );
            // #1961: dangling admin entries never got pruned, since prune
            // only ran after a reclaim. Delete mode only; metadata only.
            if mode == ReclaimMode::Delete {
                repo_worktree_reclaim_exec::prune_dangling_admin(
                    &repo_worktree_reclaim::repos_with_dangling_admin(&rows),
                );
            }
            // #1878: act job containers whose clud worktree is gone.
            if mode == ReclaimMode::Delete {
                act_containers::sweep(&act_sweep_roots(wt_root.as_deref()));
            }
            // #1687: measure only on this background thread. The CLI reads
            // the snapshot and never walks a checkout to produce its report.
            if let Err(error) = crate::gc::target_size_cache::refresh(
                rows.iter()
                    .filter(|row| !row.reservation)
                    .map(|row| (row.repo_root.clone(), row.path.clone())),
            ) {
                eprintln!("[clud] gc: target size snapshot failed: {error}");
            }
            // Issue #1610: size worktrees here, off the launch path, so the
            // launch banner only reads the cached result.
            refresh_worktree_size_cache(wt_root.as_deref());
            // #2101: warn about, then trash, oversized unmerged worktrees.
            worktree_schedule_exec::run(wt_root.as_deref(), &rows, &live_cwds, &procs, mode, &tx);
            worktree_schedule_exec::tidy_root_logs(wt_root.as_deref(), mode);
            // Release the probe slot only after all filesystem work is done.
            let _ = tx.send(RegistryMsg::RepoWorktreeVerdicts(rows));
        });
    if spawned.is_err() {
        spare_reasons.repo_probe_finished();
    }
}

/// Issue #2103: add the repos the shim log names to `repo_roots`, and hand
/// them to the worker to persist. Runs on the probe thread.
fn with_shim_hints(
    mut repo_roots: Vec<String>,
    source: Option<&shim_hints::HintSource>,
    tx: &mpsc::Sender<RegistryMsg>,
) -> Vec<String> {
    let Some(source) = source else {
        return repo_roots;
    };
    let seen = shim_hints::ingest(source);
    for root in seen.keys() {
        if !repo_roots.contains(root) {
            repo_roots.push(root.clone());
        }
    }
    if !seen.is_empty() {
        let _ = tx.send(RegistryMsg::RepoHints(seen));
    }
    repo_roots
}

/// #2103 item 5: advance the bounded home-directory crawl one step and add
/// what it found, registering the repos like shim hints. `None` (unit tests)
/// skips it.
fn with_fs_backstop(
    mut repo_roots: Vec<String>,
    crawl_file: Option<&Path>,
    tx: &mpsc::Sender<RegistryMsg>,
) -> Vec<String> {
    let (Some(crawl_file), Some(home)) = (crawl_file, crate::home::user_home()) else {
        return repo_roots;
    };
    let mut state = fs_backstop::load(crawl_file);
    let found = fs_backstop::step(&mut state, &home, fs_backstop::ENTRIES_PER_PASS, now_unix());
    if let Err(err) = fs_backstop::save(crawl_file, &state) {
        eprintln!("[clud] gc: filesystem crawl state not saved: {err}");
    }
    for root in found.keys() {
        if !repo_roots.contains(root) {
            repo_roots.push(root.clone());
        }
    }
    if !found.is_empty() {
        let _ = tx.send(RegistryMsg::RepoHints(found));
    }
    repo_roots
}

/// The roots whose missing children orphan an `act` container (#1878):
/// the clud worktree root and its legacy `tmp-wt` name.
fn act_sweep_roots(wt_root: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = wt_root.into_iter().map(Path::to_path_buf).collect();
    if let Some(home) = crate::home::user_home() {
        roots.push(crate::gc::worktree_root::legacy_worktree_root_for(&home));
    }
    roots
}

/// #1962: move what is left under the old `~/.clud/tmp-wt` into
/// `~/.clud/worktrees`. Production only, and never without a process
/// snapshot: without one, nothing shows which entries are in use.
fn migrate_legacy_worktree_root(
    live_cwds: &[PathBuf],
    procs: &repo_worktree_reclaim::ProcessCwdSnapshot,
) {
    if cfg!(test) || !procs.available {
        return;
    }
    let Some(home) = crate::home::user_home() else {
        return;
    };
    let busy: Vec<PathBuf> = live_cwds.iter().chain(&procs.cwds).cloned().collect();
    for line in legacy_worktree_root::migrate(&home, &busy) {
        eprintln!("[clud] gc: {line}");
    }
}

/// Issue #1610: bounded `check_tree_size` walk of `worktrees`, cached for the
/// launch banner. Best effort; errors are ignored. Warn-only: nothing is
/// deleted for size.
fn refresh_worktree_size_cache(wt_root: Option<&std::path::Path>) {
    let Some(root) = wt_root else { return };
    let warn_bytes = crate::clud_settings::load_worktrees_warn_bytes()
        .unwrap_or(crate::gc::worktree_root::DEFAULT_WARN_BYTES);
    let _ = crate::gc::worktree_size_cache::refresh_cache(
        root,
        warn_bytes,
        crate::gc::worktree_size_cache::now_unix(),
    );
}

/// Issue #1327: bounded, read-only size walk of `~/.clud/tmp`, cached for
/// the launch banner. Runs after the age sweep so it sizes what survived.
/// Best effort; warn-only: nothing is deleted for size.
fn refresh_session_tmp_size_cache() {
    // Unit tests never write into the real `~/.clud`.
    if cfg!(test) {
        return;
    }
    let Some(home) = crate::home::user_home() else {
        return;
    };
    let warn_bytes = crate::clud_settings::load_tmp_warn_bytes()
        .unwrap_or(crate::gc::worktree_size_cache::DEFAULT_TMP_WARN_BYTES);
    let _ = crate::gc::worktree_size_cache::refresh_tmp_cache(
        &home,
        warn_bytes,
        crate::gc::worktree_size_cache::now_unix(),
    );
}

/// Issue #1691: bounded, read-only size walk of `~/.clud/cache` (mostly
/// uv's own cache), cached for the launch banner. Warn-only (DD-144).
fn refresh_clud_cache_size_cache() {
    // Unit tests never write into the real `~/.clud`.
    if cfg!(test) {
        return;
    }
    let Some(home) = crate::home::user_home() else {
        return;
    };
    let warn_bytes = crate::clud_settings::load_cache_warn_bytes()
        .unwrap_or(crate::gc::worktree_size_cache::DEFAULT_CACHE_WARN_BYTES);
    let _ = crate::gc::worktree_size_cache::refresh_clud_cache_size(
        &home,
        warn_bytes,
        crate::gc::worktree_size_cache::now_unix(),
    );
}

/// Issue #1591: `gc list` rows for the cached repo-worktree snapshot.
/// Paths the registry already tracks are skipped (their own row wins), and a
/// live session cwd inside a worktree pins it now, even if the snapshot is
/// up to a tick old. `id` is 0: no redb row backs these, so no delete path
/// can select them.
/// Issue #2101: the `clud gc keep` list beside `wt_root`. Without a root
/// nothing is reclaimed anyway, so an empty list is correct.
fn keep_list_for(wt_root: Option<&Path>) -> crate::gc::keep_list::KeepList {
    match wt_root {
        Some(root) => crate::gc::keep_list::load(&crate::gc::keep_list::keep_path_for(root)),
        None => crate::gc::keep_list::KeepList::Paths(Default::default()),
    }
}

fn repo_worktree_list_rows(
    spare_reasons: &SpareReasons,
    tracked_paths: &HashSet<String>,
    live_cwds: &[PathBuf],
    keep: &crate::gc::keep_list::KeepList,
) -> Vec<ListRow> {
    let (rows, evaluated_unix) = spare_reasons.repo_worktrees();
    rows.iter()
        .filter(|r| !tracked_paths.contains(&r.path))
        .map(|r| {
            let live = entry_path_contains_live_cwd_path(&r.path, live_cwds);
            let (state, reason) = if live {
                ("pinned", "process inside".to_string())
            } else if keep.keeps(&r.path) {
                ("pinned", crate::gc::keep_list::KEPT_REASON.to_string())
            } else {
                (r.verdict.state.as_str(), r.verdict.reason.clone())
            };
            ListRow {
                id: 0,
                kind: REPO_WORKTREE_KIND.to_string(),
                path: r.path.clone(),
                repo_root: (!r.reservation).then(|| r.repo_root.clone()),
                branch: r.branch.clone(),
                agent_id: None,
                created_unix: r.mtime_unix,
                live_locked: live,
                state: state.to_string(),
                reclaimable: state == "reclaimable",
                reason: Some(reason),
                evaluated_unix,
            }
        })
        .collect()
}

/// Compute extern-repo purge verdicts **off** the registry worker thread and
/// deliver them back as `RegistryMsg::ExternVerdicts`.
///
/// The probe spawns up to three `git` processes per checkout. Running that on
/// the worker — the single thread that owns redb — puts it in front of every
/// client op, including launch-path `record_repo_visit`, which is what this
/// issue exists to fix. The worker instead does the one thing only it can
/// (read the registry), hands the rows over, and returns to serving.
///
/// `entries` is every extern-repo row, so the reply is a complete snapshot.
fn spawn_extern_probe(
    entries: Vec<TrackedEntry>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    activity: Option<&DaemonActivity>,
    spare_reasons: &mut SpareReasons,
) {
    if entries.is_empty() {
        // Nothing to probe. Publish an empty snapshot so the cache drops
        // verdicts for rows that no longer exist — but NOT while a real
        // probe is running: applying a snapshot also frees the probe slot,
        // so an empty one would let a second probe start and its older
        // reply could then overwrite the newer one.
        if !spare_reasons.probe_in_flight() {
            let _ = completion_tx.send(RegistryMsg::ExternVerdicts(Vec::new()));
        }
        return;
    }
    if !spare_reasons.begin_probe() {
        // A previous probe is still running — let it finish.
        return;
    }
    let tx = completion_tx.clone();
    let activity = activity.cloned();
    let stale_after = extern_repo_stale_after();
    let spawned = thread::Builder::new()
        .name("clud-gc-extern-probe".to_string())
        .spawn(move || {
            let _job_guard = activity.as_ref().map(DaemonActivity::start_job);
            let verdicts = entries
                .into_iter()
                .map(|entry| {
                    let decision = extern_repo_purge_verdict(&entry, stale_after);
                    (entry.path, decision)
                })
                .collect();
            // The worker clears the in-flight flag when it applies this
            // message. A send failure means the worker is gone (teardown),
            // so there is nothing left to unblock.
            let _ = tx.send(RegistryMsg::ExternVerdicts(verdicts));
        });
    if spawned.is_err() {
        spare_reasons.probe_finished();
    }
}

/// Guards against overlapping background maintenance sweeps. The tick fires
/// every cadence; a long `target/` walk can outlast one interval, so we never
/// spawn a second sweep thread while one is still in flight.
static MAINTENANCE_SWEEP_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Global-CPU ceiling (percent) above which the non-urgent sweeps defer.
const ENV_GC_SWEEP_MAX_CPU_PCT: &str = "CLUD_GC_SWEEP_MAX_CPU_PCT";
const DEFAULT_GC_SWEEP_MAX_CPU_PCT: f32 = 60.0;

/// What the background maintenance sweep should do this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaintenanceAction {
    /// Disk is low — reclaim immediately, bypassing the sentinel throttle.
    RunUrgent,
    /// Disk is fine and the box is idle — run the throttled sweeps.
    RunNormal,
    /// Disk is fine but the box is busy — skip; the next tick retries.
    Defer,
}

/// Pure decision: disk pressure wins over everything (reclaim now); otherwise
/// only run when the machine is not under load.
fn maintenance_action(low_disk: bool, cpu_busy: bool) -> MaintenanceAction {
    if low_disk {
        MaintenanceAction::RunUrgent
    } else if cpu_busy {
        MaintenanceAction::Defer
    } else {
        MaintenanceAction::RunNormal
    }
}

/// Issues #509/#510: fan the filesystem sweeps onto a detached thread so a
/// long walk never blocks the GC tick loop. At most one sweep thread runs at
/// a time (guarded by [`MAINTENANCE_SWEEP_IN_FLIGHT`]).
fn spawn_maintenance_sweeps(disk_config: &GcDiskWatchdogConfig, activity: Option<DaemonActivity>) {
    if MAINTENANCE_SWEEP_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // A previous sweep is still running — don't stack another.
        return;
    }
    let warn_free_bytes = disk_config.warn_free_bytes;
    let spawned = std::thread::Builder::new()
        .name("clud-gc-sweep".to_string())
        .spawn(move || {
            let _job_guard = activity.as_ref().map(DaemonActivity::start_job);
            run_maintenance_sweeps(warn_free_bytes);
            MAINTENANCE_SWEEP_IN_FLIGHT.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        // Thread spawn failed — release the guard so the next tick can retry.
        MAINTENANCE_SWEEP_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Body of the background sweep thread. Runs the CPU sample + disk probe here
/// (off the tick loop) and then dispatches per [`maintenance_action`].
fn run_maintenance_sweeps(warn_free_bytes: u64) {
    let low_disk = maintenance_disk_pressure(warn_free_bytes);
    // Only pay for the ~200ms CPU sample when it can change the decision.
    let cpu_busy = if low_disk { false } else { system_cpu_busy() };
    match maintenance_action(low_disk, cpu_busy) {
        MaintenanceAction::RunUrgent => {
            crate::daemon::session_tmp_sweep::sweep_now();
            refresh_session_tmp_size_cache();
            refresh_clud_cache_size_cache();
            crate::daemon::session_state_sweep::sweep_now();
            crate::daemon::target_sweep::sweep_now();
            crate::daemon::trash_cap::maybe_enforce();
        }
        MaintenanceAction::RunNormal => {
            crate::daemon::session_tmp_sweep::maybe_sweep_session_tmp();
            refresh_session_tmp_size_cache();
            refresh_clud_cache_size_cache();
            crate::daemon::session_state_sweep::maybe_sweep_session_state();
            crate::daemon::target_sweep::maybe_sweep_targets();
            crate::daemon::trash_cap::maybe_enforce();
        }
        MaintenanceAction::Defer => {}
    }
}

/// True when free space on the `~/.clud` volume (session temp + uv cache live
/// there) or any configured target root is below `warn_free_bytes`.
fn maintenance_disk_pressure(warn_free_bytes: u64) -> bool {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(dir) = crate::gc::session_tmp::session_tmp_dir() {
        paths.push(dir);
    }
    paths.extend(crate::daemon::target_sweep::configured_roots());
    paths
        .iter()
        .any(|path| matches!(free_space_bytes_for_path(path), Ok(free) if free < warn_free_bytes))
}

/// Sample global CPU usage over a short window; true when above the configured
/// ceiling (default 60%, override `CLUD_GC_SWEEP_MAX_CPU_PCT`). Called on the
/// background thread, so the mandatory ~200ms settle never touches the tick.
fn system_cpu_busy() -> bool {
    let ceiling = sweep_cpu_ceiling_pct(std::env::var(ENV_GC_SWEEP_MAX_CPU_PCT).ok().as_deref());
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_usage();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_usage();
    sys.global_cpu_usage() > ceiling
}

fn sweep_cpu_ceiling_pct(raw: Option<&str>) -> f32 {
    raw.and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|p| p.is_finite() && *p > 0.0)
        .unwrap_or(DEFAULT_GC_SWEEP_MAX_CPU_PCT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskWatchdogDecision {
    warn: bool,
    auto_purge: bool,
}

fn disk_watchdog_decision(config: &GcDiskWatchdogConfig, free_bytes: u64) -> DiskWatchdogDecision {
    DiskWatchdogDecision {
        warn: free_bytes < config.warn_free_bytes,
        auto_purge: config.auto_purge_enabled && free_bytes < config.auto_purge_free_bytes,
    }
}

fn run_disk_watchdog_tick<F>(
    registry: &Registry,
    pool_tx: &mpsc::Sender<PurgeJob>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    live_cwds_provider: &LiveCwdsProvider,
    config: &GcDiskWatchdogConfig,
    free_space: &F,
    spare_reasons: &mut SpareReasons,
) -> usize
where
    F: Fn(&Path) -> Result<u64, String> + ?Sized,
{
    let entries = match registry.list(None) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("[clud] gc tick disk: failed to list tracked roots: {err}");
            return 0;
        }
    };
    let roots = collect_tracked_entry_roots(&entries);
    let mut purge_roots = Vec::new();
    for root in roots {
        match free_space(&root) {
            Ok(free_bytes) => {
                let decision = disk_watchdog_decision(config, free_bytes);
                if decision.warn {
                    eprintln!(
                        "[clud] gc tick disk: {} has {} GB free, below warn threshold {} GB",
                        root.display(),
                        format_gb(free_bytes),
                        format_gb(config.warn_free_bytes)
                    );
                }
                if decision.auto_purge {
                    purge_roots.push(root);
                }
            }
            Err(message) => {
                eprintln!(
                    "[clud] gc tick disk: failed to sample free space for {}: {message}",
                    root.display()
                );
            }
        }
    }

    if purge_roots.is_empty() {
        return 0;
    }
    let purge_reply = purge_old_reclaimable_entries_for_roots(
        registry,
        pool_tx,
        completion_tx,
        live_cwds_provider,
        &purge_roots,
        config.min_age,
        spare_reasons,
    );
    log_disk_watchdog_purge_reply(purge_roots.len(), config, purge_reply)
}

#[path = "gc_service_disk_watchdog.rs"]
mod gc_service_disk_watchdog;
use gc_service_disk_watchdog::{
    collect_tracked_entry_roots, duration_secs_i64, format_gb, free_space_bytes_for_path,
    log_disk_watchdog_purge_reply, log_periodic_purge_reply,
    purge_old_reclaimable_entries_for_roots,
};
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn process_op(
    registry: &Registry,
    pool_tx: &mpsc::Sender<PurgeJob>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    op: GcOp,
    live_cwds: Vec<PathBuf>,
    spare_reasons: &mut SpareReasons,
) -> GcReply {
    match op {
        GcOp::Watch { .. } => unreachable!("watch registration is handled before process_op"),
        GcOp::List { kind } => match registry.list(kind.as_deref()) {
            Ok(rows) => {
                let live_locks = collect_live_lock_paths();
                // The other flavour of liveness: an agent cwd inside the
                // entry. This is what actually spares extern-repo rows, and
                // it is pure path arithmetic — no subprocess — so `List` can
                // derive it rather than rely on a cached marker the next
                // probe snapshot would wipe.
                let live_cwds = canonicalize_live_cwds(live_cwds);
                let mut out: Vec<ListRow> = rows
                    .into_iter()
                    .map(|r| {
                        let live_locked = (r.kind == "worktree" && live_locks.contains(&r.path))
                            || entry_path_contains_live_cwd_path(&r.path, &live_cwds);
                        let cached = spare_reasons.get(&r.path);
                        // Issue #896. Deliberately no git probe here — the
                        // verdict comes from the tick's cache; see
                        // `list_state` and #946.
                        let (state, reason) =
                            // `try_exists().unwrap_or(true)`, not `exists()`:
                            // the latter reports *any* error as absent, so an
                            // EACCES on a parent or a flaky mount would paint a
                            // live row red. "Present" is the conservative
                            // direction for a problem state.
                            derive_state(
                                Path::new(&r.path).try_exists().unwrap_or(true),
                                live_locked,
                                spare_reasons.delete_spare(&r.path),
                                cached,
                            );
                        ListRow {
                            live_locked,
                            state: state.as_str().to_string(),
                            reason: reason.map(str::to_string),
                            reclaimable: state == EntryState::Reclaimable,
                            evaluated_unix: cached.map(|c| c.evaluated_unix),
                            id: r.id,
                            kind: r.kind,
                            path: r.path,
                            repo_root: r.repo_root,
                            branch: r.branch,
                            agent_id: r.agent_id,
                            created_unix: r.created_unix,
                        }
                    })
                    .collect();
                // Issue #1591: append the cached repo-worktree verdicts. No
                // git here either — they come from the off-worker snapshot.
                if kind.as_deref().is_none_or(|k| k == REPO_WORKTREE_KIND) {
                    let tracked: HashSet<String> = match kind {
                        None => out.iter().map(|r| r.path.clone()).collect(),
                        Some(_) => registry
                            .list(None)
                            .map(|rows| rows.into_iter().map(|r| r.path).collect())
                            .unwrap_or_default(),
                    };
                    out.extend(repo_worktree_list_rows(
                        spare_reasons,
                        &tracked,
                        &live_cwds,
                        &keep_list_for(production_worktree_root().as_deref()),
                    ));
                }
                GcReply::ListOk { rows: out }
            }
            Err(e) => GcReply::Error {
                message: e.to_string(),
            },
        },

        GcOp::Purge {
            duration,
            kind,
            dry_run,
        } => {
            let candidates_res = match &duration {
                Some(d) => match worktrees::parse_duration(d) {
                    Ok(dur) => {
                        let cutoff = now_unix().saturating_sub(duration_secs_i64(dur));
                        registry.select_older_than(cutoff, kind.as_deref())
                    }
                    Err(e) => {
                        return GcReply::Error {
                            message: format!("invalid duration: {e}"),
                        };
                    }
                },
                None => registry.list(kind.as_deref()),
            };
            let candidates: Vec<TrackedEntry> = match candidates_res {
                Ok(v) => v,
                Err(e) => {
                    return GcReply::Error {
                        message: e.to_string(),
                    };
                }
            };
            if dry_run {
                dry_run_purge_entries(candidates, live_cwds, spare_reasons)
            } else {
                dispatch_purge_entries(pool_tx, completion_tx, candidates, live_cwds, spare_reasons)
            }
        }

        GcOp::Reconcile { repo_root } => {
            let root = PathBuf::from(&repo_root);
            match reconcile_repo_root(registry, &root) {
                Ok(inserted) => GcReply::ReconcileOk { inserted },
                Err(e) => GcReply::Error {
                    message: e.to_string(),
                },
            }
        }

        GcOp::Insert {
            kind,
            path,
            repo_root,
            branch,
            agent_id,
            created_unix,
        } => {
            let input = InsertInput {
                kind,
                path,
                repo_root,
                branch,
                agent_id,
                now_unix: created_unix.unwrap_or_else(now_unix),
            };
            match registry.insert_if_new(&input) {
                Ok(inserted) => GcReply::InsertOk { inserted },
                Err(e) => GcReply::Error {
                    message: e.to_string(),
                },
            }
        }

        GcOp::RecordRepoVisit {
            repo_root,
            cwd,
            now_unix: provided,
        } => {
            let stamp = provided.unwrap_or_else(now_unix);
            match registry.record_repo_visit(&repo_root, &cwd, stamp) {
                Ok(()) => GcReply::RepoVisitOk,
                Err(e) => GcReply::Error {
                    message: e.to_string(),
                },
            }
        }

        GcOp::InsertCreated { entry } => match registry.insert_created(&entry) {
            Ok(inserted) => GcReply::CreatedInsertOk { inserted },
            Err(e) => GcReply::Error {
                message: e.to_string(),
            },
        },

        GcOp::QueryCreated { session_id, path } => {
            match registry.query_created(&session_id, &path) {
                Ok(rows) => GcReply::CreatedRowsOk { rows },
                Err(e) => GcReply::Error {
                    message: e.to_string(),
                },
            }
        }

        GcOp::ListRepoVisits => match registry.list_repo_visits() {
            Ok(rows) => GcReply::RepoVisitsOk { rows },
            Err(e) => GcReply::Error {
                message: e.to_string(),
            },
        },

        GcOp::DeleteById { id } => {
            let entries = match registry.list(None) {
                Ok(v) => v,
                Err(e) => {
                    return GcReply::Error {
                        message: e.to_string(),
                    };
                }
            };
            let Some(target) = entries.into_iter().find(|e| e.id == id) else {
                // Idempotent: an id that no longer exists is `removed=0,
                // skipped=0`. The dashboard refreshes after every delete
                // so a stale id click is silently a no-op.
                return GcReply::PurgeOk {
                    removed: 0,
                    skipped: 0,
                };
            };
            let live_locks = collect_live_lock_paths();
            let live_cwds = canonicalize_live_cwds(live_cwds);
            if entry_is_live(&target, &live_locks, &live_cwds) {
                return GcReply::PurgeOk {
                    removed: 0,
                    skipped: 1,
                };
            }
            match remove_entry_and_delete_row(registry, &target) {
                Ok(()) => GcReply::PurgeOk {
                    removed: 1,
                    skipped: 0,
                },
                Err(message) => GcReply::Error { message },
            }
        }
    }
}

/// Filter `candidates` down to entries that are safe to purge. Returns
/// `(purgeable, skipped)`. Centralizes the live-lock / live-cwd /
/// kind-allows-purge gates so the dry-run and dispatch paths agree on
/// what counts as eligible.
#[path = "gc_service_purge.rs"]
mod gc_service_purge;
use gc_service_purge::*;

#[cfg(test)]
mod tests;

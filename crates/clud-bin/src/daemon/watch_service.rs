//! Daemon-owned filesystem discovery for GC entries (issue #545).
//!
//! Foreground clients register roots once, then exit this subsystem entirely.
//! A single daemon thread deduplicates canonical roots, watches immediate
//! child-directory changes, coalesces bursty events, and asks the registry
//! worker to reconcile affected roots. Notifications are an optimization, not
//! correctness-critical: every root also has an exponential-backoff rescan so
//! a watcher overflow or Windows watcher failure cannot silently lose entries.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use kernal_api::platform::fs_watch::{RecursiveMode, WatchNotification, Watcher};

use crate::gc::watch_event_may_affect_registration;

use super::gc_service::RegistryMsg;
use super::types::GcWatchRoot;

const SETTLE_WINDOW: Duration = Duration::from_millis(50);
const INITIAL_BACKOFF: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Starts the daemon-owned watch thread and returns its registration channel.
/// The channel is unbounded: registration is intentionally a tiny
/// fire-and-forget operation on the foreground launch path.
pub(super) fn spawn(registry_tx: mpsc::Sender<RegistryMsg>) -> WatchService {
    let (tx, rx) = mpsc::channel();
    let service = WatchService { tx: tx.clone() };
    let _ = thread::Builder::new()
        .name("clud-gc-watch-service".to_string())
        .spawn(move || run(rx, tx, registry_tx));
    service
}

#[derive(Clone)]
pub(super) struct WatchService {
    tx: mpsc::Sender<ServiceMsg>,
}

impl WatchService {
    pub(super) fn register(&self, root: GcWatchRoot) -> bool {
        self.tx.send(ServiceMsg::Register(root)).is_ok()
    }
}

enum ServiceMsg {
    Register(GcWatchRoot),
    Event(std::io::Result<WatchNotification>),
}

#[derive(Debug)]
struct RootEntry {
    registrations: HashSet<GcWatchRoot>,
    watching: bool,
    settle_at: Option<Instant>,
    next_fallback: Instant,
    backoff: Duration,
}

#[derive(Debug, Default)]
struct WatchRegistry {
    roots: HashMap<PathBuf, RootEntry>,
}

impl WatchRegistry {
    /// Returns the canonical root, whether it needs a new OS watcher, and
    /// whether this is a new `(kind, root, repo_root)` registration.
    fn register(&mut self, root: GcWatchRoot, now: Instant) -> (PathBuf, bool, bool) {
        let root = normalize_root(root);
        let key = PathBuf::from(&root.watch_dir);
        let first = !self.roots.contains_key(&key);
        let entry = self.roots.entry(key.clone()).or_insert_with(|| RootEntry {
            registrations: HashSet::new(),
            watching: false,
            settle_at: None,
            next_fallback: now + INITIAL_BACKOFF,
            backoff: INITIAL_BACKOFF,
        });
        let registration_new = entry.registrations.insert(root);
        (key, first, registration_new)
    }

    fn registrations(&self, root: &Path) -> Vec<GcWatchRoot> {
        self.roots
            .get(root)
            .map(|entry| entry.registrations.iter().cloned().collect())
            .unwrap_or_default()
    }
}

fn run(
    rx: mpsc::Receiver<ServiceMsg>,
    tx: mpsc::Sender<ServiceMsg>,
    registry_tx: mpsc::Sender<RegistryMsg>,
) {
    let watcher_result = Watcher::new(move |event| {
        let _ = tx.send(ServiceMsg::Event(event));
    });
    // The fallback scan remains active if the OS watcher itself cannot be
    // created. Avoid making daemon bring-up depend on native watch APIs.
    let mut watcher = watcher_result.ok();
    let mut roots = WatchRegistry::default();

    loop {
        let now = Instant::now();
        let timeout = next_wait(&roots, now);
        match rx.recv_timeout(timeout) {
            Ok(ServiceMsg::Register(root)) => {
                let now = Instant::now();
                let (path, first, registration_new) = roots.register(root, now);
                if first {
                    ensure_watched(watcher.as_mut(), &mut roots, &path);
                }
                if registration_new {
                    dispatch_scan(&registry_tx, roots.registrations(&path));
                }
            }
            Ok(ServiceMsg::Event(event)) => mark_event(&mut roots, event, Instant::now()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        dispatch_due(&mut roots, &mut watcher, &registry_tx, Instant::now());
    }
}

fn ensure_watched(watcher: Option<&mut Watcher>, roots: &mut WatchRegistry, root: &Path) {
    let Some(entry) = roots.roots.get_mut(root) else {
        return;
    };
    if entry.watching || !root.exists() {
        return;
    }
    if let Some(watcher) = watcher {
        entry.watching = watcher.watch(root, RecursiveMode::NonRecursive).is_ok();
    }
}

fn mark_event(roots: &mut WatchRegistry, event: std::io::Result<WatchNotification>, now: Instant) {
    let affected: Vec<PathBuf> = match event {
        Ok(WatchNotification::Change(event)) if !event.paths().is_empty() => roots
            .roots
            .iter()
            .filter(|(root, entry)| {
                entry.registrations.iter().any(|registration| {
                    event.paths().iter().any(|path| {
                        watch_event_may_affect_registration(
                            &registration.kind,
                            root,
                            registration.repo_root.as_deref().map(Path::new),
                            path,
                        )
                    })
                })
            })
            .map(|(root, _)| root.clone())
            .collect(),
        // Watcher errors and rescan notices (overflow, lost watch) must be
        // recovered by a full scan.
        // Some backends cannot identify the failed root, so conservatively
        // rescan registered roots; the normal event path remains root-local.
        _ => roots.roots.keys().cloned().collect(),
    };
    for root in affected {
        if let Some(entry) = roots.roots.get_mut(&root) {
            entry.settle_at = Some(now + SETTLE_WINDOW);
        }
    }
}

fn dispatch_due(
    roots: &mut WatchRegistry,
    watcher: &mut Option<Watcher>,
    registry_tx: &mpsc::Sender<RegistryMsg>,
    now: Instant,
) {
    let mut scans = Vec::new();
    let unwatched: Vec<PathBuf> = roots
        .roots
        .iter()
        .filter_map(|(root, entry)| (!entry.watching).then_some(root.clone()))
        .collect();
    for root in unwatched {
        ensure_watched(watcher.as_mut(), roots, &root);
    }
    for entry in roots.roots.values_mut() {
        let event_due = entry.settle_at.is_some_and(|at| at <= now);
        let fallback_due = entry.next_fallback <= now;
        if !event_due && !fallback_due {
            continue;
        }
        if event_due {
            entry.settle_at = None;
            entry.backoff = INITIAL_BACKOFF;
        } else {
            entry.backoff = next_backoff(entry.backoff, false);
        }
        entry.next_fallback = now + entry.backoff;
        scans.push(entry.registrations.iter().cloned().collect::<Vec<_>>());
    }
    for registrations in scans {
        dispatch_scan(registry_tx, registrations);
    }
}

fn dispatch_scan(registry_tx: &mpsc::Sender<RegistryMsg>, roots: Vec<GcWatchRoot>) {
    if !roots.is_empty() {
        let _ = registry_tx.send(RegistryMsg::WatchRescan(roots));
    }
}

fn next_wait(roots: &WatchRegistry, now: Instant) -> Duration {
    let next = roots
        .roots
        .values()
        .flat_map(|entry| {
            [
                entry.next_fallback,
                entry.settle_at.unwrap_or(entry.next_fallback),
            ]
        })
        .min()
        .unwrap_or(now + Duration::from_secs(1));
    next.saturating_duration_since(now)
        .min(Duration::from_secs(1))
}

fn normalize_root(mut root: GcWatchRoot) -> GcWatchRoot {
    root.watch_dir = normalize_path(Path::new(&root.watch_dir))
        .to_string_lossy()
        .to_string();
    root.repo_root = root
        .repo_root
        .as_deref()
        .map(Path::new)
        .map(normalize_path)
        .map(|path| path.to_string_lossy().to_string());
    root
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// Bounded reconciliation schedule. A change resets the next scan to 30s;
/// unchanged roots double to a 15-minute ceiling.
pub(super) fn next_backoff(previous: Duration, changed: bool) -> Duration {
    if changed {
        return INITIAL_BACKOFF;
    }
    previous
        .saturating_mul(2)
        .min(MAX_BACKOFF)
        .max(INITIAL_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str, kind: &str, repo_root: Option<&str>) -> GcWatchRoot {
        GcWatchRoot {
            kind: kind.to_string(),
            watch_dir: path.to_string(),
            repo_root: repo_root.map(str::to_string),
        }
    }

    #[test]
    fn registry_dedups_watchers_per_canonical_root() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let path = dir.path().to_string_lossy();
        let mut registry = WatchRegistry::default();
        assert!(
            registry
                .register(root(&path, "worktree", Some("repo-a")), now)
                .1
        );
        let second = registry.register(root(&path, "sibling-clone", Some("repo-b")), now);
        assert!(!second.1);
        assert!(second.2);
        assert!(
            !registry
                .register(root(&path, "sibling-clone", Some("repo-b")), now)
                .2
        );
        assert_eq!(registry.roots.len(), 1);
        assert_eq!(
            registry.roots.values().next().unwrap().registrations.len(),
            2
        );
    }

    #[test]
    fn next_backoff_doubles_and_resets() {
        assert_eq!(
            next_backoff(Duration::from_secs(30), false),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(480), false),
            Duration::from_secs(900)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(900), false),
            Duration::from_secs(900)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(900), true),
            Duration::from_secs(30)
        );
    }
}

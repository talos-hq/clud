use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A point-in-time view of daemon work that can prevent idle shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ActivitySnapshot {
    pub idle_for: Duration,
    pub active_connections: usize,
    pub active_jobs: usize,
}

/// Shared daemon liveness activity. Workers and foreground client leases stay
/// owned by their established registries; their counts are supplied to the
/// predicate so this tracker cannot accidentally duplicate either lifecycle.
#[derive(Clone)]
pub(super) struct DaemonActivity {
    last_activity: Arc<Mutex<Instant>>,
    active_connections: Arc<AtomicUsize>,
    active_jobs: Arc<AtomicUsize>,
}

impl DaemonActivity {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            last_activity: Arc::new(Mutex::new(now)),
            active_connections: Arc::new(AtomicUsize::new(0)),
            active_jobs: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(super) fn note_activity(&self, now: Instant) {
        *self
            .last_activity
            .lock()
            .expect("daemon activity mutex poisoned") = now;
    }

    pub(super) fn start_connection(&self) -> ActiveWorkGuard {
        self.start(WorkKind::Connection)
    }

    pub(super) fn start_job(&self) -> ActiveWorkGuard {
        self.start(WorkKind::Job)
    }

    fn start(&self, kind: WorkKind) -> ActiveWorkGuard {
        self.note_activity(Instant::now());
        match kind {
            WorkKind::Connection => self.active_connections.fetch_add(1, Ordering::AcqRel),
            WorkKind::Job => self.active_jobs.fetch_add(1, Ordering::AcqRel),
        };
        ActiveWorkGuard {
            activity: self.clone(),
            kind,
        }
    }

    pub(super) fn snapshot(&self, now: Instant) -> ActivitySnapshot {
        let last_activity = *self
            .last_activity
            .lock()
            .expect("daemon activity mutex poisoned");
        ActivitySnapshot {
            idle_for: now.saturating_duration_since(last_activity),
            active_connections: self.active_connections.load(Ordering::Acquire),
            active_jobs: self.active_jobs.load(Ordering::Acquire),
        }
    }
}

#[derive(Clone, Copy)]
enum WorkKind {
    Connection,
    Job,
}

pub(super) struct ActiveWorkGuard {
    activity: DaemonActivity,
    kind: WorkKind,
}

impl Drop for ActiveWorkGuard {
    fn drop(&mut self) {
        match self.kind {
            WorkKind::Connection => self
                .activity
                .active_connections
                .fetch_sub(1, Ordering::AcqRel),
            WorkKind::Job => self.activity.active_jobs.fetch_sub(1, Ordering::AcqRel),
        };
        self.activity.note_activity(Instant::now());
    }
}

/// The idle predicate deliberately receives externally-owned counts: workers
/// remain in the daemon worker map and foreground clients in the lease registry.
pub(super) fn should_idle_shutdown(
    timeout: Option<Duration>,
    activity: ActivitySnapshot,
    worker_count: usize,
    lease_count: usize,
) -> bool {
    timeout.is_some_and(|timeout| {
        activity.idle_for >= timeout
            && activity.active_connections == 0
            && activity.active_jobs == 0
            && worker_count == 0
            && lease_count == 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_for(seconds: u64) -> ActivitySnapshot {
        ActivitySnapshot {
            idle_for: Duration::from_secs(seconds),
            active_connections: 0,
            active_jobs: 0,
        }
    }

    #[test]
    fn predicate_requires_elapsed_timeout_and_no_liveness_blockers() {
        let timeout = Some(Duration::from_secs(5));
        assert!(!should_idle_shutdown(timeout, idle_for(4), 0, 0));
        assert!(!should_idle_shutdown(timeout, idle_for(5), 1, 0));
        assert!(!should_idle_shutdown(timeout, idle_for(5), 0, 1));
        assert!(!should_idle_shutdown(
            timeout,
            ActivitySnapshot {
                active_connections: 1,
                ..idle_for(5)
            },
            0,
            0,
        ));
        assert!(!should_idle_shutdown(
            timeout,
            ActivitySnapshot {
                active_jobs: 1,
                ..idle_for(5)
            },
            0,
            0,
        ));
        assert!(should_idle_shutdown(timeout, idle_for(5), 0, 0));
        assert!(!should_idle_shutdown(None, idle_for(500), 0, 0));
    }

    #[test]
    fn readiness_renews_the_production_idle_window_after_slow_initialization() {
        let started = Instant::now();
        let activity = DaemonActivity::new(started);
        let ready = started + Duration::from_secs(3);
        let timeout = Some(Duration::from_secs(2));
        assert!(should_idle_shutdown(
            timeout,
            activity.snapshot(ready),
            0,
            0
        ));

        activity.note_activity(ready);
        assert!(!should_idle_shutdown(
            timeout,
            activity.snapshot(ready),
            0,
            0
        ));
        assert!(!should_idle_shutdown(
            timeout,
            activity.snapshot(ready + Duration::from_secs(1)),
            0,
            0,
        ));
        assert!(should_idle_shutdown(
            timeout,
            activity.snapshot(ready + Duration::from_secs(2)),
            0,
            0,
        ));
    }

    #[test]
    fn guards_increment_counts_and_renew_activity_when_work_finishes() {
        let started = Instant::now();
        let activity = DaemonActivity::new(started);
        let connection = activity.start_connection();
        let job = activity.start_job();
        let busy = activity.snapshot(Instant::now());
        assert_eq!(busy.active_connections, 1);
        assert_eq!(busy.active_jobs, 1);
        drop(connection);
        drop(job);
        let idle = activity.snapshot(Instant::now());
        assert_eq!(idle.active_connections, 0);
        assert_eq!(idle.active_jobs, 0);
        assert!(idle.idle_for < Duration::from_secs(1));
    }
}

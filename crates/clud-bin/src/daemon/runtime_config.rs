use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

pub(super) const ENV_TEST_MODE: &str = "CLUD_DAEMON_TEST_MODE";
pub(super) const ENV_TEST_MAX_LIFETIME_SECS: &str = "CLUD_DAEMON_TEST_MAX_LIFETIME_SECS";
pub(super) const ENV_TEST_IDLE_TIMEOUT_SECS: &str = "CLUD_DAEMON_TEST_IDLE_TIMEOUT_SECS";
pub(super) const ENV_TEST_HOST_SCANS: &str = "CLUD_DAEMON_TEST_HOST_SCANS";

const DEFAULT_TEST_MAX_LIFETIME_SECS: u64 = 300;
const DEFAULT_TEST_IDLE_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DaemonRuntimeConfig {
    pub test_mode: bool,
    pub test_max_lifetime: Option<Duration>,
    pub test_idle_timeout: Option<Duration>,
    pub host_scans_enabled: bool,
    pub periodic_maintenance_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TestExpiryReason {
    Idle,
    MaxLifetime,
}

#[derive(Clone)]
pub(super) struct TestRuntimeActivity {
    last_activity: Arc<Mutex<Instant>>,
    active_requests: Arc<AtomicUsize>,
}

impl TestRuntimeActivity {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            last_activity: Arc::new(Mutex::new(now)),
            active_requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(super) fn note_activity(&self, now: Instant) {
        *self
            .last_activity
            .lock()
            .expect("test runtime activity mutex poisoned") = now;
    }

    pub(super) fn start_request(&self) -> TestRuntimeRequestGuard {
        self.note_activity(Instant::now());
        self.active_requests.fetch_add(1, Ordering::AcqRel);
        TestRuntimeRequestGuard {
            activity: self.clone(),
        }
    }

    pub(super) fn snapshot(&self, now: Instant) -> (Duration, usize) {
        let last = *self
            .last_activity
            .lock()
            .expect("test runtime activity mutex poisoned");
        (
            now.saturating_duration_since(last),
            self.active_requests.load(Ordering::Acquire),
        )
    }
}

pub(super) struct TestRuntimeRequestGuard {
    activity: TestRuntimeActivity,
}

impl Drop for TestRuntimeRequestGuard {
    fn drop(&mut self) {
        self.activity.note_activity(Instant::now());
        self.activity.active_requests.fetch_sub(1, Ordering::AcqRel);
    }
}

impl DaemonRuntimeConfig {
    pub(super) fn from_env() -> Self {
        Self::from_raw(
            std::env::var(ENV_TEST_MODE).ok().as_deref(),
            std::env::var(ENV_TEST_MAX_LIFETIME_SECS).ok().as_deref(),
            std::env::var(ENV_TEST_IDLE_TIMEOUT_SECS).ok().as_deref(),
            std::env::var(ENV_TEST_HOST_SCANS).ok().as_deref(),
        )
    }

    fn from_raw(
        test_mode: Option<&str>,
        max_lifetime_secs: Option<&str>,
        idle_timeout_secs: Option<&str>,
        host_scans: Option<&str>,
    ) -> Self {
        let test_mode = test_mode.is_some_and(|value| value.trim() == "1");
        if !test_mode {
            return Self {
                test_mode: false,
                test_max_lifetime: None,
                test_idle_timeout: None,
                host_scans_enabled: true,
                periodic_maintenance_enabled: true,
            };
        }

        Self {
            test_mode: true,
            test_max_lifetime: Some(Duration::from_secs(parse_positive_secs(
                max_lifetime_secs,
                DEFAULT_TEST_MAX_LIFETIME_SECS,
            ))),
            test_idle_timeout: Some(Duration::from_secs(parse_positive_secs(
                idle_timeout_secs,
                DEFAULT_TEST_IDLE_TIMEOUT_SECS,
            ))),
            host_scans_enabled: host_scans.is_some_and(|value| value.trim() == "1"),
            periodic_maintenance_enabled: false,
        }
    }

    pub(super) fn test_expiry_reason(
        self,
        uptime: Duration,
        idle_for: Duration,
        worker_count: usize,
        active_requests: usize,
    ) -> Option<TestExpiryReason> {
        if self
            .test_max_lifetime
            .is_some_and(|maximum| uptime >= maximum)
        {
            return Some(TestExpiryReason::MaxLifetime);
        }
        if worker_count == 0
            && active_requests == 0
            && self
                .test_idle_timeout
                .is_some_and(|timeout| idle_for >= timeout)
        {
            return Some(TestExpiryReason::Idle);
        }
        None
    }
}

fn parse_positive_secs(raw: Option<&str>, default: u64) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_defaults_are_unchanged_and_ignore_test_overrides() {
        let config = DaemonRuntimeConfig::from_raw(None, Some("2"), Some("3"), Some("0"));
        assert!(!config.test_mode);
        assert_eq!(config.test_max_lifetime, None);
        assert_eq!(config.test_idle_timeout, None);
        assert!(config.host_scans_enabled);
        assert!(config.periodic_maintenance_enabled);
    }

    #[test]
    fn test_mode_has_bounded_scanner_free_defaults() {
        let config = DaemonRuntimeConfig::from_raw(Some("1"), None, None, None);
        assert!(config.test_mode);
        assert_eq!(config.test_max_lifetime, Some(Duration::from_secs(300)));
        assert_eq!(config.test_idle_timeout, Some(Duration::from_secs(30)));
        assert!(!config.host_scans_enabled);
        assert!(!config.periodic_maintenance_enabled);
    }

    #[test]
    fn test_mode_accepts_positive_overrides_and_host_scan_opt_in() {
        let config = DaemonRuntimeConfig::from_raw(Some("1"), Some("4"), Some("2"), Some("1"));
        assert_eq!(config.test_max_lifetime, Some(Duration::from_secs(4)));
        assert_eq!(config.test_idle_timeout, Some(Duration::from_secs(2)));
        assert!(config.host_scans_enabled);
    }

    #[test]
    fn invalid_or_zero_durations_fall_back_to_safe_defaults() {
        let config =
            DaemonRuntimeConfig::from_raw(Some("1"), Some("invalid"), Some("0"), Some("true"));
        assert_eq!(config.test_max_lifetime, Some(Duration::from_secs(300)));
        assert_eq!(config.test_idle_timeout, Some(Duration::from_secs(30)));
        assert!(!config.host_scans_enabled);
    }

    #[test]
    fn hard_max_wins_even_with_live_workers() {
        let config = DaemonRuntimeConfig::from_raw(Some("1"), Some("2"), Some("1"), None);
        assert_eq!(
            config.test_expiry_reason(Duration::from_secs(2), Duration::ZERO, 1, 1),
            Some(TestExpiryReason::MaxLifetime)
        );
    }

    #[test]
    fn idle_expiry_waits_for_workers_to_disappear() {
        let config = DaemonRuntimeConfig::from_raw(Some("1"), Some("20"), Some("2"), None);
        assert_eq!(
            config.test_expiry_reason(Duration::from_secs(3), Duration::from_secs(3), 1, 0),
            None
        );
        assert_eq!(
            config.test_expiry_reason(Duration::from_secs(3), Duration::from_secs(2), 0, 0),
            Some(TestExpiryReason::Idle)
        );
    }

    #[test]
    fn readiness_starts_a_full_idle_window_without_extending_hard_lifetime() {
        let config = DaemonRuntimeConfig::from_raw(Some("1"), Some("20"), Some("2"), None);
        let started = Instant::now();
        let activity = TestRuntimeActivity::new(started);
        let ready = started + Duration::from_secs(3);
        activity.note_activity(ready);

        for (uptime, expected) in [
            (3, None),
            (4, None),
            (5, Some(TestExpiryReason::Idle)),
            (20, Some(TestExpiryReason::MaxLifetime)),
        ] {
            let (idle_for, requests) = activity.snapshot(started + Duration::from_secs(uptime));
            assert_eq!(
                config.test_expiry_reason(Duration::from_secs(uptime), idle_for, 0, requests),
                expected,
            );
        }
    }

    #[test]
    fn active_request_blocks_idle_but_not_hard_expiry() {
        let config = DaemonRuntimeConfig::from_raw(Some("1"), Some("10"), Some("2"), None);
        assert_eq!(
            config.test_expiry_reason(Duration::from_secs(3), Duration::from_secs(3), 0, 1),
            None
        );
        assert_eq!(
            config.test_expiry_reason(Duration::from_secs(10), Duration::ZERO, 0, 1),
            Some(TestExpiryReason::MaxLifetime)
        );
    }
}

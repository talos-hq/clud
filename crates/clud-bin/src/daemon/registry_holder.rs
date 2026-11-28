//! #2142: a new daemon that cannot open `data.redb` because a stale clud
//! daemon still holds its lock asks that daemon to stop, then retries the open
//! for a bounded time. Without this the new daemon ran with no GC until the
//! operator killed the old pid by hand (#2095 only made it keep retrying).

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use running_process::broker::backend_sdk::read_daemon_identity_file;
use sysinfo::Signal;

use crate::gc::{GcError, Registry};
use crate::process_identity::ProcessIdentity;

use super::client::request_daemon_shutdown;
use super::io_helpers::read_json_file;
use super::paths::daemon_info_path;
use super::process_utils::{identity_is_alive, signal_process_as};
use super::rp_broker::daemon_identity_path;
use super::types::DaemonInfo;

/// How long a new daemon waits for an evicted holder to release the lock.
pub(super) const EVICT_WAIT: Duration = Duration::from_secs(12);
const POLL: Duration = Duration::from_millis(100);
/// Slack between the sidecar's recorded start and the OS-reported start.
const START_SLACK_SECS: u64 = 5;

/// Whether `err` means another process holds the database lock.
pub(super) fn is_lock_error(err: &GcError) -> bool {
    let text = match err {
        GcError::Io(text) | GcError::Sql(text) => text.to_lowercase(),
        GcError::NoDefaultPath => return false,
    };
    text.contains("already open")
        || text.contains("cannot acquire lock")
        || text.contains("used by another process")
}

/// Open the registry; on a lock failure evict the holder with `evict` and
/// retry until `wait` elapses. Returns the last error when it never opens.
pub(super) fn open_after_eviction<O, E>(
    mut open: O,
    evict: E,
    wait: Duration,
) -> Result<Registry, GcError>
where
    O: FnMut() -> Result<Registry, GcError>,
    E: FnOnce(),
{
    let mut last = match open() {
        Ok(registry) => return Ok(registry),
        Err(err) if is_lock_error(&err) => err,
        Err(err) => return Err(err),
    };
    evict();
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        thread::sleep(POLL);
        match open() {
            Ok(registry) => return Ok(registry),
            Err(err) => last = err,
        }
    }
    Err(last)
}

/// Side effects of [`evict_stale_holders`], injectable so tests can assert
/// which pids are signalled without touching real processes.
struct EvictOps<'a> {
    request_shutdown: &'a dyn Fn(&Path),
    observe: &'a dyn Fn(u32) -> Option<ProcessIdentity>,
    /// Single-process signal (never the tree); `true` when delivered.
    signal: &'a dyn Fn(&ProcessIdentity, Signal) -> bool,
    is_alive: &'a dyn Fn(&ProcessIdentity) -> bool,
    own_pid: u32,
    /// Whether the host can deliver a graceful terminate (false on Windows).
    graceful_terminate: bool,
    grace: Duration,
}

/// Ask every other recorded clud daemon (`daemon.json`, then the frame-lane
/// identity sidecar) to stop. Best effort: nothing here is fatal.
///
/// Only the holder pid is ever signalled, never its process tree: the holder's
/// own shutdown handles its children, and a tree kill would take down the
/// stale daemon's live session workers. On Windows `Signal::Term` is
/// unsupported, so there is no graceful signal and the forced kill is skipped
/// entirely; eviction relies on the graceful `request_daemon_shutdown` alone.
pub(super) fn evict_stale_holders(state_dir: &Path) {
    evict_with(
        state_dir,
        &EvictOps {
            request_shutdown: &|dir| {
                let _ = request_daemon_shutdown(dir);
            },
            observe: &ProcessIdentity::observe,
            signal: &signal_process_as,
            is_alive: &identity_is_alive,
            own_pid: std::process::id(),
            graceful_terminate: !crate::platform::host::WINDOWS,
            grace: Duration::from_secs(3),
        },
    );
}

fn evict_with(state_dir: &Path, ops: &EvictOps<'_>) {
    let own = ops.own_pid;
    let recorded = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir)).ok();
    let recorded_pid = recorded.as_ref().map(|info| info.pid);
    if recorded.is_some_and(|info| info.pid != own) {
        eprintln!("[clud] data.redb is locked; asking the previous daemon to stop");
        (ops.request_shutdown)(state_dir);
    }
    let Some(sidecar) = read_daemon_identity_file(&daemon_identity_path(state_dir)) else {
        return;
    };
    if sidecar.pid == own || Some(sidecar.pid) == recorded_pid || !ops.graceful_terminate {
        return;
    }
    let Some(observed) = (ops.observe)(sidecar.pid) else {
        return;
    };
    // A recycled pid must not be signalled: its start has to match the
    // daemon the sidecar describes.
    let recorded_start = sidecar.started_at_unix_ms / 1000;
    if observed.start_time.abs_diff(recorded_start) > START_SLACK_SECS {
        return;
    }
    if !(ops.signal)(&observed, Signal::Term) {
        return;
    }
    let deadline = Instant::now() + ops.grace;
    while (ops.is_alive)(&observed) && Instant::now() < deadline {
        thread::sleep(POLL);
    }
    if (ops.is_alive)(&observed) {
        (ops.signal)(&observed, Signal::Kill);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn lock_error() -> GcError {
        GcError::Sql("Database already open. Cannot acquire lock.".to_string())
    }

    #[test]
    fn lock_errors_are_recognised_and_others_are_not() {
        assert!(is_lock_error(&lock_error()));
        assert!(!is_lock_error(&GcError::Sql("corrupt".to_string())));
        assert!(!is_lock_error(&GcError::NoDefaultPath));
    }

    #[test]
    fn lock_failure_evicts_then_retries_until_the_lock_clears() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = tmp.path().join("data.redb");
        let evicted = Cell::new(false);
        let calls = Cell::new(0u32);
        let result = open_after_eviction(
            || {
                calls.set(calls.get() + 1);
                if evicted.get() && calls.get() > 2 {
                    Registry::open_at(&path)
                } else {
                    Err(lock_error())
                }
            },
            || evicted.set(true),
            Duration::from_secs(5),
        );
        assert!(result.is_ok());
        assert!(evicted.get());
        assert!(calls.get() >= 3);
    }

    #[test]
    fn non_lock_failure_does_not_evict() {
        let evicted = Cell::new(false);
        let result = open_after_eviction(
            || Err(GcError::Sql("corrupt".to_string())),
            || evicted.set(true),
            Duration::from_secs(1),
        );
        assert!(result.is_err());
        assert!(!evicted.get());
    }

    #[test]
    fn eviction_gives_up_after_the_bounded_wait() {
        let started = Instant::now();
        let result = open_after_eviction(|| Err(lock_error()), || {}, Duration::from_millis(300));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    fn sidecar_for(pid: u32, started_at_unix_ms: u64, state_dir: &Path) {
        use running_process::broker::backend_handle::DaemonProcess;
        use running_process::broker::backend_sdk::write_daemon_identity_file;
        let endpoint =
            super::super::rp_broker::endpoint_for_state_dir(state_dir).expect("endpoint");
        let mut daemon = DaemonProcess::current_process(endpoint, None).expect("identity");
        daemon.pid = pid;
        daemon.started_at_unix_ms = started_at_unix_ms;
        write_daemon_identity_file(&daemon_identity_path(state_dir), &daemon).expect("sidecar");
    }

    /// Run `evict_with` over a sidecar for `holder_pid`; return (shutdown
    /// calls, signals sent).
    fn run_evict(
        holder_pid: u32,
        sidecar_start_secs: u64,
        observed_start: u64,
        own_pid: u32,
        graceful: bool,
    ) -> (u32, Vec<(u32, bool)>) {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        sidecar_for(holder_pid, sidecar_start_secs * 1000, tmp.path());
        let shutdowns = Cell::new(0u32);
        let signals = std::cell::RefCell::new(Vec::new());
        let request_shutdown = |_: &Path| shutdowns.set(shutdowns.get() + 1);
        let observe = |pid: u32| Some(ProcessIdentity::new(pid, observed_start));
        let signal = |id: &ProcessIdentity, sig: Signal| {
            signals
                .borrow_mut()
                .push((id.pid, matches!(sig, Signal::Kill)));
            true
        };
        let is_alive = |_: &ProcessIdentity| true;
        evict_with(
            tmp.path(),
            &EvictOps {
                request_shutdown: &request_shutdown,
                observe: &observe,
                signal: &signal,
                is_alive: &is_alive,
                own_pid,
                graceful_terminate: graceful,
                grace: Duration::from_millis(150),
            },
        );
        (shutdowns.get(), signals.into_inner())
    }

    #[test]
    fn eviction_signals_only_the_holder_pid_term_then_kill() {
        let (_, signals) = run_evict(4242, 1_000_000, 1_000_000, 1, true);
        assert_eq!(signals, vec![(4242, false), (4242, true)]);
    }

    #[test]
    fn eviction_never_signals_its_own_pid() {
        let (_, signals) = run_evict(77, 1_000_000, 1_000_000, 77, true);
        assert!(signals.is_empty());
    }

    #[test]
    fn eviction_spares_a_holder_whose_start_time_mismatches() {
        let (_, signals) = run_evict(4242, 1_000_000, 2_000_000, 1, true);
        assert!(signals.is_empty());
    }

    #[test]
    fn eviction_without_graceful_terminate_issues_no_signal_at_all() {
        let (_, signals) = run_evict(4242, 1_000_000, 1_000_000, 1, false);
        assert!(signals.is_empty());
    }
}

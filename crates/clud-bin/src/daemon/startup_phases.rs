//! Per-phase startup events for `run_daemon` (#2333).
//!
//! Between `daemon_starting` and `daemon_started` the daemon does several
//! steps that can be slow or fail. [`StartupPhases::mark`] appends one
//! `daemon_startup_phase` event per finished step (`phase`, `elapsed_ms` since
//! start) and [`StartupPhases::fail`] records `daemon_start_failed` plus the
//! stderr line in `daemon.stderr.log`, so the cause survives a detached
//! daemon whose stderr is `discarded. Both reuse the existing event append.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::json;

use super::daemon_events;

const STDERR_LOG_NAME: &str = "daemon.stderr.log";
const MAX_STDERR_LOG_BYTES: u64 = 256 * 1024;

/// Set once fd 2 / `STD_ERROR_HANDLE` points at `daemon.stderr.log`. From then
/// on anything written to stderr already lands in the log, so the explicit
/// appends in `fail` and the panic hook would record every line twice.
static STDERR_REDIRECTED: AtomicBool = AtomicBool::new(false);

pub(super) struct StartupPhases<'a> {
    state_dir: &'a Path,
    started: Instant,
}

impl<'a> StartupPhases<'a> {
    pub(super) fn new(state_dir: &'a Path, started: Instant) -> Self {
        Self { state_dir, started }
    }

    /// Record that `phase` just finished.
    pub(super) fn mark(&self, phase: &'static str) {
        daemon_events::log_event(
            self.state_dir,
            "daemon_startup_phase",
            [
                ("phase", json!(phase)),
                ("elapsed_ms", json!(elapsed_ms(self.started))),
            ],
        );
    }

    /// Report an early exit: stderr, `daemon.stderr.log`, and a
    /// `daemon_start_failed` event. Returns the exit code (1).
    pub(super) fn fail(&self, phase: &'static str, error: &str) -> i32 {
        let message = format!("[clud] {error}");
        eprintln!("{message}");
        if !STDERR_REDIRECTED.load(Ordering::SeqCst) {
            append_stderr_log(&self.state_dir.join(STDERR_LOG_NAME), &message);
        }
        daemon_events::log_event(
            self.state_dir,
            "daemon_start_failed",
            [
                ("phase", json!(phase)),
                ("elapsed_ms", json!(elapsed_ms(self.started))),
                ("error", json!(error)),
            ],
        );
        1
    }
}

/// Chain a panic hook that appends the panic to `daemon.stderr.log` (#2386),
/// then calls the previous hook. The detached daemon's stderr is discarded, so
/// without this a panic leaves no trace outside the crash reporter. Installs at
/// most once per process; call it only from the real daemon entry point.
pub(super) fn install_panic_hook(state_dir: &Path) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    let path = state_dir.join(STDERR_LOG_NAME);
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let backtrace = std::backtrace::Backtrace::capture();
            let report = panic_report(thread.name(), info, &backtrace);
            // Once redirected, `previous` (the default hook) writes to the log
            // through stderr; appending here too would duplicate the panic.
            if !STDERR_REDIRECTED.load(Ordering::SeqCst) {
                append_stderr_log(&path, &report);
            }
            previous(info);
        }));
    });
}

/// Send the detached daemon's own stderr to `daemon.stderr.log` (#2386), so
/// stray non-panic output is kept instead of discarded. Call it at startup.
pub(super) fn redirect_stderr_to_log(state_dir: &Path) {
    rotate_and_redirect(state_dir);
}

/// Redirect-aware rotation. If the live log is past the cap, roll it to the
/// `.1` backup, open a fresh `daemon.stderr.log` and point stderr at it again,
/// so fd 2 never follows the backup or an unlinked inode. When stderr is
/// already redirected and the log is under the cap this is one `metadata()`
/// call, so the daemon's periodic tasks can call it freely. A failure is
/// reported once and never stops the daemon.
pub(super) fn rotate_and_redirect(state_dir: &Path) {
    let path = state_dir.join(STDERR_LOG_NAME);
    let redirected = STDERR_REDIRECTED.load(Ordering::SeqCst);
    if redirected && std::fs::metadata(&path).is_ok_and(|meta| meta.len() < MAX_STDERR_LOG_BYTES) {
        return;
    }
    let _ = daemon_events::rotate_if_needed(&path, MAX_STDERR_LOG_BYTES);
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|file| crate::platform::stdio::redirect_stderr_to(&file));
    match result {
        Ok(()) => STDERR_REDIRECTED.store(true, Ordering::SeqCst),
        Err(error) => eprintln!("[clud] daemon stderr redirect to {path:?} failed: {error}"),
    }
}

/// Cheap periodic hook: keep a redirected log bounded. No-op before the
/// redirect succeeded (the append path rotates in that case).
pub(super) fn rotate_redirected_log(state_dir: &Path) {
    if STDERR_REDIRECTED.load(Ordering::SeqCst) {
        rotate_and_redirect(state_dir);
    }
}

/// Pure formatter for the panic log entry (message + location come from `info`).
fn panic_report(
    thread: Option<&str>,
    info: &dyn std::fmt::Display,
    backtrace: &dyn std::fmt::Display,
) -> String {
    format!(
        "[clud] daemon panic in thread '{}': {info}\n{backtrace}",
        thread.unwrap_or("<unnamed>")
    )
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Append one line, rolling to a single `.1` backup past the size cap.
fn append_stderr_log(path: &Path, message: &str) {
    let _ = daemon_events::rotate_if_needed(path, MAX_STDERR_LOG_BYTES);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::paths::daemon_events_path;
    use std::fs;

    fn events(dir: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(daemon_events_path(dir))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn phases_are_logged_in_order_with_monotonic_elapsed() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let phases = StartupPhases::new(tmp.path(), Instant::now());
        for name in ["a", "b", "c"] {
            phases.mark(name);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let got = events(tmp.path());
        let names: Vec<_> = got.iter().map(|e| e["phase"].as_str().unwrap()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        let ms: Vec<u64> = got
            .iter()
            .map(|e| e["elapsed_ms"].as_u64().unwrap())
            .collect();
        assert!(ms.windows(2).all(|w| w[0] <= w[1]), "{ms:?}");
        assert!(got.iter().all(|e| e["op"] == "daemon_startup_phase"));
    }

    #[test]
    fn fail_records_event_and_stderr_log() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let phases = StartupPhases::new(tmp.path(), Instant::now());
        assert_eq!(phases.fail("loopback_bind", "boom"), 1);
        let got = events(tmp.path());
        assert_eq!(got[0]["op"], "daemon_start_failed");
        assert_eq!(got[0]["phase"], "loopback_bind");
        assert_eq!(got[0]["error"], "boom");
        let log = fs::read_to_string(tmp.path().join(STDERR_LOG_NAME)).unwrap();
        assert_eq!(log, "[clud] boom\n");
    }

    #[test]
    fn panic_report_keeps_message_location_and_thread_when_appended() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = tmp.path().join(STDERR_LOG_NAME);
        let info = "panicked at src/x.rs:1:2:\ndaemon-boom-2386";
        append_stderr_log(&path, &panic_report(Some("worker"), &info, &"<bt>"));
        let log = fs::read_to_string(&path).unwrap();
        assert!(log.contains("daemon-boom-2386"), "{log}");
        assert!(log.contains("src/x.rs:1:2"), "{log}");
        assert!(log.contains("daemon panic in thread 'worker'"), "{log}");
        assert!(log.contains("<bt>"), "{log}");
        assert!(panic_report(None, &"m", &"").contains("<unnamed>"));
    }

    #[test]
    fn stderr_log_rolls_past_the_cap() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = tmp.path().join(STDERR_LOG_NAME);
        fs::write(&path, vec![b'x'; MAX_STDERR_LOG_BYTES as usize]).unwrap();
        append_stderr_log(&path, "fresh");
        assert_eq!(fs::read_to_string(&path).unwrap(), "fresh\n");
        assert!(tmp.path().join("daemon.stderr.log.1").exists());
    }
}

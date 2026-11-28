//! #1186: `clud __daemon` is started through running-process's daemon spawn,
//! so it inherits nothing from its launcher except the environment it asked
//! for.
//!
//! Two properties are pinned here, and both need a **real** daemon started by
//! the **real** `clud` binary, because what they check is the detach itself:
//!
//! 1. **No leaked descriptors (Unix,
//!    `platform/unix/daemon_spawn_hygiene.rs`).** A launcher that holds a pipe without
//!    `FD_CLOEXEC` — a CI wrapper, an IDE host, a Python harness with
//!    inheritable fds — must not have that pipe's write end pinned open by the
//!    long-lived daemon. If it is, the reader never sees EOF. This is the Unix
//!    twin of the #37 Windows handle leak; the hand-rolled trampoline closed
//!    the Windows side and never swept Unix fds.
//! 2. **Declared, untagged.** The daemon carries `RUNNING_PROCESS_IS_DAEMON`
//!    (the cooperative spare signal DD-021/DD-023 rely on) and does **not**
//!    carry the launching session's `RUNNING_PROCESS_ORIGINATOR` tag, which a
//!    lazily started daemon would otherwise inherit from whichever `clud`
//!    subcommand happened to start it (#683).
//!
//! Raw `std::process::Command` is intentional and is why this file is in
//! `ci/banned_imports.py`'s exempt set: `NativeProcess` sweeps every fd above
//! 2 in the child, which would remove the very descriptor test 1 hands to
//! `clud`, making it pass whether or not the daemon leaks.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::exe;

fn clud() -> PathBuf {
    exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
}

/// `clud daemon <verb>` against an isolated state dir in daemon test mode:
/// no host-wide scans, and a bounded lifetime so a daemon this test fails to
/// stop still exits on its own.
pub(crate) fn daemon_command(state_dir: &Path, verb: &str) -> Command {
    let mut command = Command::new(clud());
    command
        .args(["daemon", verb])
        .env("CLUD_DAEMON_STATE_DIR", state_dir)
        .env("CLUD_DAEMON_TEST_MODE", "1")
        .env("CLUD_DAEMON_TEST_IDLE_TIMEOUT_SECS", "20")
        .env("CLUD_DAEMON_TEST_MAX_LIFETIME_SECS", "120")
        .env_remove("CLUD_DAEMON_TEST_HOST_SCANS")
        .env_remove(running_process::ORIGINATOR_ENV_VAR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub(crate) fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn read_daemon_pid(state_dir: &Path) -> u32 {
    let path = state_dir.join("daemon.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(pid) = value.get("pid").and_then(serde_json::Value::as_u64) {
                    return pid as u32;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Stops the test's daemon on every exit path, including a failed assertion.
pub(crate) struct DaemonGuard {
    pub(crate) state_dir: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = daemon_command(&self.state_dir, "stop").output();
    }
}

#[test]
fn daemon_declares_itself_and_drops_the_launchers_originator_tag() {
    let _serial = super::REAPER_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let scratch = kernal_api::platform::fs::TemporaryDirectory::new().expect("scratch dir");
    let state_dir = scratch.path().join("state");

    // Stand in for a `clud` subcommand run from inside an agent session: this
    // test process is the live originator.
    let session_tag = format!("CLUD:{}", std::process::id());
    let mut command = daemon_command(&state_dir, "restart");
    command.env(running_process::ORIGINATOR_ENV_VAR, &session_tag);
    let _guard = DaemonGuard {
        state_dir: state_dir.clone(),
    };
    let output = command.output().expect("run clud daemon restart");
    assert_success(&output, "clud daemon restart");
    let daemon_pid = read_daemon_pid(&state_dir);

    let scan = clud::process_scan::scan_env("CLUD");
    let tagged = scan
        .tagged
        .iter()
        .find(|process| process.pid == daemon_pid)
        .map(|process| process.originator.clone());

    assert!(
        scan.declared_daemons.contains(&daemon_pid),
        "daemon pid {daemon_pid} does not carry {}",
        running_process::DAEMON_MARKER_ENV_VAR
    );
    assert_eq!(
        tagged, None,
        "daemon pid {daemon_pid} inherited the launching session's originator tag"
    );
}

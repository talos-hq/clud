//! #333: the Windows runtime-cache hop must **relay**, not **contain**.
//!
//! Windows has no `execv`, so `runtime_cache::reexec_from_cached_binary` has
//! to spawn the cached `clud.exe` and wait for it. That wrapper is only
//! acceptable if it is otherwise invisible: whatever the relayed clud starts
//! must live exactly as long as it would have without the hop.
//!
//! The hop originally spawned through `NativeProcess`, which wraps every
//! Windows child in a `KILL_ON_JOB_CLOSE` Job Object. Job membership is
//! inherited, so the relayed clud's *own* children joined that job too — and
//! the `__daemon` the trampoline started at the time did not request
//! `CREATE_BREAKAWAY_FROM_JOB` (it does now, through running-process's daemon
//! spawn, #1186). Closing the relay's job handle on exit
//! therefore killed the daemon it had just started, which is why
//! `CLUD_USE_RUNTIME_CACHE=1` failed 31 integration tests on Windows with a
//! `daemon.json` naming a dead PID.
//!
//! Only a real Job Object and a real detach can show this, so it is an
//! integration test rather than a unit one. It uses
//! [`daemon-stub`](../../../testbins/daemon-stub/src/main.rs)'s
//! `spawn-detached` mode, which reproduces exactly that shape: detach with
//! `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, no breakaway, no daemon
//! marker.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::exe;

/// `daemon-stub` lives in `testbins/`, so this crate has no
/// `CARGO_BIN_EXE_daemon-stub`; `common/exe.rs` owns the bundle-vs-local
/// precedence the exec runners need.
fn daemon_stub() -> PathBuf {
    let stub = exe::sibling_bin_path("daemon-stub");
    assert!(
        stub.is_file(),
        "daemon-stub not found at {}; `soldr cargo build -p daemon-stub` \
         (on an exec runner, CLUD_TEST_BIN_DIR should point at the bundle's bin dir)",
        stub.display()
    );
    stub
}

fn read_pid_file(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    let pid = text.trim().parse::<u32>().ok()?;
    (pid != 0).then_some(pid)
}

fn wait_for_pid_file(path: &Path) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(pid) = read_pid_file(path) {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn pid_is_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    let Ok(handle) = (unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    }) else {
        return false;
    };
    let alive = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe {
        let _ = CloseHandle(handle);
    }
    alive
}

/// The regression guard for the #333 Windows hop defect.
///
/// The relayed process detaches a daemon and exits. Once the relay has
/// returned *and* dropped everything it owns, that daemon must still be
/// running: the hop is not allowed to own a lifetime that outranks it.
#[test]
fn a_relayed_child_can_leave_a_detached_daemon_running_behind_it() {
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let pid_path = temp.path().join("detached.pid");

    let code = clud::trampoline::relay_child_and_wait(
        &daemon_stub(),
        &[
            std::ffi::OsString::from("spawn-detached"),
            pid_path.clone().into_os_string(),
        ],
    )
    .expect("relay daemon-stub");
    assert_eq!(code, 0, "daemon-stub spawn-detached should exit cleanly");

    // A `KILL_ON_JOB_CLOSE` teardown fires as soon as the relay's job handle
    // closes, which is before this line. Give it room to land either way so a
    // pass means survival rather than a lucky read.
    let pid = wait_for_pid_file(&pid_path);
    std::thread::sleep(Duration::from_millis(750));

    let survived = pid.is_some_and(pid_is_alive);
    if let Some(pid) = pid {
        clud::process_tree::kill_tree(pid);
    }

    assert!(
        survived,
        "the detached daemon started by the relayed child is gone (pid file: {pid:?}). \
         The runtime-cache hop must not wrap its child in a KILL_ON_JOB_CLOSE Job \
         Object — job membership is inherited, so closing it kills the daemon the \
         relayed clud just detached. See #333."
    );
}

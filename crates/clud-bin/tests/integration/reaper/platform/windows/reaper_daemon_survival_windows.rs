//! Tier 2 of #674: long-lived daemons survive a clud session.
//!
//! **Almost all reaper coverage belongs in Tier 1** — table-driven unit tests
//! over synthetic process graphs and injected `ProcessFacts`, in
//! `job_orphan_reaper`'s `lifecycle_tests`. They run on every platform CI
//! builds for, need no spawning, and finish in microseconds.
//!
//! What is left here is only what cannot be faked: a **real** Job Object, a
//! **real** breakaway, and a **real** detached process that owns a **real**
//! listening socket. Four tests, deliberately. Anything expressible against
//! injected facts must not be added to this file.
//!
//! The stub is [`daemon-stub`](../../../testbins/daemon-stub/src/main.rs), not
//! a real sccache/docker/soldr install: depending on those being present would
//! make the suite non-hermetic, and the reaper keys on *signal shape*, which is
//! exactly what the stub reproduces.
//!
//! Raw `std::process::Command` is intentional and is why this file is in
//! `ci/banned_imports.py`'s exempt set: wrapping these fixtures in
//! `NativeProcess` would attach another Job Object and set the very daemon
//! marker whose absence the sccache-shaped case is about.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clud::job_orphan_reaper::ForegroundJobTracker;

use crate::exe;

/// `daemon-stub` belongs to `testbins/`, so this crate has no
/// `CARGO_BIN_EXE_daemon-stub` to fall back on — see `common/exe.rs` for the
/// bundle-vs-local precedence.
///
/// The "one level up from `deps/`" guess this used to make is correct only for
/// a local `cargo test`. CI compiles harnesses on Linux and runs them from
/// `bundle/tests/`, where that path resolves to nothing — which is why this
/// suite has been failing on `main` since it landed.
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

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                if pid != 0 {
                    return pid;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon-stub pid at {}",
            path.display()
        );
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

/// Drive one full tool-shell lifecycle: a registered `cmd.exe` agent runs a
/// PowerShell tool root, which starts the stub in `mode` and exits. The exit is
/// the trigger the reaper acts on.
///
/// Returns the served daemon's PID.
fn run_session_that_starts_a_daemon(
    tracker: &ForegroundJobTracker,
    temp: &Path,
    mode: &str,
    label: &str,
) -> u32 {
    let pid_path = temp.join(format!("{label}.pid"));
    let script = format!(
        "& '{stub}' {mode} '{pid_path}'",
        stub = daemon_stub().display(),
        pid_path = pid_path.display(),
    );
    let mut agent = Command::new("cmd.exe")
        .args([
            "/d",
            "/c",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fake agent");
    tracker.register_backend(agent.id(), "cmd.exe");
    assert!(agent.wait().expect("wait fake agent").success());

    let pid = wait_for_pid_file(&pid_path);
    // Give the reaper several completion-port quiet periods to act. If it is
    // going to kill this daemon, it does so here.
    std::thread::sleep(Duration::from_millis(1_500));
    pid
}

fn kill_stub(pid: u32) {
    clud::process_tree::kill_tree(pid);
}

/// Every archetype in one session, because installing a Job Object per test
/// would contend for the same process tree and the assertion is the same
/// shape for all three: the daemon is still alive after the tool shell that
/// started it has exited.
///
/// | Stub mode | Reproduces | Protected by |
/// |---|---|---|
/// | `spawn-breakaway` | anything using `spawn_daemon_breaking_away_from_job` | job-object membership |
/// | `spawn-marked` | zccache, soldr — inside the job, marker set | the cooperative marker |
/// | `spawn-detached` | **sccache**, `FBuildWorker`, language servers | listening-endpoint ownership |
#[test]
fn daemons_started_inside_a_session_survive_the_tool_shell_that_started_them() {
    let tracker = ForegroundJobTracker::install().expect("install foreground Job tracker");
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");

    for (mode, label, why) in [
        (
            "spawn-breakaway",
            "breakaway",
            "a daemon that broke away from our Job Object was never ours to kill",
        ),
        (
            "spawn-marked",
            "marked",
            "the cooperative marker is what keeps zccache/soldr alive; this is the \
             load-bearing case for it",
        ),
        (
            // The row that had no protection at all before #673: no marker, no
            // breakaway, just its own detach and a listening socket.
            "spawn-detached",
            "detached",
            "sccache never calls running-process, so marker absence must not be \
             read as permission to kill",
        ),
    ] {
        let pid = run_session_that_starts_a_daemon(&tracker, temp.path(), mode, label);
        let survived = pid_is_alive(pid);
        kill_stub(pid);
        assert!(survived, "{mode}: {why}");
    }
}

/// The counterweight. If sparing were over-broad the suite above would pass
/// while the reaper had quietly stopped working, so a genuinely leaked client
/// — no marker, no breakaway, no listener — must still be reaped by the same
/// tracker in the same session.
#[test]
fn a_genuinely_leaked_client_is_still_reaped() {
    let tracker = ForegroundJobTracker::install().expect("install foreground Job tracker");
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let pid_path = temp.path().join("leaked.pid");

    // `ping` is an ordinary long-running non-shell client: it declares nothing,
    // detaches from nothing, and listens on nothing.
    let script = format!(
        "$p = Start-Process -FilePath 'ping.exe' -ArgumentList '-n','60','127.0.0.1' \
         -PassThru -WindowStyle Hidden; Set-Content -LiteralPath '{pid_path}' -Value $p.Id",
        pid_path = pid_path.display(),
    );
    let mut agent = Command::new("cmd.exe")
        .args([
            "/d",
            "/c",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fake agent");
    tracker.register_backend(agent.id(), "cmd.exe");
    assert!(agent.wait().expect("wait fake agent").success());

    let leaked_pid = wait_for_pid_file(&pid_path);
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_is_alive(leaked_pid) {
        assert!(
            Instant::now() < deadline,
            "a leaked client with no daemon signal must still be reaped; \
             over-sparing has disabled the reaper"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Session A's tool-shell exits must never touch session B's descendants.
///
/// Two trackers means two Job Objects, and B's daemon is in B's job only.
/// This is the property that makes running six concurrent cluds safe, and it
/// cannot be expressed against injected facts — the job membership is the
/// thing under test.
#[test]
fn one_sessions_exit_does_not_reap_another_sessions_daemon() {
    let session_a = ForegroundJobTracker::install().expect("install tracker A");
    let session_b = ForegroundJobTracker::install().expect("install tracker B");
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");

    let b_pid = run_session_that_starts_a_daemon(&session_b, temp.path(), "spawn-marked", "b");
    // Now churn session A: a full tool-shell lifecycle of its own.
    let a_pid = run_session_that_starts_a_daemon(&session_a, temp.path(), "spawn-detached", "a");

    let b_survived = pid_is_alive(b_pid);
    let a_survived = pid_is_alive(a_pid);
    kill_stub(a_pid);
    kill_stub(b_pid);

    assert!(
        b_survived,
        "session A's reaper reached into session B's process tree"
    );
    assert!(a_survived, "session A reaped its own detached daemon");
}

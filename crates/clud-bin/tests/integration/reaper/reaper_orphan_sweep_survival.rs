//! Tier 2 of #674, completing the lifecycle matrix: the **cross-platform**
//! reaper's three destructive entry points (#688).
//!
//! [`reaper_daemon_survival_windows`](./platform/windows/reaper_daemon_survival_windows.rs)
//! covers the Windows tool-shell reaper — the one whose blast radius is a
//! single `cmd.exe` subtree. This file covers [`clud::orphan_reaper`], which
//! runs on **every** foreground `clud` exit, on `clud slay`, and on the
//! daemon's periodic sweep, on every platform. Until #688 that reaper spared
//! by the cooperative `RUNNING_PROCESS_IS_DAEMON` marker alone, so an
//! sccache-shaped daemon survived the tool shell that started it (#681) and was
//! then killed moments later by clud's own exit.
//!
//! **Almost all reaper coverage belongs in Tier 1** — table-driven unit tests
//! over injected `ProcessFacts`, in `orphan_reaper`'s and `job_orphan_reaper`'s
//! test modules. Three tests live here, and only because what they exercise
//! cannot be faked: a **real** detached process that owns a **real** listening
//! socket, found through a **real** full-host environment scan.
//!
//! ## Why these sweeps cannot hurt the machine running them
//!
//! `clud slay` and the daemon sweep reap *every* CLUD-tagged orphan on the
//! host, which on a developer's workstation may include another session's
//! work. The tests therefore drive [`clud::orphan_reaper::reap_orphans_filtered`]
//! with an admission closure narrowed to this test's own stub PIDs. The
//! admission hook runs *before* `report_and_reap`, so the code under test —
//! scan, spare-list, classification, kill — is bit-for-bit what `clud slay`
//! executes; only the candidate set is narrowed.
//!
//! Raw `std::process::Command` is intentional and is why this file is in
//! `ci/banned_imports.py`'s exempt set: a `NativeProcess` would set the very
//! `RUNNING_PROCESS_IS_DAEMON` marker whose absence the sccache-shaped case is
//! about, and attach containment that masks the detach.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clud::orphan_reaper::{self, ReapOpts};
use clud::reaper_facts::SpareReason;

use crate::exe;

/// Long enough for a sweep to run and for a doomed process to actually die,
/// short enough that a hang fails the test rather than the job.
const SETTLE: Duration = Duration::from_millis(400);

/// `daemon-stub` belongs to `testbins/`, so this crate has no
/// `CARGO_BIN_EXE_daemon-stub` to fall back on — see `common/exe.rs` for the
/// bundle-vs-local precedence.
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
    clud::process_identity::ProcessIdentity::observe(pid).is_some()
}

/// A PID that is certainly not running, which is what makes a tag look
/// *abandoned* to the sweep — the state `clud slay` and the daemon heartbeat
/// select on.
///
/// Deliberately NOT the PID of a just-dead throwaway process: on a busy
/// runner that PID is recycled within moments by a long-lived process, and
/// `reap_orphans_from_scan` then reads the tagged child as "originator still
/// alive" and drops it from the candidate set forever ("candidates=[]", the
/// #994 reaper flake). The constant sits above Linux's `pid_max` (4,194,304)
/// so no process can ever hold it; `orphan_reap` uses the same value for the
/// same reason and is stable on every CI lane.
pub(super) fn a_dead_originator_pid() -> u32 {
    99_999_999
}

/// Start a `daemon-stub` in the given mode, tagged as a descendant of
/// `originator`, and return the served daemon's PID.
///
/// The tag is the whole point: `RUNNING_PROCESS_ORIGINATOR=CLUD:<pid>` is
/// inherited by everything an agent spawns, and sccache-class daemons never
/// strip it, because they never call `running-process` at all. That is what
/// puts them in the reaper's candidate set with nothing protecting them.
fn start_tagged_stub(temp: &Path, label: &str, mode: &str, originator: u32) -> u32 {
    let pid_path = temp.join(format!("{label}.pid"));
    let mut launcher = Command::new(daemon_stub());
    launcher
        .arg(mode)
        .arg(&pid_path)
        .env(
            running_process::ORIGINATOR_ENV_VAR,
            format!("CLUD:{originator}"),
        )
        .env_remove(running_process::DAEMON_MARKER_ENV_VAR)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = launcher.spawn().expect("spawn daemon-stub launcher");
    let _ = child.wait();
    wait_for_pid_file(&pid_path)
}

pub(super) fn quiet() -> ReapOpts {
    ReapOpts {
        quiet: true,
        ..ReapOpts::default()
    }
}

/// A full-host environment scan can race a just-spawned child: under CI load
/// the child's row is missing from the first pass ("candidates=[]", the #994
/// reaper flake). Retry the sweep until the PID is observed or the deadline
/// passes. Test-only robustness — production reap semantics are unchanged.
pub(super) fn sweep_until_selected(
    pid: u32,
    mut sweep: impl FnMut() -> orphan_reaper::ReapOutcome,
) -> orphan_reaper::ReapOutcome {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut outcome = sweep();
    while !outcome.candidate_pids.contains(&pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        outcome = sweep();
    }
    outcome
}

/// #688's headline claim, against the sweep that `clud slay` and the daemon's
/// periodic heartbeat both run.
///
/// The stub reproduces the **signal shape** of sccache / `FBuildWorker` /
/// a language server: tagged, undeclared, detached on its own (`setsid` on
/// Unix, `DETACHED_PROCESS` on Windows), owning a listening socket. Depending
/// on a real sccache install would make the suite non-hermetic, and the reaper
/// keys on the shape, not the name.
///
/// The assertion is on the **reason**, not on survival alone: a sweep that
/// never saw the process at all also leaves it running, and would pass a
/// survival-only test while the protection did not exist.
#[test]
fn a_tagged_undeclared_listening_daemon_survives_the_orphan_sweep() {
    let _guard = super::REAPER_TEST_LOCK
        .lock()
        .expect("reaper test lock poisoned");
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let originator = a_dead_originator_pid();
    let stub = start_tagged_stub(temp.path(), "sweep", "spawn-detached", originator);

    let outcome = sweep_until_selected(stub, || {
        orphan_reaper::reap_orphans_filtered(&quiet(), &mut |pid| pid == stub)
    });
    std::thread::sleep(SETTLE);
    let survived = pid_is_alive(stub);
    clud::process_tree::kill_tree(stub);

    assert!(
        outcome.candidate_pids.contains(&stub),
        "the sweep never selected the stub, so this proves nothing; \
         candidates={:?}",
        outcome.candidate_pids
    );
    let spared = outcome.spared.iter().find(|s| s.pid == stub);
    let reason = spared.map(|s| s.reason);
    // #1146: a spare line is the reaper reporting a deliberate decision, so it
    // has to say what it left alive. A bare PID is the one thing a reader
    // cannot look up afterwards — the process is, by construction, one they
    // did not know was running.
    assert!(
        spared.is_some_and(|s| !s.label.is_empty()),
        "the spared entry must carry a shape label: {:?}",
        outcome.spared
    );
    assert!(
        matches!(
            reason,
            Some(SpareReason::ListeningEndpoint) | Some(SpareReason::SessionLeader)
        ),
        "an sccache-shaped daemon must be spared by an OS signal, not by luck; \
         got {reason:?}"
    );
    assert!(survived, "the sweep killed a daemon it claimed to spare");
}

/// The same claim against the *other* entry point: the scan every foreground
/// `clud` runs as it exits. It selects on "originator is me" rather than
/// "originator is dead", so it is a genuinely different candidate set and was
/// separately unprotected.
#[test]
fn a_tagged_undeclared_listening_daemon_survives_the_on_exit_scan() {
    let _guard = super::REAPER_TEST_LOCK
        .lock()
        .expect("reaper test lock poisoned");
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    // This test process stands in for the exiting clud, so the tag points at
    // it and only this test's own stubs can be selected.
    let me = std::process::id();
    let stub = start_tagged_stub(temp.path(), "exit", "spawn-detached", me);

    let outcome = sweep_until_selected(stub, || orphan_reaper::scan_and_report(me, &quiet()));
    std::thread::sleep(SETTLE);
    let survived = pid_is_alive(stub);
    clud::process_tree::kill_tree(stub);

    assert!(
        outcome.candidate_pids.contains(&stub),
        "the on-exit scan never selected the stub; candidates={:?}",
        outcome.candidate_pids
    );
    assert!(
        outcome.spared.iter().any(|s| s.pid == stub),
        "the on-exit scan reaped an sccache-shaped daemon"
    );
    assert!(survived, "the on-exit scan killed a spared daemon");
}

/// The counterweight, and the reason the signal table is ranked rather than
/// permissive. If sparing were over-broad the two tests above would pass while
/// the reaper had quietly stopped working, so a genuinely leaked orphan — same
/// tag, same dead originator, no marker, no detach, no listener — must still be
/// reaped by the same sweep.
#[test]
fn a_leaked_orphan_with_no_daemon_signal_is_still_reaped() {
    let _guard = super::REAPER_TEST_LOCK
        .lock()
        .expect("reaper test lock poisoned");
    let originator = a_dead_originator_pid();
    // An ordinary long-running client: declares nothing, detaches from nothing,
    // listens on nothing.
    let mut leaked = sleeper()
        .env(
            running_process::ORIGINATOR_ENV_VAR,
            format!("CLUD:{originator}"),
        )
        .env_remove(running_process::DAEMON_MARKER_ENV_VAR)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn leaked client");
    let pid = leaked.id();
    // Let it appear in a full-host environment scan.
    std::thread::sleep(SETTLE);

    let outcome = sweep_until_selected(pid, || {
        orphan_reaper::reap_orphans_filtered(&quiet(), &mut |candidate| candidate == pid)
    });
    assert!(
        outcome.candidate_pids.contains(&pid),
        "the sweep never selected the leaked client; candidates={:?}\n\
         diagnostics: child_alive={} running_process_sees={:?} kernel_environ={}",
        outcome.candidate_pids,
        pid_is_alive(pid),
        running_process::originator::find_processes_by_originator("CLUD")
            .iter()
            .map(|p| p.pid)
            .collect::<Vec<_>>(),
        leaked_environ_text(pid),
    );
    assert!(
        outcome.spared.is_empty(),
        "nothing about a leaked client deserves protection: {:?}",
        outcome.spared
    );

    // `try_wait`, not a process-table lookup: this one is our *direct* child,
    // so once it is SIGKILLed it becomes a zombie until we reap it — and a
    // zombie is still a row in the process table. `pid_is_alive` would spin
    // here until the deadline and report a reaper failure that did not happen.
    // The two survival tests above look up the PID instead because their stub
    // is a *grand*child whose launcher has exited, so init reaps it for us.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match leaked.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(err) => panic!("try_wait on the leaked client failed: {err}"),
        }
        if Instant::now() >= deadline {
            let _ = leaked.kill();
            let _ = leaked.wait();
            panic!(
                "a leaked orphan with no daemon signal must still be reaped; \
                 over-sparing has disabled the reaper"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// What the kernel itself exposes for the child's environ (Linux). This
/// distinguishes "the tag never made it onto the child" from "the scanner
/// cannot read it" when the sweep cannot see the leaked client on CI.
fn leaked_environ_text(pid: u32) -> String {
    if clud::platform::host::LINUX {
        std::fs::read(format!("/proc/{pid}/environ"))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|error| format!("unreadable: {error}"))
    } else {
        String::from("n/a (not linux)")
    }
}

fn sleeper() -> Command {
    // A repo-built child, deliberately not coreutils `sleep`: on GitHub
    // runners a PATH shim can re-exec a host binary with a scrubbed
    // environment, which made the tagged child invisible to the full-host
    // environ scan ("candidates=[]" across ten seconds of retries, the
    // #994 reaper flake). `mock-agent` is the child shape `orphan_reap`
    // already uses and is proven visible on the exec runners.
    let mut command = Command::new(exe::sibling_bin_path("mock-agent"));
    command.args(["--mock-sleep-ms", "60000"]);
    command
}

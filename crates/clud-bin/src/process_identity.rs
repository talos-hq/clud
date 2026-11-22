//! Process identity = PID **plus** the OS-reported start time.
//!
//! A bare numeric PID is not a stable handle to a process. Windows can hand
//! the same PID to an unrelated process moments after the original exits, and
//! Unix wraps `pid_max` under load. Every clud path that stores a PID and acts
//! on it later — killing a session tree, deciding a session is live, sampling
//! CPU — is therefore exposed to attributing (or terminating) a completely
//! unrelated process.
//!
//! The #550 idle-CPU harness reproduced exactly this on a busy Windows
//! workstation: a detached session PID was observed at t0 and represented a
//! different active process by t1.
//!
//! The rule this module encodes: **record the start time alongside the PID,
//! and re-check it before acting.** A missing PID or a changed start time both
//! mean the original process is gone; the replacement is never touched.
//!
//! ## Granularity, and why that is acceptable
//!
//! [`sysinfo::Process::start_time`] reports whole seconds since the UNIX
//! epoch, so two processes that both hold PID *N* within the same second are
//! indistinguishable here. That residual window is orders of magnitude
//! narrower than the unguarded one (which spans the entire lifetime of the
//! recorded PID).
//!
//! ## Fallback for records written before this existed
//!
//! Session snapshots persisted by an older clud carry no start time. Those
//! compare by PID alone — identical to today's behaviour — rather than being
//! treated as universally stale, which would make an upgrade silently stop
//! cleaning up the sessions already on disk. [`UNKNOWN_START_TIME`] marks that
//! case explicitly so a reader can tell "not recorded" from "recorded as 0".

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessStatus, System};

/// Sentinel for "no start time was recorded / none could be read".
///
/// Real start times are seconds since the UNIX epoch, so `0` cannot collide
/// with a genuine value for any process on a machine whose clock is set.
pub const UNKNOWN_START_TIME: u64 = 0;

/// A PID paired with the start time of the process that held it when the PID
/// was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Seconds since the UNIX epoch, or [`UNKNOWN_START_TIME`].
    pub start_time: u64,
}

impl ProcessIdentity {
    pub fn new(pid: u32, start_time: u64) -> Self {
        Self { pid, start_time }
    }

    /// An identity with no start time — compares by PID alone.
    pub fn pid_only(pid: u32) -> Self {
        Self::new(pid, UNKNOWN_START_TIME)
    }

    /// Whether a usable start time was recorded.
    pub fn has_start_time(&self) -> bool {
        self.start_time != UNKNOWN_START_TIME
    }

    /// Is `observed` — a process seen on the host *now* — the same process
    /// this identity was recorded from?
    ///
    /// PIDs must match. Start times must match too, unless either side is
    /// [`UNKNOWN_START_TIME`], in which case this degrades to the PID-only
    /// comparison clud used before start times were recorded (see the module
    /// docs on why that fallback is permissive rather than strict).
    pub fn matches(&self, observed: &ProcessIdentity) -> bool {
        if self.pid != observed.pid {
            return false;
        }
        if !self.has_start_time() || !observed.has_start_time() {
            return true;
        }
        self.start_time == observed.start_time
    }

    /// Read the identity of `pid` out of an already-refreshed snapshot.
    ///
    /// `None` means the PID is absent or its process has terminated (including
    /// an unreaped Unix zombie, which retains its PID and start time).
    pub fn observe_in(system: &System, pid: u32) -> Option<Self> {
        system
            .process(Pid::from_u32(pid))
            .filter(|process| {
                !matches!(
                    process.status(),
                    ProcessStatus::Zombie | ProcessStatus::Dead
                )
            })
            .map(|process| Self::new(pid, process.start_time()))
    }

    /// Read the identity of `pid` from the live process table.
    ///
    /// Windows opens one read-only process handle and reads its creation
    /// time directly; other platforms refresh only the requested PID.
    /// Neither path enumerates the host process table.
    pub fn observe(pid: u32) -> Option<Self> {
        observe_process(pid)
    }

    /// Is the process this identity names still running?
    ///
    /// `false` when the PID is gone **or** when it has been recycled onto a
    /// different process. Callers about to terminate something must gate on
    /// this rather than on a bare PID lookup.
    pub fn is_live(&self) -> bool {
        match Self::observe(self.pid) {
            Some(observed) => self.matches(&observed),
            None => false,
        }
    }

    /// Has the OS released this identity's process entry?
    ///
    /// Stricter than `!is_live()`, which turns false as soon as sysinfo stops
    /// reporting a running process. On macOS that happens while the kernel is
    /// still tearing the process down: the PID and start time remain visible
    /// to `kinfo_proc` readers (psutil) until the entry is reaped (#1995).
    /// Released means the PID no longer answers a liveness probe at all, or
    /// it now belongs to a process with another start time.
    pub fn released(&self) -> bool {
        if !crate::platform::session_registry::os_probe_is_alive(self.pid) {
            return true;
        }
        Self::observe(self.pid).is_some_and(|observed| !self.matches(&observed))
    }
}

/// Creation time of the live process `pid` in 100 ns FILETIME ticks.
///
/// Sub-second precision for questions whole seconds cannot answer, such as
/// whether a process started before the one now holding its parent PID
/// (`process_tree::children_index`, #1738). `None` if the process is gone or
/// cannot be queried. Windows only: FILETIME ticks do not exist on POSIX, so
/// it is always `None` there.
pub fn creation_ticks(pid: u32) -> Option<u64> {
    crate::platform::process_identity::creation_ticks(pid)
}

/// The native creation time written into launcher tags and foreground rows.
pub(crate) fn origin_start(pid: u32, seconds: u64) -> Option<u64> {
    crate::platform::process_identity::origin_start(pid, seconds)
}

fn observe_process(pid: u32) -> Option<ProcessIdentity> {
    crate::platform::process_identity::observe_process(pid)
}

/// Start time of the calling process, or [`UNKNOWN_START_TIME`] if the OS
/// would not report one.
///
/// Used at record time so a session snapshot pins the worker that wrote it.
pub fn self_start_time() -> u64 {
    ProcessIdentity::observe(std::process::id())
        .map(|identity| identity.start_time)
        .unwrap_or(UNKNOWN_START_TIME)
}

/// Start time of `pid` as seen right now, or [`UNKNOWN_START_TIME`] if the PID
/// is already gone.
pub fn start_time_of(pid: u32) -> u64 {
    ProcessIdentity::observe(pid)
        .map(|identity| identity.start_time)
        .unwrap_or(UNKNOWN_START_TIME)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every case below uses synthetic identities. Forcing the OS to actually
    // recycle a PID is not something a test can do deterministically, so the
    // comparison is exercised directly -- which is the whole reason it is a
    // pure function separated from the process-table lookup.

    #[test]
    fn same_pid_and_start_time_is_the_same_process() {
        let recorded = ProcessIdentity::new(4321, 1_700_000_000);
        let observed = ProcessIdentity::new(4321, 1_700_000_000);
        assert!(recorded.matches(&observed));
        assert!(observed.matches(&recorded));
    }

    #[test]
    fn recycled_pid_is_rejected() {
        let recorded = ProcessIdentity::new(4321, 1_700_000_000);
        // Same PID, started later: the recorded process exited and Windows
        // handed the number to something else.
        let replacement = ProcessIdentity::new(4321, 1_700_000_042);
        assert!(!recorded.matches(&replacement));
        assert!(!replacement.matches(&recorded));
    }

    #[test]
    fn a_start_time_that_moved_backwards_is_also_rejected() {
        // Direction is irrelevant -- any difference means a different
        // process.
        let recorded = ProcessIdentity::new(4321, 1_700_000_042);
        let other = ProcessIdentity::new(4321, 1_700_000_000);
        assert!(!recorded.matches(&other));
    }

    /// #1995: a running process is not released; the same PID under
    /// another start time (a recycled PID) is.
    #[test]
    fn only_a_gone_or_recycled_identity_is_released() {
        let own = ProcessIdentity::new(std::process::id(), self_start_time());
        assert!(own.has_start_time());
        assert!(!own.released(), "this test process still holds its entry");
        let recycled = ProcessIdentity::new(std::process::id(), self_start_time() + 7);
        assert!(recycled.released());
    }

    #[test]
    fn different_pids_never_match() {
        let a = ProcessIdentity::new(1, 1_700_000_000);
        let b = ProcessIdentity::new(2, 1_700_000_000);
        assert!(!a.matches(&b));
        // ...not even when neither side recorded a start time.
        assert!(!ProcessIdentity::pid_only(1).matches(&ProcessIdentity::pid_only(2)));
    }

    #[test]
    fn a_missing_start_time_falls_back_to_pid_only() {
        // A snapshot written by a clud that predates this module.
        let legacy = ProcessIdentity::pid_only(4321);
        let observed = ProcessIdentity::new(4321, 1_700_000_000);
        assert!(legacy.matches(&observed));
        // ...and symmetrically, when the OS refuses to report a start time
        // for the live process.
        assert!(observed.matches(&legacy));
        assert!(!legacy.has_start_time());
    }

    #[test]
    fn observing_this_process_yields_a_matching_identity() {
        let pid = std::process::id();
        let identity = ProcessIdentity::observe(pid).expect("this process is running");
        assert_eq!(identity.pid, pid);
        assert!(identity.is_live());
        assert_eq!(identity.start_time, self_start_time());
    }

    #[test]
    fn a_stale_start_time_makes_a_live_pid_read_as_dead() {
        // The PID exists, so a bare `pid_is_alive` would say "live" and a
        // caller would go on to kill it. The identity check must not.
        let pid = std::process::id();
        let real = ProcessIdentity::observe(pid).expect("this process is running");
        let impostor = ProcessIdentity::new(pid, real.start_time.wrapping_add(1));
        assert!(!impostor.is_live());
    }

    #[test]
    fn an_exited_process_is_dead_even_while_its_handle_remains_open() {
        use std::io::Write;

        let mut child = if crate::platform::host::WINDOWS {
            std::process::Command::new("cmd")
                .args(["/C", "set /p token= & exit /b 0"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("spawn short-lived Windows child")
        } else {
            std::process::Command::new("sh")
                .args(["-c", "read token; exit 0"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("spawn short-lived Unix child")
        };

        let identity =
            ProcessIdentity::observe(child.id()).expect("child identity before waiting for exit");
        child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
        assert!(child.wait().expect("wait for child").success());
        assert!(
            !identity.is_live(),
            "a terminated process must be dead before its process handle is dropped"
        );
    }
}

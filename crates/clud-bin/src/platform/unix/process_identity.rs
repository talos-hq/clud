//! Unix half of `crate::process_identity`: a single-PID sysinfo refresh.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::process_identity::ProcessIdentity;

/// FILETIME creation ticks are a Windows concept; POSIX has none, so this is
/// always `None`. Only Windows callers consult it.
pub fn creation_ticks(_pid: u32) -> Option<u64> {
    None
}

pub fn observe_process(pid: u32) -> Option<ProcessIdentity> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    ProcessIdentity::observe_in(&system, pid)
}

pub(crate) fn orphaned_parent(system: &System, process: &sysinfo::Process) -> bool {
    process.parent().is_none_or(|parent| {
        parent.as_u32() == 1 || ProcessIdentity::observe_in(system, parent.as_u32()).is_none()
    })
}

pub(crate) fn process_group(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    // SAFETY: getpgid takes a PID by value and does not signal it.
    let group = unsafe { libc::getpgid(pid) };
    (group > 0).then_some(group as u32)
}

pub(crate) fn origin_start(_pid: u32, seconds: u64) -> Option<u64> {
    (seconds != 0).then_some(seconds)
}

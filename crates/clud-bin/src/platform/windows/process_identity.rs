//! Windows half of `crate::process_identity`: creation times read straight
//! from one read-only process handle.

use crate::process_identity::ProcessIdentity;

/// Creation time of the live process `pid` in 100 ns FILETIME ticks.
///
/// Sub-second precision for questions whole seconds cannot answer, such as
/// whether a process started before the one now holding its parent PID
/// (`process_tree::children_index`, #1738). `None` if the process is gone or
/// cannot be queried.
pub fn creation_ticks(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const STILL_ACTIVE: u32 = 259;

    if pid == 0 {
        return None;
    }

    // SAFETY: the handle is opened read-only for one exact PID, all four
    // FILETIME out-pointers remain valid for the call, and the handle is
    // closed before returning.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let mut exit_code = 0;
    let exit_result = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    let times_result =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    let _ = unsafe { CloseHandle(handle) };
    exit_result.ok()?;
    times_result.ok()?;
    if exit_code != STILL_ACTIVE {
        return None;
    }
    Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

pub fn observe_process(pid: u32) -> Option<ProcessIdentity> {
    // FILETIME is measured in 100ns ticks since 1601-01-01. sysinfo exposes
    // process start times as whole seconds since the Unix epoch, so preserve
    // that contract for identities already persisted on disk.
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const WINDOWS_TO_UNIX_EPOCH_SECS: u64 = 11_644_473_600;

    let creation_ticks = creation_ticks(pid)?;
    let start_time = (creation_ticks / TICKS_PER_SECOND).checked_sub(WINDOWS_TO_UNIX_EPOCH_SECS)?;
    Some(ProcessIdentity::new(pid, start_time))
}

pub(crate) fn orphaned_parent(system: &sysinfo::System, process: &sysinfo::Process) -> bool {
    process.parent().is_none_or(|parent| {
        ProcessIdentity::observe_in(system, parent.as_u32()).is_none_or(|identity| {
            identity.start_time > process.start_time()
                || (identity.start_time == process.start_time()
                    && creation_ticks(parent.as_u32())
                        .zip(creation_ticks(process.pid().as_u32()))
                        .is_some_and(|(parent, child)| parent > child))
        })
    })
}

pub(crate) fn process_group(_pid: u32) -> Option<u32> {
    // Worker Job Objects already contain descendants on Windows.
    None
}

pub(crate) fn origin_start(pid: u32, _seconds: u64) -> Option<u64> {
    creation_ticks(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_identity_uses_native_creation_ticks() {
        let pid = std::process::id();
        let ticks = creation_ticks(pid).unwrap();
        assert_eq!(origin_start(pid, 1), Some(ticks));
    }
}

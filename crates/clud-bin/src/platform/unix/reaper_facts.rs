//! Unix producer for `crate::reaper_facts::collect_host_facts`.

use crate::reaper_facts::{FactsSnapshot, Signal};

pub fn mark_unavailable(snapshot: &mut FactsSnapshot) {
    // No Windows services session or desktop shell anywhere on Unix.
    snapshot.unavailable.insert(Signal::ServiceSession);
    snapshot.unavailable.insert(Signal::ActiveDesktopShell);
    #[cfg(not(target_os = "linux"))]
    {
        // `/proc` is the only interface here that answers these without
        // linking a platform-specific process-inspection library. On
        // non-Linux Unix the honest answer is "cannot evaluate".
        snapshot.unavailable.insert(Signal::TokenOwner);
        snapshot.unavailable.insert(Signal::ListeningEndpoint);
    }
}

pub fn fill(candidates: &[u32], snapshot: &mut FactsSnapshot) {
    mark_unavailable(snapshot);

    // A separate session is evidence of deliberate detachment only for
    // the process that leads it. Ordinary workers also inherit the session
    // of a PTY or wrapper whose leader can outlive their clud invocation.
    let own_session = unsafe { libc::getsid(0) };
    if own_session >= 0 {
        for &pid in candidates {
            let session = unsafe { libc::getsid(pid as libc::pid_t) };
            if session > 0 && session != own_session && session as u32 == pid {
                snapshot.session_leaders.insert(pid);
            }
        }
    }

    #[cfg(target_os = "linux")]
    linux::fill(candidates, snapshot);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;

    use super::FactsSnapshot;

    pub(super) fn fill(candidates: &[u32], snapshot: &mut FactsSnapshot) {
        let own_euid = unsafe { libc::geteuid() };
        for &pid in candidates {
            if let Some(euid) = effective_uid(pid) {
                if euid != own_euid {
                    snapshot.foreign_owner.insert(pid);
                }
            }
        }

        let survivors: Vec<u32> = candidates
            .iter()
            .copied()
            .filter(|pid| !snapshot.foreign_owner.contains(pid))
            .collect();
        if survivors.is_empty() {
            return;
        }

        let listening = listening_inodes();
        if listening.is_empty() {
            return;
        }
        snapshot.listening = survivors
            .into_iter()
            .filter(|pid| owns_any_socket(*pid, &listening))
            .collect();
    }

    /// Effective uid from `/proc/<pid>/status`'s `Uid:` line, whose four
    /// fields are real / effective / saved-set / filesystem.
    fn effective_uid(pid: u32) -> Option<u32> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let line = status.lines().find(|line| line.starts_with("Uid:"))?;
        line.split_whitespace().nth(2)?.parse().ok()
    }

    /// Inodes of every listening socket on the host: TCP over both address
    /// families, plus unix-domain sockets (the POSIX analogue of a Windows
    /// named pipe, and how a language server or build cache is typically
    /// reached).
    fn listening_inodes() -> HashSet<u64> {
        let mut inodes = HashSet::new();
        for family in ["/proc/net/tcp", "/proc/net/tcp6"] {
            collect_tcp_listeners(family, &mut inodes);
        }
        collect_unix_listeners(&mut inodes);
        inodes
    }

    /// `/proc/net/tcp{,6}` columns: `sl local rem st ... uid timeout inode`.
    /// State `0A` is `TCP_LISTEN`.
    fn collect_tcp_listeners(path: &str, out: &mut HashSet<u64>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() > 9 && fields[3] == "0A" {
                if let Ok(inode) = fields[9].parse::<u64>() {
                    out.insert(inode);
                }
            }
        }
    }

    /// `/proc/net/unix` columns end with `... st inode path`. `st` is `01`
    /// for a listening (unconnected, accepting) socket.
    fn collect_unix_listeners(out: &mut HashSet<u64>) {
        let Ok(text) = std::fs::read_to_string("/proc/net/unix") else {
            return;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() > 6 && fields[5] == "01" {
                if let Ok(inode) = fields[6].parse::<u64>() {
                    out.insert(inode);
                }
            }
        }
    }

    /// Does `pid` hold a descriptor onto any of `listening`?
    ///
    /// Bounded by the candidate set, not the host: we read one process's
    /// `fd` directory per candidate, never the whole `/proc` tree.
    fn owns_any_socket(pid: u32, listening: &HashSet<u64>) -> bool {
        let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            return false;
        };
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(inode) = target
                .to_str()
                .and_then(|link| link.strip_prefix("socket:["))
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|digits| digits.parse::<u64>().ok())
            else {
                continue;
            };
            if listening.contains(&inode) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::reaper_facts::{collect_host_facts, ProcessFacts};

    /// Our own process is in our own session, so it must not be mistaken for a
    /// detached daemon — the signal has to distinguish, not blanket-spare.
    #[test]
    fn our_own_process_is_not_a_session_leader_by_this_signal() {
        let facts = collect_host_facts(&[std::process::id()], &HashSet::new(), Vec::new());
        assert_eq!(facts.is_session_leader(std::process::id()), Some(false));
    }
}

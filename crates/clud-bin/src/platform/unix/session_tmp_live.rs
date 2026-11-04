//! Recognize protected desktop infrastructure without trusting argv[0] alone.
//!
//! Linux intentionally hides cwd/environ for privilege-bearing desktop
//! services. Only the verified systemd user-manager lineage and its known
//! desktop service scopes qualify; every other unreadable process remains
//! unknown to the temp collector.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, Debug)]
struct Process {
    parent: u32,
    comm: String,
    cgroup: String,
    command: Vec<String>,
}

fn read_process(pid: u32) -> Option<Process> {
    let root = std::path::PathBuf::from(format!("/proc/{pid}"));
    let status = fs::read_to_string(root.join("status")).ok()?;
    let parent = status.lines().find_map(|line| {
        line.strip_prefix("PPid:")
            .and_then(|s| s.trim().parse().ok())
    })?;
    let cgroup = fs::read_to_string(root.join("cgroup"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .to_string();
    let command = fs::read(root.join("cmdline"))
        .ok()?
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| std::str::from_utf8(s).map(str::to_string))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(Process {
        parent,
        comm: fs::read_to_string(root.join("comm"))
            .ok()?
            .trim()
            .to_string(),
        cgroup,
        command,
    })
}

fn trusted_program(command: &str) -> bool {
    let path = Path::new(command);
    path.is_absolute()
        && fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.uid() == 0 && meta.mode() & 0o022 == 0)
}

fn program_matches(process: &Process, program: &str, comm: &[&str]) -> bool {
    comm.contains(&process.comm.as_str())
        && process.command.first().is_some_and(|command| {
            Path::new(command)
                .file_name()
                .is_some_and(|name| name == program)
        })
}

fn manager(process: &Process, scope: &str, trusted: &impl Fn(&str) -> bool) -> bool {
    process.parent == 1
        && process.cgroup == format!("{scope}/init.scope")
        && program_matches(process, "systemd", &["systemd"])
        && process.command.iter().any(|arg| arg == "--user")
        && process.command.first().is_some_and(|path| trusted(path))
}

fn verified_service(
    process: &Process,
    uid: u32,
    read: &impl Fn(u32) -> Option<Process>,
    trusted: &impl Fn(&str) -> bool,
) -> bool {
    let scope = format!("/user.slice/user-{uid}.slice/user@{uid}.service");
    if !read(1).is_some_and(|init| init.comm == "systemd" && init.parent == 0) {
        return false;
    }
    if manager(process, &scope, trusted) {
        return true;
    }
    let Some(parent) = read(process.parent) else {
        return false;
    };
    if process.comm == "(sd-pam)"
        && process.command == ["(sd-pam)"]
        && process.cgroup == format!("{scope}/init.scope")
    {
        return manager(&parent, &scope, trusted);
    }
    let compositor = process.cgroup == format!("{scope}/session.slice/plasma-kwin_wayland.service")
        && program_matches(
            process,
            "kwin_wayland",
            &["kwin_wayland", ".kwin_wayland-w"],
        );
    let policy_agent = process.cgroup
        == format!("{scope}/background.slice/plasma-polkit-agent.service")
        && program_matches(
            process,
            "polkit-kde-authentication-agent-1",
            &[
                "polkit-kde-auth",
                "polkit-kde-authentication-agent-1",
                ".polkit-kde-aut",
            ],
        );
    if !(compositor || policy_agent) || !process.command.first().is_some_and(|path| trusted(path)) {
        return false;
    }
    if manager(&parent, &scope, trusted) {
        return true;
    }
    // NixOS starts KWin through its root-owned wrapper in the same service.
    compositor
        && parent.cgroup == process.cgroup
        && program_matches(
            &parent,
            "kwin_wayland_wrapper",
            &["kwin_wayland_w", ".kwin_wayland_w"],
        )
        && parent.command.first().is_some_and(|path| trusted(path))
        && read(parent.parent)
            .is_some_and(|manager_process| manager(&manager_process, &scope, trusted))
}

pub fn verified_desktop_service(pid: u32) -> bool {
    let Some(uid) = super::rm_tool::effective_uid() else {
        return false;
    };
    read_process(pid)
        .is_some_and(|process| verified_service(&process, uid, &read_process, &trusted_program))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_process() -> Process {
        Process {
            parent: 1,
            comm: "systemd".into(),
            cgroup: "/user.slice/user-1000.slice/user@1000.service/init.scope".into(),
            command: vec!["/usr/lib/systemd/systemd".into(), "--user".into()],
        }
    }

    fn service() -> Process {
        Process {
            parent: 10,
            comm: ".kwin_wayland-w".into(),
            cgroup: "/user.slice/user-1000.slice/user@1000.service/session.slice/plasma-kwin_wayland.service".into(),
            command: vec!["/nix/store/system-kwin/bin/kwin_wayland".into()],
        }
    }

    #[test]
    fn only_a_verified_desktop_lineage_is_exempt() {
        let read = |pid| match pid {
            10 => Some(manager_process()),
            1 => Some(Process {
                parent: 0,
                ..manager_process()
            }),
            _ => None,
        };
        let process = service();
        assert!(verified_service(&process, 1000, &read, &|_| true));
        assert!(!verified_service(&process, 1000, &read, &|_| false));
        let mut fake = process.clone();
        fake.comm = "claude".into();
        assert!(!verified_service(&fake, 1000, &read, &|_| true));
        fake = process.clone();
        fake.cgroup = "/user.slice/app-claude.scope".into();
        assert!(!verified_service(&fake, 1000, &read, &|_| true));
        fake = process.clone();
        fake.command[0] = "/usr/bin/claude".into();
        assert!(!verified_service(&fake, 1000, &read, &|_| true));
        assert!(!verified_service(&process, 1000, &|_| None, &|_| true));
        assert!(!verified_service(&process, 1001, &read, &|_| true));
        let fake_manager = |_: u32| {
            let mut manager = manager_process();
            manager.parent = 20;
            Some(manager)
        };
        assert!(!verified_service(&process, 1000, &fake_manager, &|_| true));
    }

    #[test]
    fn manager_pam_and_nix_wrapper_require_their_actual_scope_and_parent() {
        let init = Process {
            parent: 0,
            comm: "systemd".into(),
            cgroup: "/init.scope".into(),
            command: vec!["/usr/lib/systemd/systemd".into()],
        };
        let read = |pid| match pid {
            1 => Some(init.clone()),
            10 => Some(manager_process()),
            11 => Some(Process {
                parent: 10,
                comm: ".kwin_wayland_w".into(),
                cgroup: service().cgroup,
                command: vec!["/nix/store/system-kwin/bin/kwin_wayland_wrapper".into()],
            }),
            _ => None,
        };
        assert!(verified_service(&manager_process(), 1000, &read, &|_| true));
        let mut pam = manager_process();
        pam.parent = 10;
        pam.comm = "(sd-pam)".into();
        pam.command = vec!["(sd-pam)".into()];
        assert!(verified_service(&pam, 1000, &read, &|_| true));
        pam.parent = 42;
        assert!(!verified_service(&pam, 1000, &read, &|_| true));
        let mut compositor = service();
        compositor.parent = 11;
        assert!(verified_service(&compositor, 1000, &read, &|_| true));
        assert!(!verified_service(&compositor, 1000, &read, &|path| !path
            .ends_with("_wrapper")));
        let policy = Process {
            parent: 10,
            comm: ".polkit-kde-aut".into(),
            cgroup: "/user.slice/user-1000.slice/user@1000.service/background.slice/plasma-polkit-agent.service".into(),
            command: vec!["/usr/libexec/polkit-kde-authentication-agent-1".into()],
        };
        assert!(verified_service(&policy, 1000, &read, &|_| true));
    }
}

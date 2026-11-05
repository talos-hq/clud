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
        && root_controlled_path(path, &mut 40, &inspect_entry)
        && fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
}

struct PathEntry {
    controlled: bool,
    link_target: Option<std::path::PathBuf>,
}

fn inspect_entry(path: &Path) -> Option<PathEntry> {
    let meta = fs::symlink_metadata(path).ok()?;
    let controlled = meta.uid() == 0
        && (meta.file_type().is_symlink()
            || meta.mode() & 0o022 == 0
            // Sticky root-owned directories cannot have their existing
            // root-owned children replaced by another user (e.g. /nix/store).
            || (meta.is_dir() && meta.mode() & 0o1000 != 0));
    let link_target = if meta.file_type().is_symlink() {
        Some(fs::read_link(path).ok()?)
    } else {
        None
    };
    Some(PathEntry {
        controlled,
        link_target,
    })
}

fn root_controlled_path(
    path: &Path,
    remaining_links: &mut usize,
    inspect: &impl Fn(&Path) -> Option<PathEntry>,
) -> bool {
    if !path.is_absolute() {
        return false;
    }
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let Some(entry) = inspect(ancestor) else {
            return false;
        };
        if !entry.controlled {
            return false;
        }
        if let Some(target) = entry.link_target {
            if *remaining_links == 0 {
                return false;
            }
            *remaining_links -= 1;
            let target = if target.is_absolute() {
                target
            } else {
                let Some(parent) = ancestor.parent() else {
                    return false;
                };
                parent.join(target)
            };
            // Verify each target path before following the remainder. Final
            // canonicalization alone can hide a writable trampoline that
            // links back into a root-owned package directory.
            if !root_controlled_path(&target, remaining_links, inspect) {
                return false;
            }
        }
    }
    true
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

fn portal_helper(
    process: &Process,
    parent: &Process,
    scope: &str,
    uid: u32,
    read: &impl Fn(u32) -> Option<Process>,
    trusted: &impl Fn(&str) -> bool,
) -> bool {
    let options = process
        .command
        .get(2)
        .map(|options| options.split(',').collect::<Vec<_>>())
        .unwrap_or_default();
    let expected = [
        "rw",
        "nosuid",
        "nodev",
        "fsname=portal",
        "auto_unmount",
        "subtype=portal",
    ];
    process.cgroup == format!("{scope}/session.slice/xdg-document-portal.service")
        && parent.cgroup == process.cgroup
        && program_matches(process, "fusermount3", &["fusermount3"])
        && process.command.first().is_some_and(|path| trusted(path))
        && process.command.len() == 5
        && process.command[1] == "-o"
        && process
            .command
            .ends_with(&["--".to_string(), format!("/run/user/{uid}/doc")])
        && options.len() == expected.len()
        && expected.iter().all(|option| options.contains(option))
        && program_matches(
            parent,
            "xdg-document-portal",
            &["xdg-document-po", ".xdg-document-p"],
        )
        && parent.command.first().is_some_and(|path| trusted(path))
        && read(parent.parent)
            .is_some_and(|manager_process| manager(&manager_process, scope, trusted))
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
    if portal_helper(process, &parent, &scope, uid, read, trusted) {
        return true;
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

fn thread_group(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        line.strip_prefix("Tgid:")
            .and_then(|value| value.trim().parse().ok())
    })
}

fn verified_group(
    group: Option<u32>,
    uid: u32,
    read: &impl Fn(u32) -> Option<Process>,
    trusted: &impl Fn(&str) -> bool,
) -> bool {
    group
        .and_then(read)
        .is_some_and(|process| verified_service(&process, uid, read, trusted))
}

pub fn verified_desktop_service(pid: u32) -> bool {
    let Some(uid) = super::rm_tool::effective_uid() else {
        return false;
    };
    // Sysinfo includes tasks by default. Their kernel names need not match
    // the program (QDBusConnection, output workers, ...), but the kernel Tgid
    // proves which service owns them. Ordinary agent tasks are never ignored.
    let group = fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| thread_group(&status));
    verified_group(group, uid, &read_process, &trusted_program)
}

fn full_proc_visibility(mountinfo: &str) -> bool {
    let mut found = false;
    for line in mountinfo.lines() {
        let Some((mount, filesystem)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<_> = mount.split_whitespace().collect();
        if fields.get(4) != Some(&"/proc") {
            continue;
        }
        found = true;
        let filesystem: Vec<_> = filesystem.split_whitespace().collect();
        if filesystem.first() != Some(&"proc") {
            return false;
        }
        if fields.get(5).is_none() || filesystem.get(2).is_none() {
            return false;
        }
        let mut options = fields
            .get(5)
            .into_iter()
            .chain(filesystem.get(2))
            .flat_map(|options| options.split(','));
        if options.any(|option| {
            option.starts_with("hidepid") && !matches!(option, "hidepid=0" | "hidepid=off")
        }) {
            return false;
        }
    }
    found
}

/// A disappeared task cannot own scratch. Only unrestricted Linux procfs
/// plus an actual NotFound answer prove this; hidden/denied reads are unknown.
pub fn task_has_exited(pid: u32) -> bool {
    super::host::LINUX
        && fs::metadata("/proc/self/status").is_ok()
        && fs::read_to_string("/proc/self/mountinfo").is_ok_and(|info| full_proc_visibility(&info))
        && fs::metadata(format!("/proc/{pid}"))
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
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

    #[test]
    fn protected_tasks_follow_their_kernel_group_not_their_thread_name() {
        let read = |pid| match pid {
            1 => Some(Process {
                parent: 0,
                ..manager_process()
            }),
            10 => Some(manager_process()),
            20 => Some(service()),
            30 => Some(Process {
                comm: "claude".into(),
                ..service()
            }),
            _ => None,
        };
        let group = thread_group("Name:\tQDBusConnection\nTgid:\t20\nPid:\t21\n");
        assert!(verified_group(group, 1000, &read, &|_| true));
        assert!(!verified_group(Some(30), 1000, &read, &|_| true));
        assert!(!verified_group(Some(99), 1000, &read, &|_| true));
        assert!(!verified_group(
            thread_group("Pid:\t21\n"),
            1000,
            &read,
            &|_| true
        ));
    }

    #[test]
    fn document_portal_fuse_helper_requires_scope_parent_and_fixed_mount() {
        let scope = "/user.slice/user-1000.slice/user@1000.service/session.slice/xdg-document-portal.service";
        let portal = Process {
            parent: 10,
            comm: ".xdg-document-p".into(),
            cgroup: scope.into(),
            command: vec!["/usr/libexec/xdg-document-portal".into()],
        };
        let read = |pid| match pid {
            1 => Some(Process {
                parent: 0,
                ..manager_process()
            }),
            10 => Some(manager_process()),
            40 => Some(portal.clone()),
            _ => None,
        };
        let helper = Process {
            parent: 40,
            comm: "fusermount3".into(),
            cgroup: scope.into(),
            command: [
                "/usr/bin/fusermount3",
                "-o",
                "rw,nosuid,nodev,fsname=portal,auto_unmount,subtype=portal",
                "--",
                "/run/user/1000/doc",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        };
        assert!(verified_service(&helper, 1000, &read, &|_| true));
        let mut wrong_mount = helper.clone();
        *wrong_mount.command.last_mut().unwrap() = "/home/u/.clud/tmp".into();
        assert!(!verified_service(&wrong_mount, 1000, &read, &|_| true));
        let mut wrong_parent = helper.clone();
        wrong_parent.parent = 10;
        assert!(!verified_service(&wrong_parent, 1000, &read, &|_| true));
        let mut wrong_options = helper.clone();
        wrong_options.command[2] = "rw,fsname=other,subtype=portal".into();
        assert!(!verified_service(&wrong_options, 1000, &read, &|_| true));
        for extra in [",fsname=other", ",fsname=portal", ",allow_other"] {
            let mut injected = helper.clone();
            injected.command[2].push_str(extra);
            assert!(!verified_service(&injected, 1000, &read, &|_| true));
        }
        let mut duplicate_pair = helper.clone();
        duplicate_pair
            .command
            .splice(3..3, ["-o".into(), "fsname=other,subtype=other".into()]);
        assert!(!verified_service(&duplicate_pair, 1000, &read, &|_| true));
        let mut extra_switch = helper.clone();
        extra_switch.command.insert(1, "--unmount".into());
        assert!(!verified_service(&extra_switch, 1000, &read, &|_| true));
        assert!(!verified_service(&helper, 1000, &read, &|_| false));
    }

    #[test]
    fn live_task_is_never_classified_as_exited() {
        assert!(!task_has_exited(std::process::id()));
        let visible = super::super::host::LINUX
            && fs::read_to_string("/proc/self/mountinfo")
                .is_ok_and(|info| full_proc_visibility(&info));
        assert_eq!(task_has_exited(u32::MAX), visible);
    }

    #[test]
    fn hidden_or_unverified_procfs_never_proves_a_missing_task_exited() {
        let visible = "10 1 0:5 / /proc rw,nosuid,nodev - proc proc rw\n";
        assert!(full_proc_visibility(visible));
        for restriction in [
            "hidepid=1",
            "hidepid=2",
            "hidepid=invisible",
            "hidepid=ptraceable",
            "hidepid=bad",
        ] {
            let restricted = format!("10 1 0:5 / /proc rw - proc proc rw,{restriction}\n");
            assert!(!full_proc_visibility(&restricted), "{restriction}");
            assert!(!full_proc_visibility(&format!("{visible}{restricted}")));
        }
        assert!(!full_proc_visibility(
            "10 1 0:5 / /proc rw,hidepid=2 - proc proc rw\n"
        ));
        assert!(!full_proc_visibility(
            "10 1 0:5 / /proc rw - tmpfs tmpfs rw\n"
        ));
        assert!(!full_proc_visibility(""));
        assert!(!full_proc_visibility("10 1 0:5 / /proc rw - proc proc\n"));
    }

    #[test]
    fn writable_programs_and_parent_directories_are_not_trusted() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("writable-parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        let program = parent.join("fusermount3");
        fs::write(&program, "not a system executable").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!trusted_program(program.to_str().unwrap()));
        fs::set_permissions(&program, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!trusted_program(program.to_str().unwrap()));
        assert!(!trusted_program("fusermount3"));
    }

    #[test]
    fn every_symlink_target_must_stay_under_root_control() {
        let inspect = |path: &Path| {
            let target = match path.to_str().unwrap() {
                "/trusted/bin" => Some("/home/user/bin"),
                "/trusted/bin/fusermount3" | "/home/user/bin/fusermount3" => {
                    Some("/nix/store/fuse/bin/fusermount3")
                }
                _ => None,
            };
            Some(PathEntry {
                controlled: path != Path::new("/home/user"),
                link_target: target.map(Into::into),
            })
        };
        assert!(!root_controlled_path(
            Path::new("/trusted/bin/fusermount3"),
            &mut 40,
            &inspect
        ));
        let root_links = |path: &Path| {
            Some(PathEntry {
                controlled: true,
                link_target: (path == Path::new("/run/current-system"))
                    .then(|| "/nix/store/system".into()),
            })
        };
        assert!(root_controlled_path(
            Path::new("/run/current-system/bin/fusermount3"),
            &mut 40,
            &root_links
        ));
        let cycle = |_: &Path| {
            Some(PathEntry {
                controlled: true,
                link_target: Some("/loop".into()),
            })
        };
        assert!(!root_controlled_path(Path::new("/loop"), &mut 40, &cycle));
        assert!(!root_controlled_path(
            Path::new("/unknown"),
            &mut 40,
            &|_| None
        ));
    }
}

//! Unix half of `crate::rm_guard`: mount points are found from
//! `/proc/self/mountinfo` on Linux and from device-number boundaries
//! everywhere (the only signal on macOS).

use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[cfg(target_os = "linux")]
pub fn is_mount_point(path: &Path) -> bool {
    if std::fs::read_to_string("/proc/self/mountinfo")
        .is_ok_and(|contents| mountinfo_has_path(path, &contents))
    {
        return true;
    }
    device_boundary(path)
}

#[cfg(not(target_os = "linux"))]
pub fn is_mount_point(path: &Path) -> bool {
    device_boundary(path)
}

fn device_boundary(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(meta), Some(parent)) = (std::fs::metadata(path), path.parent()) else {
        return false;
    };
    std::fs::metadata(parent).is_ok_and(|p| meta.dev() != p.dev())
}

#[cfg(target_os = "linux")]
fn mountinfo_has_path(path: &Path, contents: &str) -> bool {
    contents
        .lines()
        .filter_map(mountinfo_path)
        .any(|mounted| mounted == path)
}

#[cfg(target_os = "linux")]
fn mountinfo_has_descendant(path: &Path, contents: &str) -> bool {
    contents
        .lines()
        .filter_map(mountinfo_path)
        .any(|mounted| mounted != path && mounted.starts_with(path))
}

#[cfg(target_os = "linux")]
fn mountinfo_path(line: &str) -> Option<PathBuf> {
    let encoded = line.split_whitespace().nth(4)?;
    let decoded = encoded
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\");
    Some(PathBuf::from(decoded))
}

#[cfg(target_os = "linux")]
pub fn contains_nested_mount(path: &Path) -> Result<bool, String> {
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(contents) => Ok(mountinfo_has_descendant(path, &contents)),
        Err(_) => device_boundary_within(path),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn contains_nested_mount(path: &Path) -> Result<bool, String> {
    device_boundary_within(path)
}

fn device_boundary_within(path: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let root_dev = std::fs::metadata(path)
        .map_err(|error| format!("cannot inspect deletion tree {path:?}: {error}"))?
        .dev();
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| format!("cannot inspect deletion tree {dir:?}: {error}"))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("cannot inspect deletion tree: {error}"))?;
            let meta = std::fs::symlink_metadata(entry.path())
                .map_err(|error| format!("cannot inspect deletion tree: {error}"))?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                continue;
            }
            if meta.dev() != root_dev {
                return Ok(true);
            }
            pending.push(entry.path());
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::{mountinfo_has_descendant, mountinfo_has_path};
    use crate::rm_guard::{find_handoff, find_handoff_with};
    #[cfg(target_os = "linux")]
    use std::path::Path;

    #[cfg(target_os = "linux")]
    #[test]
    fn bind_mount_on_same_device_is_still_a_mount_point() {
        let mountinfo = "36 25 8:1 /source /tmp/fixture/inner rw - ext4 /dev/sda1 rw\n";
        assert!(mountinfo_has_path(
            Path::new("/tmp/fixture/inner"),
            mountinfo
        ));
        assert!(!mountinfo_has_path(Path::new("/tmp/fixture"), mountinfo));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_recursive_parent_detects_same_device_nested_bind_mounts() {
        let mountinfo = "36 25 8:1 /source /tmp/fixture/inner rw - ext4 /dev/sda1 rw\n";
        assert!(mountinfo_has_descendant(
            Path::new("/tmp/fixture"),
            mountinfo
        ));
        assert!(!mountinfo_has_descendant(
            Path::new("/tmp/fixture/inner"),
            mountinfo
        ));
        assert!(!mountinfo_has_descendant(
            Path::new("/tmp/other"),
            mountinfo
        ));
    }

    /// #1746: an alias that had to be a symlink (binary and session shim
    /// directory on different filesystems) runs as `clud`, so only the
    /// session shim directory on PATH marks where the real binary search
    /// starts. Strict mode must find it there, and still refuse without it.
    #[test]
    fn a_symlinked_alias_finds_its_session_shim_directory() {
        use std::os::unix::fs::PermissionsExt;
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let bin_dir = temp.path().join("venv-bin");
        let shim_dir = temp.path().join("session-shims");
        let real_dir = temp.path().join("usr-bin");
        for dir in [&bin_dir, &shim_dir, &real_dir] {
            std::fs::create_dir(dir).unwrap();
        }
        let clud = bin_dir.join(crate::shim_registry::file_name("clud"));
        std::fs::write(&clud, b"clud").unwrap();
        let real = real_dir.join(concat!("r", "m"));
        std::fs::write(&real, b"real").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&shim_dir, &real_dir]).unwrap();
        let path = path.to_string_lossy();
        assert_eq!(
            find_handoff_with(&path, &clud, Some(&shim_dir), None).unwrap(),
            real
        );
        assert!(
            find_handoff_with(&path, &clud, None, None).is_err(),
            "strict mode still refuses when no shim directory is known"
        );
    }

    #[test]
    fn busybox_handoff_keeps_the_rm_applet_symlink_name() {
        use std::os::unix::fs::symlink;
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let shim_dir = temp.path().join("shim");
        let handoff_dir = temp.path().join("handoff");
        std::fs::create_dir(&shim_dir).unwrap();
        std::fs::create_dir(&handoff_dir).unwrap();
        let shim = shim_dir.join("rm");
        std::fs::write(&shim, b"shim").unwrap();
        let busybox = handoff_dir.join("busybox");
        std::fs::write(&busybox, b"busybox").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&busybox, std::fs::Permissions::from_mode(0o755)).unwrap();
        let applet = handoff_dir.join("rm");
        symlink(&busybox, &applet).unwrap();
        let path = std::env::join_paths([&shim_dir, &handoff_dir]).unwrap();
        assert_eq!(
            find_handoff(&path.to_string_lossy(), &shim).unwrap(),
            applet
        );
    }
}

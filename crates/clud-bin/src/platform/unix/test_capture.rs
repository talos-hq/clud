//! Unix privacy checks and pipe handle conversion for test capture.

#[cfg(not(target_os = "macos"))]
use std::fs;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[cfg(target_os = "macos")]
#[path = "test_capture_acl.rs"]
mod macos_acl;

/// Refuse a writable, untrusted or symlinked path before creating artifacts.
#[cfg(not(target_os = "macos"))]
pub fn verify_root(root: &Path) -> io::Result<()> {
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact root must be absolute",
        ));
    }
    // SAFETY: geteuid takes no arguments and has no side effects.
    let uid = unsafe { libc::geteuid() };
    for (index, path) in root.ancestors().enumerate() {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.file_type().is_symlink() || (meta.uid() != uid && meta.uid() != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "untrusted artifact directory",
            ));
        }
        if index == 0 {
            if meta.uid() != uid || meta.mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "artifact root is not private",
                ));
            }
        } else if meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "writable artifact ancestor",
            ));
        }
    }
    Ok(())
}

/// Verify the opened regular file before any output reaches it.
#[cfg(not(target_os = "macos"))]
pub fn verify_file(file: &File) -> io::Result<()> {
    // SAFETY: geteuid has no arguments and no side effects.
    let uid = unsafe { libc::geteuid() };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "artifact file is not private",
        ));
    }
    Ok(())
}

/// Refuse macOS roots until extended ACLs are verified.
#[cfg(target_os = "macos")]
pub fn verify_root(_root: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "macOS private artifact ACLs are unavailable",
    ))
}

/// Check an open macOS file's mode, owner, link count and extended ACL.
/// The root still fails closed until handle-based ancestry checks exist.
#[cfg(target_os = "macos")]
pub fn verify_file(file: &File) -> io::Result<()> {
    // SAFETY: geteuid takes no arguments and has no side effects.
    let uid = unsafe { libc::geteuid() };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "artifact file is not private",
        ));
    }
    macos_acl::verify_empty_acl(file)
}

/// Poll the capture pipe so a retained writer cannot prevent reader shutdown.
pub fn read_pipe(reader: &mut os_pipe::PipeReader, data: &mut [u8]) -> io::Result<Option<usize>> {
    let mut descriptor = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: descriptor points to one initialized pollfd for the duration of poll.
    let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if ready == 0 {
        return Ok(None);
    }
    if descriptor.revents & libc::POLLNVAL != 0 {
        return Err(io::Error::other("invalid capture pipe"));
    }
    reader.read(data).map(Some)
}

/// Convert the writer to the file type accepted by running-process.
pub fn pipe_writer_file(writer: os_pipe::PipeWriter) -> File {
    let handle: OwnedFd = writer.into();
    File::from(handle)
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;
    use crate::test_runtime::capture::capture;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[test]
    fn captures_merged_binary_output_and_exact_failure_status() {
        let root = private_root();
        let command = vec![
            "sh".into(),
            "-c".into(),
            "printf 'a\\r\\n\\377'; printf 'err' >&2; exit 7".into(),
        ];
        let result = capture(root.path(), Duration::from_secs(5), &command).unwrap();
        assert_eq!(result.state, "complete");
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(fs::read(result.artifact).unwrap(), b"a\r\n\xfferr");
    }

    #[test]
    fn rejects_untrusted_root_before_start() {
        let root = private_root();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let command = vec!["sh".into(), "-c".into(), "exit 0".into()];
        assert!(capture(root.path(), Duration::from_secs(1), &command).is_err());
    }

    #[test]
    fn output_limit_reports_the_written_prefix_length() {
        let root = private_root();
        let command = vec!["sh".into(), "-c".into(), "printf 1234567890".into()];
        let result = crate::test_runtime::capture::capture_with_limit(
            root.path(),
            Duration::from_secs(5),
            &command,
            5,
        )
        .unwrap();
        assert_eq!(result.state, "output_limit");
        assert_eq!(result.bytes, Some(5));
        assert_eq!(fs::read(result.artifact).unwrap(), b"12345");
        assert_eq!(result.tree_cleanup, "unverified");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_writer_makes_timeout_cleanup_uncertain() {
        let root = private_root();
        let command = vec![
            "sh".into(),
            "-c".into(),
            "setsid sh -c 'sleep 30' & printf ready".into(),
        ];
        let result = capture(root.path(), Duration::from_secs(2), &command).unwrap();
        assert_eq!(result.state, "timeout");
        assert!(!result.pipe_eof);
        assert_eq!(result.tree_cleanup, "unverified");
        assert_eq!(fs::read(result.artifact).unwrap(), b"ready");
    }

    #[test]
    fn retained_pipe_writer_causes_bounded_incomplete_result() {
        let root = private_root();
        let command = vec!["sh".into(), "-c".into(), "(sleep 30) & printf ready".into()];
        let result = capture(root.path(), Duration::from_secs(2), &command).unwrap();
        assert_eq!(result.state, "timeout");
        assert_eq!(fs::read(result.artifact).unwrap(), b"ready");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    fn grant_acl(path: &std::path::Path, acl: &str) {
        use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec![
                "/bin/chmod".into(),
                "+a".into(),
                acl.into(),
                path.to_str().expect("fixture path is UTF-8").into(),
            ]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Null,
            nice: None,
            address_space_limit_bytes: None,
        });
        process.start().expect("start chmod");
        assert_eq!(
            process
                .wait(Some(std::time::Duration::from_secs(5)))
                .unwrap(),
            0,
            "fixture must install its ACL"
        );
    }
    #[test]
    fn secure_factory_captures_binary_failure_and_reopens_same_object() {
        use std::io::Read as _;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let command = vec![
            "sh".into(),
            "-c".into(),
            "printf 'a\\r\\n\\377'; printf err >&2; exit 7".into(),
        ];
        let identity = std::cell::Cell::new(None);
        let result = crate::test_runtime::capture::capture_with_test_factory(
            &root,
            Duration::from_secs(5),
            &command,
            1024,
            |root| {
                let (file, path) = super::macos_acl::create_private_artifact(root)?;
                let metadata = file.metadata()?;
                identity.set(Some((metadata.dev(), metadata.ino())));
                Ok((file, path))
            },
        )
        .unwrap();
        assert_eq!(result.state, "complete");
        assert_eq!(result.exit_code, Some(7));
        assert!(result.pipe_eof);
        let mut file = super::macos_acl::reopen_private_artifact(
            &root,
            &result.artifact,
            identity.get().unwrap(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"a\r\n\xfferr");
    }

    #[test]
    fn unsafe_acl_refuses_capture_before_child_starts() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        grant_acl(&root, "everyone allow list,search");
        let marker = root.join("child-ran");
        let command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "fixture".into(),
            marker.to_string_lossy().into_owned(),
        ];
        assert!(crate::test_runtime::capture::capture_with_test_factory(
            &root,
            Duration::from_secs(5),
            &command,
            1024,
            super::macos_acl::create_private_artifact,
        )
        .is_err());
        assert!(!marker.exists(), "unsafe capture must not start its child");
    }

    #[test]
    fn accepts_private_root_without_extended_acl() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("temporary root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        super::macos_acl::open_private_root(&canonical)
            .expect("an owner-only root without an ACL is supported");
    }

    #[test]
    fn rejects_root_with_explicit_acl_grant() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("temporary root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        super::macos_acl::open_private_root(&canonical).expect("private root accepted");
        grant_acl(&canonical, "everyone allow list,search");
        assert!(super::macos_acl::open_private_root(&canonical).is_err());
    }

    #[test]
    fn creates_and_reopens_only_the_original_artifact() {
        use std::io::{Read as _, Write as _};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (mut file, path) = super::macos_acl::create_private_artifact(&root).unwrap();
        file.write_all(b"fixture bytes").unwrap();
        file.flush().unwrap();
        let meta = file.metadata().unwrap();
        let identity = (meta.dev(), meta.ino());
        let mut reopened =
            super::macos_acl::reopen_private_artifact(&root, &path, identity).unwrap();
        let mut bytes = Vec::new();
        reopened.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"fixture bytes");
        std::fs::rename(&path, root.join("original")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(super::macos_acl::reopen_private_artifact(&root, &path, identity).is_err());
    }

    #[test]
    fn cleanup_removes_only_the_created_object() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let directory = super::macos_acl::open_private_root(&root).unwrap();
        let (file, path) = super::macos_acl::create_private_artifact(&root).unwrap();
        let name = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
        super::macos_acl::remove_created_artifact(&directory, &name, &file).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn rejects_symlinked_root_component() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.join("link");
        symlink(&target, &link).unwrap();
        assert!(super::macos_acl::open_private_root(&link).is_err());
    }

    #[test]
    fn rejects_inherited_acl_on_root_and_ancestor() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        grant_acl(
            &base,
            "everyone allow list,search,file_inherit,directory_inherit",
        );
        let child = base.join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
        let child_fd = std::fs::File::open(&child).unwrap();
        let error = super::macos_acl::verify_empty_acl(&child_fd)
            .expect_err("inherited ACL must be detected");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(super::macos_acl::open_private_root(&child).is_err());
    }

    #[test]
    fn failed_validation_cleans_the_new_empty_artifact() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let error = super::macos_acl::create_rejected_artifact_for_test(&root)
            .expect_err("validation must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn cleanup_does_not_remove_a_replacement() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let directory = super::macos_acl::open_private_root(&root).unwrap();
        let (file, path) = super::macos_acl::create_private_artifact(&root).unwrap();
        let name = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
        std::fs::rename(&path, root.join("original")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert!(super::macos_acl::remove_created_artifact(&directory, &name, &file).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn rejects_open_file_with_explicit_acl_grant() {
        let root = tempfile::tempdir().expect("temporary root");
        let file = tempfile::NamedTempFile::new_in(root.path()).expect("private file");
        super::verify_file(file.as_file()).expect("an owner-only file without an ACL is supported");
        grant_acl(file.path(), "everyone allow read");
        assert!(
            super::verify_file(file.as_file()).is_err(),
            "an ACL grant must be rejected"
        );
    }

    #[test]
    fn private_capture_fails_closed_before_spawn() {
        let root = tempfile::tempdir().expect("temporary root");
        let error = super::verify_root(root.path()).expect_err("ACL verification is unavailable");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
}

#[cfg(test)]
mod reader_tests {
    use crate::test_runtime::capture::{capture_reader, Event};
    use std::io::Write;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn reader_stops_and_joins_while_writer_remains_open() {
        let (reader, mut writer) = os_pipe::pipe().unwrap();
        let (receiver, stop, handle) = capture_reader(reader).unwrap();
        writer.write_all(b"ready").unwrap();
        assert!(
            matches!(receiver.recv_timeout(Duration::from_secs(2)), Ok(Event::Data(data)) if data == b"ready")
        );
        stop.store(true, Ordering::Release);
        drop(receiver);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(handle.join());
        });
        assert!(done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_ok());
        drop(writer);
    }
}

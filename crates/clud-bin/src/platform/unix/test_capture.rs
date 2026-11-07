//! Unix privacy checks and pipe handle conversion for test capture.

#[cfg(not(target_os = "macos"))]
use std::fs;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(not(target_os = "macos"))]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

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

/// Refuse macOS files until extended ACLs are verified.
#[cfg(target_os = "macos")]
pub fn verify_file(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "macOS private artifact ACLs are unavailable",
    ))
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
        let result = capture(root.path(), Some(Duration::from_secs(5)), &command).unwrap();
        assert_eq!(result.state, "complete");
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(fs::read(result.artifact).unwrap(), b"a\r\n\xfferr");
    }

    #[test]
    fn rejects_untrusted_root_before_start() {
        let root = private_root();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let command = vec!["sh".into(), "-c".into(), "exit 0".into()];
        assert!(capture(root.path(), Some(Duration::from_secs(1)), &command).is_err());
    }

    #[test]
    fn output_limit_reports_the_written_prefix_length() {
        let root = private_root();
        let command = vec!["sh".into(), "-c".into(), "printf 1234567890".into()];
        let result = crate::test_runtime::capture::capture_with_limit(
            root.path(),
            Some(Duration::from_secs(5)),
            &command,
            5,
        )
        .unwrap();
        assert_eq!(result.state, "output_limit");
        assert_eq!(result.bytes, Some(5));
        assert_eq!(fs::read(result.artifact).unwrap(), b"12345");
        assert_eq!(result.tree_cleanup, "unverified");
    }

    #[test]
    fn output_limit_drains_until_natural_failure() {
        for (ending, expected) in [("exit 0", 0), ("exit 7", 7), ("kill -TERM $$", -15)] {
            let root = private_root();
            let marker = root.path().join("finished");
            let script = format!(
                "head -c 1048576 /dev/zero; printf done > '{}'; {ending}",
                marker.display()
            );
            let result = crate::test_runtime::capture::capture_with_limit(
                root.path(),
                Some(Duration::from_secs(5)),
                &["sh".into(), "-c".into(), script],
                5,
            )
            .unwrap();
            assert!(
                marker.exists(),
                "capture stopped the validation at its output cap"
            );
            assert_eq!(result.exit_code, Some(expected));
            assert!(result.pipe_eof);
            assert_eq!(result.termination_requested, None);
            assert_eq!(result.state, "output_limit");
            assert_eq!(result.bytes, Some(5));
        }
    }

    struct BrokenArtifact {
        file: std::fs::File,
        remaining: usize,
    }

    impl std::io::Write for BrokenArtifact {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("injected artifact write failure"));
            }
            let count = self.file.write(&data[..data.len().min(self.remaining)])?;
            self.remaining -= count;
            Ok(count)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_failure_drains_until_natural_completion() {
        for (ending, expected) in [("exit 0", 0), ("exit 7", 7), ("kill -TERM $$", -15)] {
            let root = private_root();
            let marker = root.path().join("finished");
            let script = format!(
                "head -c 1048576 /dev/zero; printf done > '{}'; {ending}",
                marker.display()
            );
            let result = crate::test_runtime::capture::capture_with_writer(
                root.path(),
                Some(Duration::from_secs(5)),
                &["sh".into(), "-c".into(), script],
                5,
                |file| BrokenArtifact { file, remaining: 2 },
            )
            .unwrap();
            assert!(marker.exists());
            assert_eq!(result.state, "write_error");
            assert_eq!(result.evidence_state, Some("write_error"));
            assert_eq!(result.exit_code, Some(expected));
            assert!(result.pipe_eof);
            assert_eq!(result.termination_requested, None);
            assert_eq!(result.bytes, Some(2));
        }
    }

    #[test]
    fn output_limit_keeps_explicit_timeout() {
        let root = private_root();
        let result = crate::test_runtime::capture::capture_with_limit(
            root.path(),
            Some(Duration::from_millis(100)),
            &[
                "sh".into(),
                "-c".into(),
                "printf 1234567890; sleep 10".into(),
            ],
            5,
        )
        .unwrap();
        assert_eq!(result.state, "timeout");
        assert_eq!(result.evidence_state, Some("output_limit"));
        assert_eq!(result.termination_requested, Some(true));
        assert_eq!(result.bytes, Some(5));
    }

    #[test]
    fn no_deadline_captures_a_natural_pass() {
        let root = private_root();
        let result = capture(
            root.path(),
            None,
            &["sh".into(), "-c".into(), "printf done".into()],
        )
        .unwrap();
        assert_eq!(result.state, "complete");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.pipe_eof);
        assert_eq!(result.termination_requested, None);
    }

    #[test]
    fn reporter_alias_has_no_outer_deadline() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("diagnostics");
        fs::create_dir(&directory).unwrap();
        let reporter = directory.join("test_report.py");
        fs::write(&reporter, "fixture").unwrap();
        let alias = root.path().join("report.py");
        std::os::unix::fs::symlink(&reporter, &alias).unwrap();
        let hard_link = root.path().join("hard-report.py");
        fs::hard_link(&reporter, &hard_link).unwrap();
        for path in [&reporter, &alias, &hard_link] {
            let mut watchdog =
                crate::tool_run::watchdog_for_tool(path.to_str().unwrap(), path, root.path());
            watchdog.started_at = std::time::Instant::now() - Duration::from_secs(172_800);
            assert_eq!(
                watchdog.check(),
                crate::tool_watchdog::WatchdogDecision::Continue
            );
        }
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
        let result = capture(root.path(), Some(Duration::from_secs(2)), &command).unwrap();
        assert_eq!(result.state, "timeout");
        assert!(!result.pipe_eof);
        assert_eq!(result.tree_cleanup, "unverified");
        assert_eq!(fs::read(result.artifact).unwrap(), b"ready");
    }

    #[test]
    fn retained_pipe_writer_causes_bounded_incomplete_result() {
        let root = private_root();
        let command = vec!["sh".into(), "-c".into(), "(sleep 30) & printf ready".into()];
        let result = capture(root.path(), Some(Duration::from_secs(2)), &command).unwrap();
        assert_eq!(result.state, "timeout");
        assert_eq!(fs::read(result.artifact).unwrap(), b"ready");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
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

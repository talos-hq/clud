//! The Unix half of `reaper/daemon_spawn_hygiene.rs` (#1186): `clud __daemon`
//! must not keep a launcher's non-`FD_CLOEXEC` pipe open. See that file's
//! header for both properties and why raw `std::process::Command` is used
//! (this basename is in `ci/banned_imports.py`'s exempt set).

use std::time::{Duration, Instant};

use crate::reaper::daemon_spawn_hygiene::{
    assert_success, daemon_command, read_daemon_pid, DaemonGuard,
};

#[test]
fn daemon_does_not_inherit_a_launchers_non_cloexec_fds() {
    use std::os::unix::process::CommandExt;

    let _serial = crate::reaper::REAPER_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let scratch = kernal_api::platform::fs::TemporaryDirectory::new().expect("scratch dir");
    let state_dir = scratch.path().join("state");

    let mut fds = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe()");
    let (read_end, write_end) = (fds[0], fds[1]);
    // CLOEXEC in *this* process, so no other test's child can pick the pipe
    // up and hold it open — that would fail this test for someone else's leak.
    for fd in fds {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            assert_ne!(libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC), -1);
        }
    }

    let mut command = daemon_command(&state_dir, "restart");
    // Hand only the write end to `clud`, exactly as a launcher that leaked a
    // non-CLOEXEC pipe would. Runs in the forked child just before exec.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(write_end, libc::F_GETFD);
            if flags == -1 || libc::fcntl(write_end, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _guard = DaemonGuard {
        state_dir: state_dir.clone(),
    };
    let output = command.output().expect("run clud daemon restart");
    assert_success(&output, "clud daemon restart");
    let daemon_pid = read_daemon_pid(&state_dir);

    // `clud` has exited. Once this process drops its own copy, the only
    // possible remaining writer is something `clud` started.
    unsafe { libc::close(write_end) };
    let saw_eof = wait_for_eof(read_end, Duration::from_secs(5));
    unsafe { libc::close(read_end) };

    assert!(
        saw_eof,
        "the launcher's pipe never reached EOF: daemon pid {daemon_pid} inherited \
         a non-CLOEXEC write end from `clud` and is holding it open"
    );
}

/// Whether `fd` reports end-of-file within `timeout`.
fn wait_for_eof(fd: libc::c_int, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 64];
    while Instant::now() < deadline {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready > 0 {
            let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if read == 0 {
                return true;
            }
        }
    }
    false
}

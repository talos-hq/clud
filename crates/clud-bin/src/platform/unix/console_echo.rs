//! Unix half of `crate::platform::console_echo`: hide typed input while a
//! line reader keeps working.
//!
//! The remote-assistance client reads the user's sudo password on its own
//! terminal. Echo is a property of the terminal, not of the thread reading
//! it, so the session loop clears `ECHO` on fd 0 while the background line
//! reader (`remote_connect::stdin_lines`) stays blocked in `read_line`.
//! `ICANON` and `ISIG` stay on, so line editing and Ctrl+C still work, and
//! `ECHONL` makes Enter still move to a new line, as `getpass` does. The
//! change uses `TCSANOW`, not `TCSAFLUSH`, so a line already typed is kept.

use std::io;
use std::os::fd::AsRawFd;

/// Restores the terminal's saved attributes on drop, including during a
/// panic unwind. An inert guard (stdin is not a terminal) restores nothing.
pub struct EchoGuard {
    fd: libc::c_int,
    original: Option<libc::termios>,
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            // SAFETY: `fd` was a terminal when `original` was read from it by
            // a successful `tcgetattr`; restoring cannot fail in a way a retry
            // would fix.
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &original);
            }
        }
    }
}

/// Stop echoing what is typed on stdin until the guard drops.
pub fn suppress_stdin_echo() -> io::Result<EchoGuard> {
    suppress_echo_on(io::stdin().as_raw_fd())
}

fn suppress_echo_on(fd: libc::c_int) -> io::Result<EchoGuard> {
    // SAFETY: a zeroed `termios` is a valid buffer for `tcgetattr` to fill.
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `term` is a valid out-pointer.
    if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
        return Ok(EchoGuard { fd, original: None });
    }
    let original = term;
    term.c_lflag &= !libc::ECHO;
    term.c_lflag |= libc::ECHONL;
    // SAFETY: `term` holds attributes `tcgetattr` just returned for `fd`.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(EchoGuard {
        fd,
        original: Some(original),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Pty {
        master: libc::c_int,
        slave: libc::c_int,
    }

    impl Drop for Pty {
        fn drop(&mut self) {
            // SAFETY: both descriptors came from `openpty` and are closed once.
            unsafe {
                libc::close(self.slave);
                libc::close(self.master);
            }
        }
    }

    fn open_pty() -> Pty {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: valid out-pointers; null name, termios and winsize are allowed.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", io::Error::last_os_error());
        Pty { master, slave }
    }

    fn local_flags(fd: libc::c_int) -> libc::tcflag_t {
        // SAFETY: as in `suppress_echo_on`.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut term) }, 0);
        term.c_lflag
    }

    #[test]
    fn echo_is_off_while_held_and_restored_on_drop() {
        let pty = open_pty();
        let before = local_flags(pty.slave);
        assert_ne!(before & libc::ECHO, 0, "a new pty echoes");
        let guard = suppress_echo_on(pty.slave).unwrap();
        let held = local_flags(pty.slave);
        assert_eq!(held & libc::ECHO, 0);
        assert_ne!(held & libc::ECHONL, 0);
        assert_ne!(held & libc::ICANON, 0, "line editing stays on");
        assert_ne!(held & libc::ISIG, 0, "Ctrl+C stays a signal");
        drop(guard);
        assert_eq!(local_flags(pty.slave), before);
    }

    #[test]
    fn echo_is_restored_when_a_panic_unwinds_past_the_guard() {
        let pty = open_pty();
        let before = local_flags(pty.slave);
        let slave = pty.slave;
        let unwound = std::panic::catch_unwind(move || {
            let _guard = suppress_echo_on(slave).unwrap();
            panic!("password prompt interrupted");
        });
        assert!(unwound.is_err());
        assert_eq!(local_flags(pty.slave), before);
    }

    #[test]
    fn a_non_terminal_gets_an_inert_guard() {
        let file = tempfile::tempfile().unwrap();
        let guard = suppress_echo_on(file.as_raw_fd()).unwrap();
        assert!(guard.original.is_none());
    }
}

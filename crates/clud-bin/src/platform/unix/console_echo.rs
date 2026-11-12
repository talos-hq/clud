//! Unix half of `crate::platform::console_echo`.
//!
//! Turn off terminal `ECHO` while keeping canonical line mode, so a password
//! typed on the controlling terminal is not displayed but a line-buffered
//! reader still returns it on Enter. The remote-assistance client uses this to
//! read the user's sudo password locally without echoing it to the screen.

use std::io;
use std::os::fd::AsRawFd;

/// Restores the terminal's original local flags on drop. An inert guard (when
/// stdin is not a TTY, e.g. a pipe under test) restores nothing.
pub struct StdinEchoGuard {
    fd: i32,
    original: libc::termios,
    active: bool,
}

impl Drop for StdinEchoGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: `fd` is this process's stdin and `original` was filled by a
            // successful `tcgetattr`. Restoring the saved attributes cannot fail
            // in a way a retry would fix.
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
            }
        }
    }
}

/// Clear `ECHO` on the controlling terminal, leaving canonical mode intact so a
/// line reader still works. Returns an inert guard when stdin is not a terminal.
pub fn suppress_stdin_echo() -> io::Result<StdinEchoGuard> {
    let fd = io::stdin().as_raw_fd();
    // SAFETY: a zeroed `termios` is a valid buffer for `tcgetattr` to fill.
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is this process's stdin; `term` is a valid out-pointer.
    if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
        // Not a terminal: nothing to suppress, hand back an inert guard.
        return Ok(StdinEchoGuard {
            fd,
            original: term,
            active: false,
        });
    }
    let original = term;
    term.c_lflag &= !libc::ECHO;
    // SAFETY: `fd` is this process's stdin; `term` holds valid attributes.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(StdinEchoGuard {
        fd,
        original,
        active: true,
    })
}

#[cfg(test)]
mod tests {
    use super::suppress_stdin_echo;

    #[test]
    fn suppress_returns_a_guard_and_restores_cleanly() {
        // Under the test harness stdin is not a terminal, so this takes the
        // inert path and must still hand back a droppable guard without error.
        let guard = suppress_stdin_echo().expect("inert guard on a non-tty stdin");
        drop(guard);
    }
}

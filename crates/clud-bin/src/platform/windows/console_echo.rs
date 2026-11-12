//! Windows half of `crate::platform::console_echo`.
//!
//! Clear `ENABLE_ECHO_INPUT` on the console input handle while keeping
//! `ENABLE_LINE_INPUT`, so a password typed at the console is not displayed but
//! a line reader still returns it on Enter. Mirrors the Unix `ECHO` toggle used
//! by the remote-assistance client to read the user's sudo password locally.

use std::io;

/// `ENABLE_ECHO_INPUT` from `wincon.h`.
const ENABLE_ECHO_INPUT: u32 = 0x0004;

/// Restores the console input mode on drop. An inert guard (when stdin is not a
/// console) restores nothing.
pub struct StdinEchoGuard {
    handle: isize,
    original: u32,
    active: bool,
}

impl Drop for StdinEchoGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: `handle` is this process's stdin console handle and
            // `original` was read by a successful `GetConsoleMode`.
            unsafe {
                SetConsoleMode(self.handle, self.original);
            }
        }
    }
}

/// Clear `ENABLE_ECHO_INPUT` on the console input handle, leaving line input
/// intact. Returns an inert guard when stdin is not a console.
pub fn suppress_stdin_echo() -> io::Result<StdinEchoGuard> {
    use std::os::windows::io::AsRawHandle;
    let handle = io::stdin().as_raw_handle() as isize;
    let mut mode: u32 = 0;
    // SAFETY: `handle` is this process's own stdin handle and `mode` is a valid
    // out-pointer. A non-console handle makes the call fail.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Ok(StdinEchoGuard {
            handle,
            original: mode,
            active: false,
        });
    }
    let original = mode;
    // SAFETY: `handle` is this process's own stdin handle.
    if unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(StdinEchoGuard {
        handle,
        original,
        active: true,
    })
}

extern "system" {
    fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: isize, mode: u32) -> i32;
}

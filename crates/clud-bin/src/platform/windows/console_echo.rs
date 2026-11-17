//! Windows half of `crate::platform::console_echo`.
//!
//! The only caller is the remote-assistance sudo password prompt, and the
//! client never advertises sudo on Windows (`platform::remote_shell::SUPPORTED`
//! is false), so this is an inert guard rather than a second console-mode
//! implementation nothing would exercise.

use std::io;

pub struct EchoGuard;

pub fn suppress_stdin_echo() -> io::Result<EchoGuard> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hidden input is not implemented on Windows",
    ))
}

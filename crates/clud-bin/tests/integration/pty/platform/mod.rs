//! The `pty` category's host boundary (#1953): the only place in
//! `tests/integration/pty/` that selects a host. Host-only test modules
//! live in the concrete trees below.

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(super) use unix::bash_executable;
#[cfg(windows)]
pub(super) use windows::bash_executable;

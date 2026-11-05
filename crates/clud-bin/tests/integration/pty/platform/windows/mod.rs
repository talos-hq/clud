//! Windows-only `pty` integration tests.

mod console_wrap;
mod pty_pump;
mod shift_enter_dual_reader;
mod start_terminal;
mod utf8_codepage;

pub(crate) fn bash_executable() -> std::path::PathBuf {
    start_terminal::git_bash()
}

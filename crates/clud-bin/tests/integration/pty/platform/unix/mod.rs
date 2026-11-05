//! Unix-only `pty` integration tests.

mod toast_pty;

pub(crate) fn bash_executable() -> std::path::PathBuf {
    std::path::PathBuf::from("bash")
}

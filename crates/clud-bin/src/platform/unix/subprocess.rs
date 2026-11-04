//! Unix half of `crate::subprocess`. POSIX has no `.cmd` / `.bat` concept
//! and no suspended-spawn path: every launch stays argv-based.

use std::path::PathBuf;
use std::time::Duration;

use running_process::ReadStatus;

/// Never constructed off Windows: `crate::subprocess` only takes the
/// suspended path when `platform::host::WINDOWS` holds.
pub enum SuspendedSubprocess {}

impl SuspendedSubprocess {
    pub fn spawn(
        _argv: Vec<String>,
        _cwd: Option<PathBuf>,
        _env: Option<Vec<(String, String)>>,
        _capture: bool,
        _inherit_stdin: bool,
    ) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "suspended spawn is Windows-only",
        ))
    }

    pub fn pid(&self) -> u32 {
        match *self {}
    }

    pub fn poll(&self) -> std::io::Result<Option<i32>> {
        match *self {}
    }

    pub fn wait(&self, _timeout: Option<Duration>) -> std::io::Result<i32> {
        match *self {}
    }

    pub fn kill(&self) -> std::io::Result<()> {
        match *self {}
    }

    pub fn read_stdout(&self, _timeout: Option<Duration>) -> ReadStatus<Vec<u8>> {
        match *self {}
    }
}

/// Always `None`: POSIX never routes a launch through a batch shell.
pub fn batch_shell_command(_argv: &[String]) -> Option<String> {
    None
}

/// Always false: a file named `foo.cmd` is just an executable here.
pub fn is_batch_wrapper(_program: Option<&str>) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use crate::subprocess::{argv_is_batch_wrapped, command_spec_for_subprocess};

    #[test]
    fn argv_is_batch_wrapped_always_false_off_windows() {
        assert!(!argv_is_batch_wrapped(&["something.cmd".into()]));
        assert!(!argv_is_batch_wrapped(&["something.bat".into()]));
    }
    #[test]
    fn cmd_suffix_is_not_special_off_windows() {
        // POSIX doesn't have batch wrappers; even a file literally named
        // `foo.cmd` is just an executable, and we must not invent a cmd.exe
        // dependency on Linux/macOS.
        let spec = command_spec_for_subprocess(vec!["codex.cmd".into(), "exec".into()]);
        match spec {
            running_process::CommandSpec::Argv(argv) => {
                assert_eq!(argv, vec!["codex.cmd", "exec"]);
            }
            other => panic!("expected Argv off Windows, got {other:?}"),
        }
    }
    #[test]
    fn shell_script_path_is_not_wrapped() {
        // `.sh` and extensionless POSIX paths must always pass through.
        let spec = command_spec_for_subprocess(vec!["./run.sh".into(), "--flag".into()]);
        match spec {
            running_process::CommandSpec::Argv(argv) => {
                assert_eq!(argv, vec!["./run.sh", "--flag"]);
            }
            other => panic!("expected Argv for .sh script, got {other:?}"),
        }
        let spec = command_spec_for_subprocess(vec!["/usr/bin/codex".into(), "exec".into()]);
        match spec {
            running_process::CommandSpec::Argv(argv) => {
                assert_eq!(argv, vec!["/usr/bin/codex", "exec"]);
            }
            other => panic!("expected Argv for extensionless path, got {other:?}"),
        }
    }
}

//! Subprocess launch helpers.
//!
//! On Windows, Rust's `std::process::Command` cannot directly spawn `.cmd` /
//! `.bat` files. Since Rust 1.77 (BatBadBat / CVE-2024-24576) the standard
//! library refuses any batch invocation whose arguments don't round-trip
//! losslessly through cmd.exe's idiosyncratic command-line parser; the visible
//! failure mode is `failed to spawn process: batch file arguments are
//! invalid`. That hits `clud --codex` because npm installs Codex as a `.cmd`
//! shim at `C:\Users\<user>\AppData\Roaming\npm\codex.cmd` (see issue #59).
//!
//! The fix is to rewrite the launch as `cmd.exe /D /S /C "<bat-path>" <args>`
//! so Windows is launching cmd.exe (a real `.exe`) and the batch invocation
//! lives inside a shell command line where we control the quoting.
//! `running-process-core` already provides exactly that wrapper via
//! `CommandSpec::Shell`, which builds `cmd /D /S /C "<command>"` using
//! `raw_arg` and therefore preserves our quoting verbatim.
//!
//! This module is the single place that decides whether a given backend argv
//! needs the Windows batch rewrite. POSIX has no `.cmd` / `.bat` concept;
//! every code path stays argv-based there.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode, StreamKind,
};

use crate::platform::host;
use crate::platform::subprocess::{self as imp, SuspendedSubprocess};

/// A backend child whose Windows pipe-owning launches use the race-free
/// contained-spawn API.
///
/// `NativeProcess` creates a running child and only then assigns its Job
/// Object. A very short-lived `cmd.exe` can spawn a descendant and exit in
/// that gap, leaving the descendant outside the Job with inherited capture
/// handles. `running_process::spawn` creates the child suspended, assigns the
/// Job, and only then resumes it. We use that path exactly when pipes are
/// load-bearing: clud is capturing internally, or either of its own output
/// streams is a non-terminal (pytest/CI/supervisor capture). Console-attached launches
/// keep `NativeProcess`, preserving their console process-group and Ctrl-C
/// behavior; a terminal handle has no EOF reader for a descendant to strand.
pub struct ManagedSubprocess(ManagedSubprocessInner);

enum ManagedSubprocessInner {
    // Boxed: `NativeProcess` grew to ~350 bytes in running-process 4.9.0, and
    // an unboxed variant here trips `clippy::large_enum_variant` against the
    // much smaller `Suspended` variant. The box keeps the enum small; match
    // arms auto-deref through it unchanged.
    Native(Box<NativeProcess>),
    Suspended(SuspendedSubprocess),
}

impl ManagedSubprocess {
    pub fn start(
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Vec<(String, String)>,
        capture: bool,
        creationflags: Option<u32>,
    ) -> Result<Self, String> {
        Self::start_with_env(argv, cwd, Some(env), capture, creationflags, true)
    }

    /// Headless child whose terminal input is owned by the loop dashboard.
    pub fn start_without_stdin(
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Vec<(String, String)>,
        capture: bool,
        creationflags: Option<u32>,
    ) -> Result<Self, String> {
        Self::start_with_env(argv, cwd, Some(env), capture, creationflags, false)
    }

    pub fn start_inheriting_env(
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        capture: bool,
        creationflags: Option<u32>,
    ) -> Result<Self, String> {
        Self::start_with_env(argv, cwd, None, capture, creationflags, false)
    }

    fn start_with_env(
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Option<Vec<(String, String)>>,
        capture: bool,
        creationflags: Option<u32>,
        inherit_stdin: bool,
    ) -> Result<Self, String> {
        if use_suspended_spawn(
            capture,
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        ) {
            return SuspendedSubprocess::spawn(argv, cwd, env, capture, inherit_stdin)
                .map(ManagedSubprocessInner::Suspended)
                .map(Self)
                .map_err(|error| error.to_string());
        }

        let process = NativeProcess::new(ProcessConfig {
            command: command_spec_for_subprocess(argv),
            cwd,
            env,
            capture,
            stderr_mode: StderrMode::Stdout,
            creationflags,
            create_process_group: false,
            stdin_mode: if inherit_stdin {
                StdinMode::Inherit
            } else {
                StdinMode::Null
            },
            nice: None,
            address_space_limit_bytes: None,
        });
        process.start().map_err(|error| error.to_string())?;
        Ok(Self(ManagedSubprocessInner::Native(Box::new(process))))
    }

    pub fn pid(&self) -> Option<u32> {
        match &self.0 {
            ManagedSubprocessInner::Native(process) => process.pid(),
            ManagedSubprocessInner::Suspended(process) => Some(process.pid()),
        }
    }

    pub fn poll(&self) -> Result<Option<i32>, String> {
        match &self.0 {
            ManagedSubprocessInner::Native(process) => {
                process.poll().map_err(|error| error.to_string())
            }
            ManagedSubprocessInner::Suspended(process) => {
                process.poll().map_err(|error| error.to_string())
            }
        }
    }

    pub fn wait(&self, timeout: Option<Duration>) -> Result<i32, String> {
        match &self.0 {
            ManagedSubprocessInner::Native(process) => {
                process.wait(timeout).map_err(|error| error.to_string())
            }
            ManagedSubprocessInner::Suspended(process) => {
                process.wait(timeout).map_err(|error| error.to_string())
            }
        }
    }

    pub fn kill(&self) -> Result<(), String> {
        match &self.0 {
            ManagedSubprocessInner::Native(process) => {
                process.kill().map_err(|error| error.to_string())
            }
            ManagedSubprocessInner::Suspended(process) => {
                process.kill().map_err(|error| error.to_string())
            }
        }
    }

    pub fn returncode(&self) -> Option<i32> {
        match &self.0 {
            ManagedSubprocessInner::Native(process) => process.returncode(),
            ManagedSubprocessInner::Suspended(process) => process.poll().ok().flatten(),
        }
    }

    pub fn read_stdout(&self, timeout: Option<Duration>) -> ReadStatus<Vec<u8>> {
        match &self.0 {
            ManagedSubprocessInner::Native(process) => {
                process.read_stream(StreamKind::Stdout, timeout)
            }
            ManagedSubprocessInner::Suspended(process) => process.read_stdout(timeout),
        }
    }
}

fn use_suspended_spawn(capture: bool, stdout_terminal: bool, stderr_terminal: bool) -> bool {
    host::WINDOWS && (capture || !stdout_terminal || !stderr_terminal)
}

/// Translate an argv-style launch into the most compatible `CommandSpec` for
/// the current platform.
///
/// On Windows, when `argv[0]` is a `.cmd` / `.bat` script we route through
/// `cmd.exe` (see module docs) by emitting [`CommandSpec::Shell`]. Native
/// executables and every POSIX target stay as plain `Argv` so we keep the
/// well-defined argv contract end-to-end.
pub fn command_spec_for_subprocess(argv: Vec<String>) -> CommandSpec {
    let argv = with_real_git_or_gh(argv);
    if let Some(command) = imp::batch_shell_command(&argv) {
        return CommandSpec::Shell(command);
    }

    CommandSpec::Argv(argv)
}

/// Issue #1486: clud's own `git` / `gh` spawns (GC probes, `worktrees.rs`,
/// grind, `pr_merge_watch` lookups) bypass the session's telemetry alias:
/// no extra process hop and no telemetry line for clud's own maintenance.
/// A bare `git` / `gh` program
/// becomes the session's real target when one is set
/// ([`crate::shim_registry::real_program`]); anything else is untouched.
fn with_real_git_or_gh(mut argv: Vec<String>) -> Vec<String> {
    if let Some(first) = argv.first_mut() {
        if first == "git" || first == "gh" {
            if let Some(real) = crate::shim_registry::real_program(first) {
                *first = real.to_string_lossy().into_owned();
            }
        }
    }
    argv
}

/// Return whether this argv will be launched through `cmd.exe` on Windows.
///
/// Callers use this to distinguish native backend processes from BatBadBat
/// `.cmd` / `.bat` wrappers. The distinction matters during Ctrl+C teardown:
/// sending Ctrl+Break to a batch-wrapper `cmd.exe` can leave the user at the
/// blocking `Terminate batch job (Y/N)?` prompt, while native executables can
/// still use the cooperative break before the hard tree kill.
pub fn argv_is_batch_wrapped(argv: &[String]) -> bool {
    imp::is_batch_wrapper(argv.first().map(String::as_str))
}

#[cfg(test)]
mod tests {
    use super::{argv_is_batch_wrapped, command_spec_for_subprocess, use_suspended_spawn};

    #[test]
    fn suspended_spawn_selection_matches_the_pipe_ownership_boundary() {
        if crate::platform::host::WINDOWS {
            assert!(use_suspended_spawn(true, true, true));
            assert!(use_suspended_spawn(false, false, false));
            assert!(use_suspended_spawn(false, false, true));
            assert!(use_suspended_spawn(false, true, false));
            assert!(!use_suspended_spawn(false, true, true));
        } else {
            assert!(!use_suspended_spawn(true, false, false));
        }
    }

    #[test]
    fn argv_is_batch_wrapped_false_for_native_and_empty_argv() {
        assert!(!argv_is_batch_wrapped(&["claude.exe".into()]));
        assert!(!argv_is_batch_wrapped(&["claude".into()]));
        assert!(!argv_is_batch_wrapped(&[]));
    }

    #[test]
    fn native_executable_stays_argv() {
        let spec =
            command_spec_for_subprocess(vec!["codex.exe".into(), "exec".into(), "hello".into()]);
        match spec {
            running_process::CommandSpec::Argv(argv) => {
                assert_eq!(argv, vec!["codex.exe", "exec", "hello"]);
            }
            other => panic!("expected Argv for native executable, got {other:?}"),
        }
    }
}

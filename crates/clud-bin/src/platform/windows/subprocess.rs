//! Windows half of `crate::subprocess`: the race-free suspended spawn and
//! the `.cmd` / `.bat` rewrite through `cmd.exe` (see that module's docs).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command; // running-process: command-builder
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use running_process::ReadStatus;

pub struct SuspendedSubprocess {
    pid: u32,
    child: Mutex<SuspendedChildState>,
    capture: Arc<CaptureState>,
}

struct SuspendedChildState {
    child: Option<running_process::SpawnedChild>,
    returncode: Option<i32>,
}

#[derive(Default)]
struct CaptureState {
    inner: Mutex<CaptureInner>,
    ready: Condvar,
}

#[derive(Default)]
struct CaptureInner {
    stdout: VecDeque<Vec<u8>>,
    readers_open: usize,
}

impl SuspendedSubprocess {
    pub fn spawn(
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Option<Vec<(String, String)>>,
        capture: bool,
        inherit_stdin: bool,
    ) -> std::io::Result<Self> {
        let mut command = command_for_suspended_spawn(&argv)?;
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let environment_policy = match env {
            Some(env) => {
                command.envs(env);
                running_process::EnvironmentPolicy::Clear
            }
            None => running_process::EnvironmentPolicy::Inherit,
        };
        let pipe = if capture {
            running_process::StdioSource::Pipe
        } else {
            running_process::StdioSource::Parent
        };
        let stdio = running_process::SpawnStdio {
            stdin: if inherit_stdin {
                running_process::StdioSource::Parent
            } else {
                running_process::StdioSource::Null
            },
            stdout: pipe,
            stderr: if capture {
                running_process::StdioSource::Pipe
            } else {
                running_process::StdioSource::Parent
            },
            drain_timeout: Some(Duration::from_secs(2)),
            // This branch owns pipes rather than an interactive console. A
            // console-less child does not receive the parent's Ctrl-C event;
            // clud remains the sole cancellation owner, as in the existing
            // CREATE_NEW_PROCESS_GROUP path.
            show_console: false,
        };
        let mut child =
            running_process::spawn_with_env_policy(&mut command, stdio, environment_policy)?;
        let state = Arc::new(CaptureState::default());
        if capture {
            let stdout = child.stdout.take().expect("captured stdout missing");
            let stderr = child.stderr.take().expect("captured stderr missing");
            state
                .inner
                .lock()
                .expect("capture mutex poisoned")
                .readers_open = 2;
            spawn_capture_reader(stdout, Arc::clone(&state));
            spawn_capture_reader(stderr, Arc::clone(&state));
        }
        let pid = child.id();
        Ok(Self {
            pid,
            child: Mutex::new(SuspendedChildState {
                child: Some(child),
                returncode: None,
            }),
            capture: state,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn poll(&self) -> std::io::Result<Option<i32>> {
        let mut state = self.child.lock().expect("child mutex poisoned");
        if let Some(code) = state.returncode {
            return Ok(Some(code));
        }
        let Some(child) = state.child.as_mut() else {
            return Ok(None);
        };
        let result = child.try_wait()?;
        if let Some(code) = result {
            state.returncode = Some(code);
            // The direct backend has finished, so its clud session is over.
            // Close the kill-on-close Job now, rather than waiting for this
            // wrapper to drop: contained descendants may still own inherited
            // capture writers, and the renderer cannot observe EOF until they
            // are terminated. Any bytes already written remain in the kernel
            // pipe buffers for the reader threads to drain.
            drop(state.child.take());
        }
        Ok(result)
    }

    pub fn wait(&self, timeout: Option<Duration>) -> std::io::Result<i32> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            if let Some(code) = self.poll()? {
                return Ok(code);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for child",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn kill(&self) -> std::io::Result<()> {
        let mut state = self.child.lock().expect("child mutex poisoned");
        match state.child.as_mut() {
            Some(child) => child.kill(),
            None => Ok(()),
        }
    }

    pub fn read_stdout(&self, timeout: Option<Duration>) -> ReadStatus<Vec<u8>> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut inner = self.capture.inner.lock().expect("capture mutex poisoned");
        loop {
            if let Some(line) = inner.stdout.pop_front() {
                return ReadStatus::Line(line);
            }
            if inner.readers_open == 0 {
                return ReadStatus::Eof;
            }
            inner = match deadline {
                Some(deadline) => {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        return ReadStatus::Timeout;
                    };
                    let (guard, result) = self
                        .capture
                        .ready
                        .wait_timeout(inner, remaining)
                        .expect("capture mutex poisoned");
                    if result.timed_out() {
                        return ReadStatus::Timeout;
                    }
                    guard
                }
                None => self
                    .capture
                    .ready
                    .wait(inner)
                    .expect("capture mutex poisoned"),
            };
        }
    }
}

fn spawn_capture_reader(reader: impl std::io::Read + Send + 'static, state: Arc<CaptureState>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    state
                        .inner
                        .lock()
                        .expect("capture mutex poisoned")
                        .stdout
                        .push_back(line);
                    state.ready.notify_all();
                }
            }
        }
        let mut inner = state.inner.lock().expect("capture mutex poisoned");
        inner.readers_open = inner.readers_open.saturating_sub(1);
        state.ready.notify_all();
    });
}

fn command_for_suspended_spawn(argv: &[String]) -> std::io::Result<Command> {
    let Some(program) = argv.first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty subprocess argv",
        ));
    };
    if is_batch_wrapper(Some(program)) {
        let comspec = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        // running-process: command-builder
        let mut command = Command::new(comspec);
        command.args(["/D", "/S", "/C", &render_windows_batch_command(argv)]);
        Ok(command)
    } else {
        // running-process: command-builder
        let mut command = Command::new(program);
        command.args(&argv[1..]);
        Ok(command)
    }
}

/// The `cmd.exe` shell command that launches `argv`, when `argv[0]` is a
/// `.cmd` / `.bat` wrapper (issue #59); `None` for a native executable.
pub fn batch_shell_command(argv: &[String]) -> Option<String> {
    is_batch_wrapper(argv.first().map(String::as_str)).then(|| render_windows_batch_command(argv))
}

/// True when `program` ends in `.cmd` or `.bat` (case-insensitive).
pub fn is_batch_wrapper(program: Option<&str>) -> bool {
    let Some(program) = program else {
        return false;
    };
    matches!(
        Path::new(program)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("cmd" | "bat")
    )
}

/// Build the inner `<bat-path> <args>` command string that goes inside
/// `cmd /D /S /C "..."`. Each token is quoted independently with
/// [`quote_for_cmd`].
///
/// `running-process-core::shell_command` already prepends `cmd /D /S /C "`
/// (and appends the matching close-quote) using `raw_arg`, so we only need
/// to render the *contents* of that outer quoted region.
///
/// The flags it picks are intentional:
///
/// * `/D` skips `AutoRun` so a user `cmd.exe /K ...` registry key cannot
///   inject extra commands ahead of ours.
/// * `/S` makes cmd's quote handling predictable: the *outermost* `"..."`
///   pair is stripped verbatim and what's inside is parsed as a normal
///   command line. Without `/S`, cmd applies a different "first-and-last
///   quote on the line" rule that breaks on paths with spaces.
/// * `/C` runs the command and exits.
///
/// The rendered command is prefixed with `chcp 65001 > nul &` so the
/// shim and its descendants run under codepage 65001 (UTF-8). Without
/// this, cmd.exe inherits the user's OEM/ANSI codepage (CP437 / CP1252
/// / CP932 / …) and any non-ASCII bytes written by Node-based agents
/// (Claude, Codex) get mojibaked on the way out. `> nul` swallows the
/// "Active code page: 65001" banner so the user's session stays clean.
/// `&` (not `&&`) keeps the launch going even if a hardened
/// configuration refuses the codepage switch — the worst case is then
/// the same as before this change, not a hard failure (issue #168).
fn render_windows_batch_command(argv: &[String]) -> String {
    let cmd = argv
        .iter()
        .map(|arg| quote_for_cmd(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!("chcp 65001 > nul & {cmd}")
}

/// Quote one argv token for cmd.exe.
///
/// Strategy: wrap the token in `"..."` and escape the two characters that
/// matter inside a double-quoted region:
///
/// * `"` → `""`. Inside a quoted block, cmd.exe treats a doubled quote as a
///   single embedded literal quote.
/// * `%` → `%%`. cmd.exe expands `%FOO%` *before* it strips the outer
///   quotes, even inside `"..."`. Doubling the `%` defangs that pass.
///
/// Other shell metacharacters (`& | < > ^ ( ) ;`) only matter outside quotes;
/// once the whole argument is wrapped in `"..."`, cmd treats them as literal
/// bytes and forwards them to the batch file unchanged.
///
/// `!` is special only when delayed expansion is enabled (`setlocal
/// enabledelayedexpansion`), which is off by default. We intentionally do not
/// escape it: doing so would corrupt prompts that legitimately contain `!`,
/// and a `.cmd` shim that turned delayed expansion on for itself would
/// already break in the same way regardless of how we quote.
fn quote_for_cmd(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for ch in arg.chars() {
        match ch {
            '%' => out.push_str("%%"),
            '"' => out.push_str("\"\""),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use crate::subprocess::{argv_is_batch_wrapped, command_spec_for_subprocess};

    fn start_windows(args: &[&str], capture: bool) -> crate::subprocess::ManagedSubprocess {
        let mut argv = vec!["cmd.exe".to_string()];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        crate::subprocess::ManagedSubprocess::start(
            argv,
            None,
            std::env::vars().collect(),
            capture,
            Some(crate::win_creation_flags::CREATE_NEW_PROCESS_GROUP),
        )
        .expect("race-free contained spawn")
    }
    #[test]
    fn suspended_spawn_covers_success_and_child_nonzero() {
        let success = start_windows(&["/D", "/S", "/C", "exit /b 0"], true);
        assert_eq!(success.wait(Some(std::time::Duration::from_secs(5))), Ok(0));

        let nonzero = start_windows(&["/D", "/S", "/C", "exit /b 23"], true);
        assert_eq!(
            nonzero.wait(Some(std::time::Duration::from_secs(5))),
            Ok(23)
        );
    }
    #[test]
    fn suspended_spawn_surfaces_spawn_failure() {
        let result = crate::subprocess::ManagedSubprocess::start(
            vec![r"Z:\clud-definitely-missing\missing.exe".to_string()],
            None,
            std::env::vars().collect(),
            true,
            Some(crate::win_creation_flags::CREATE_NEW_PROCESS_GROUP),
        );
        assert!(result.is_err(), "a missing executable must fail at spawn");
    }
    #[test]
    fn suspended_spawn_stream_capture_reaches_eof() {
        let process = start_windows(
            &["/D", "/S", "/C", "echo capture-line & echo error-line 1>&2"],
            true,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut output = Vec::new();
        loop {
            match process.read_stdout(Some(std::time::Duration::from_millis(100))) {
                running_process::ReadStatus::Line(line) => output.extend(line),
                running_process::ReadStatus::Eof => break,
                running_process::ReadStatus::Timeout if std::time::Instant::now() < deadline => {}
                running_process::ReadStatus::Timeout => panic!("capture did not reach EOF"),
            }
        }
        assert_eq!(process.wait(Some(std::time::Duration::from_secs(5))), Ok(0));
        let output = String::from_utf8_lossy(&output);
        assert!(
            output.contains("capture-line"),
            "stdout missing: {output:?}"
        );
        assert!(output.contains("error-line"), "stderr missing: {output:?}");
    }
    #[test]
    fn suspended_spawn_cancellation_is_bounded() {
        let process = start_windows(&["/D", "/S", "/C", "ping -n 30 127.0.0.1 > nul"], true);
        assert_eq!(process.poll().expect("poll"), None);
        process.kill().expect("cancel direct child");
        assert!(
            process
                .wait(Some(std::time::Duration::from_secs(5)))
                .is_ok(),
            "cancelled child must become waitable"
        );
    }
    #[test]
    fn argv_is_batch_wrapped_true_for_cmd_and_bat_on_windows() {
        assert!(argv_is_batch_wrapped(&[
            r"C:\Users\me\AppData\Roaming\npm\claude.cmd".into()
        ]));
        assert!(argv_is_batch_wrapped(&[
            r"C:\tools\thing.BAT".into(),
            "go".into()
        ]));
    }

    // ---- Windows-only: `.cmd` / `.bat` wrapping (issue #59) ----
    //
    // We expect every Windows batch backend to come out as a `Shell` spec
    // that running-process-core will hand to `cmd /D /S /C "..."`.
    #[test]
    fn cmd_wrapper_with_no_args_routes_through_shell() {
        // Just the `.cmd` path with no extra args: the rendered command is
        // the UTF-8 codepage prefix followed by the quoted exe path.
        let spec =
            command_spec_for_subprocess(vec![r"C:\Users\me\AppData\Roaming\npm\codex.cmd".into()]);
        match spec {
            running_process::CommandSpec::Shell(command) => {
                assert_eq!(
                    command,
                    r#"chcp 65001 > nul & "C:\Users\me\AppData\Roaming\npm\codex.cmd""#
                );
            }
            other => panic!("expected Shell for .cmd wrapper, got {other:?}"),
        }
    }
    #[test]
    fn cmd_wrapper_quotes_args_with_spaces() {
        let spec = command_spec_for_subprocess(vec![
            r"C:\path with spaces\codex.cmd".into(),
            "exec".into(),
            "hello world".into(),
        ]);
        let rendered = match spec {
            running_process::CommandSpec::Shell(s) => s,
            other => panic!("expected Shell, got {other:?}"),
        };
        assert!(
            rendered.contains(r#""C:\path with spaces\codex.cmd" "exec" "hello world""#),
            "spaces in exe path and arg must each be wrapped in quotes: {rendered}"
        );
    }

    /// Issue #168: every cmd.exe wrapping must force codepage 65001 so
    /// Node-based agents (Claude / Codex) emit and consume UTF-8 instead
    /// of mojibaking under the user's OEM/ANSI codepage.
    #[test]
    fn cmd_wrapper_prepends_utf8_codepage_switch() {
        let spec = command_spec_for_subprocess(vec![
            r"C:\Users\me\AppData\Roaming\npm\codex.cmd".into(),
            "exec".into(),
            "hello".into(),
        ]);
        let rendered = match spec {
            running_process::CommandSpec::Shell(s) => s,
            other => panic!("expected Shell, got {other:?}"),
        };
        assert!(
            rendered.starts_with("chcp 65001 > nul & "),
            "every cmd.exe shim must start with the UTF-8 codepage switch: {rendered}"
        );
    }
    #[test]
    fn cmd_wrapper_escapes_shell_metacharacters() {
        // `&`, `|`, `<`, `>`, `^` are cmd metacharacters outside quoted
        // regions. Wrapping each token in `"..."` plus the outer
        // `cmd /S /C "..."` rule keeps them literal so the batch file
        // receives them byte-for-byte.
        let spec = command_spec_for_subprocess(vec![
            r"C:\codex.cmd".into(),
            "fix this & that".into(),
            "left | right".into(),
            "redirect < input > output".into(),
            "caret ^ stays".into(),
        ]);
        let rendered = match spec {
            running_process::CommandSpec::Shell(s) => s,
            other => panic!("expected Shell, got {other:?}"),
        };
        assert!(
            rendered.contains(r#""fix this & that""#),
            "& must stay inside quotes: {rendered}"
        );
        assert!(
            rendered.contains(r#""left | right""#),
            "| must stay inside quotes: {rendered}"
        );
        assert!(
            rendered.contains(r#""redirect < input > output""#),
            "redirection chars must stay inside quotes: {rendered}"
        );
        assert!(
            rendered.contains(r#""caret ^ stays""#),
            "^ must stay inside quotes: {rendered}"
        );
        // Sanity: there must be no occurrence of an unescaped metacharacter
        // appearing *outside* the quoted regions. We approximate this by
        // checking that every metachar shows up only between matching quotes.
        for needle in ["&", "|", "<", ">", "^"] {
            // Each needle should appear at least once (it's in our input);
            // the surrounding `" `... `"` from the join contract is the
            // assurance it stays inside.
            assert!(
                rendered.contains(needle),
                "expected `{needle}` to appear in rendered command: {rendered}"
            );
        }
    }
    #[test]
    fn cmd_wrapper_escapes_percent_and_quote() {
        // `%` would otherwise expand env vars before the batch sees it; `"`
        // needs to be doubled to embed inside a quoted token.
        let spec = command_spec_for_subprocess(vec![
            r"C:\codex.cmd".into(),
            "exec".into(),
            r#"100% "quoted" prompt"#.into(),
        ]);
        let rendered = match spec {
            running_process::CommandSpec::Shell(s) => s,
            other => panic!("expected Shell, got {other:?}"),
        };
        assert!(
            rendered.contains(r#""100%% ""quoted"" prompt""#),
            "percent and quote escaping is wrong: {rendered}"
        );
    }
    #[test]
    fn bat_extension_is_also_wrapped() {
        // The check is case-insensitive: `.BAT`, `.Cmd`, etc. all have to
        // hit the cmd.exe path.
        let spec = command_spec_for_subprocess(vec![r"C:\tools\thing.BAT".into(), "go".into()]);
        match spec {
            running_process::CommandSpec::Shell(_) => {}
            other => panic!("expected Shell for .BAT wrapper, got {other:?}"),
        }
    }
    #[test]
    fn exe_path_is_not_wrapped() {
        // `.exe` is a native executable; it must stay argv-based so we don't
        // pay the cmd.exe parsing tax (and don't regress any quoting that
        // already worked for non-batch backends).
        let spec = command_spec_for_subprocess(vec![
            r"C:\Program Files\Tool\codex.exe".into(),
            "exec".into(),
            "hello".into(),
        ]);
        match spec {
            running_process::CommandSpec::Argv(argv) => {
                assert_eq!(
                    argv,
                    vec![r"C:\Program Files\Tool\codex.exe", "exec", "hello"],
                    ".exe paths must not be cmd-wrapped"
                );
            }
            other => panic!("expected Argv passthrough for .exe, got {other:?}"),
        }
    }
    #[test]
    fn extensionless_path_is_not_wrapped() {
        // PATH lookup may hand us a bare program name on Windows too (e.g.
        // for .exe files where the extension was already stripped by an
        // earlier resolver). These must not be wrapped.
        let spec = command_spec_for_subprocess(vec!["codex".into(), "exec".into()]);
        match spec {
            running_process::CommandSpec::Argv(argv) => {
                assert_eq!(argv, vec!["codex", "exec"]);
            }
            other => panic!("expected Argv for extensionless path, got {other:?}"),
        }
    }
}

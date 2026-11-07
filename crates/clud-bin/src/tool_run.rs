//! `clud tool run <rel_path> [args]` — Layer 1 of the three-layer
//! `UV_CACHE_DIR` enforcement from issue #408.
//!
//! Resolves a tool installed under `~/.clud/tools/<rel_path>`, reads the
//! file, and chooses the runner from its contents. Plain Python uses clud's
//! managed Python under `~/.clud/tools/python`; uv script mode is used only
//! when the shebang explicitly asks for it. `UV_CACHE_DIR` is pinned to
//! [`tools::clud_uv_cache_dir`]: the `clud tool` dispatch in `main.rs` sets it
//! before this runs, and the read-or-default below falls back to the same
//! value. Since #1964 nothing pins it for the rest of a clud session.
//!
//! Surfaces the inner runner exit code as the subcommand's exit code so
//! callers can chain on success/failure.
//!
//! File mode is irrelevant; clud invokes the interpreter explicitly so no
//! `chmod +x` is required.
//!
//! Subprocess execution goes through [`running_process::NativeProcess`]
//! per the repo-wide lint rule that bans direct `std::process::Command`
//! use.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ProcessError, StderrMode, StdinMode,
};
use serde::Serialize;

use crate::log_event::{ENV_DAEMON_HTTP_SERVER, ENV_DAEMON_HTTP_TOKEN};
use crate::process_identity::{start_time_of, UNKNOWN_START_TIME};
use crate::session_index::{
    allocate_next_id, append_event, unix_millis_now, IndexEvent, SessionContext,
};
use crate::shim_uv;
use crate::tool_install::{ensure_installed as ensure_tools_installed, tools_root};
use crate::tool_tee::TeeWriter;
use crate::tool_termination::{emit_termination, format_elapsed, ExitKind};
use crate::tool_watchdog::{AbortReason, Watchdog, WatchdogDecision, WATCHDOG_EXIT_CODE};
use crate::tools::clud_uv_cache_dir;

/// Poll interval for draining captured stdout/stderr into the tee writer.
/// 100ms keeps the visual feel of live output without burning CPU on
/// idle tools; the captured-output API is lock-cheap so this is fine.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MANAGED_PYTHON_VERSION: &str = "3.11";
const MANAGED_PYTHON_DIR: &str = "python";
const TEXT_SNIFF_LIMIT: usize = 8192;
const TOOL_TELEMETRY_TIMEOUT: Duration = Duration::from_millis(50);

struct ToolRunState {
    telemetry: ToolTelemetry,
    watchdog: Watchdog,
}

/// Resolve and execute a bundled tool by relative path. Returns the
/// inner `uv` process's exit code so the CLI can surface it verbatim.
///
/// Errors:
/// - The home directory cannot be resolved (returns `Other`).
/// - The tool file doesn't exist under `~/.clud/tools/` (returns
///   `NotFound`). This usually means `rel_path` is not a registered
///   `BUNDLED_TOOLS` entry; `ensure_installed` is called inline before the
///   existence check, so a missing-but-registered tool implies the install
///   itself failed (a `[clud] note: …` line was printed to stderr).
/// - `uv` isn't on `PATH` or could not start (returns the underlying
///   running-process error wrapped in `Other`).
/// - `uv` ran but reported a non-zero exit — that's surfaced as the `Ok`
///   value, not as `Err`. Callers should treat the `Ok` case as "uv ran"
///   and inspect the exit code to know whether the tool succeeded.
pub fn run(rel_path: &str, args: &[String]) -> io::Result<i32> {
    let tools_root = tools_root()
        .ok_or_else(|| io::Error::other("could not resolve home directory for ~/.clud/tools/"))?;
    let tool_path = resolve_tool_path(&tools_root, rel_path);

    // Self-heal the bundled-tools install in this process.
    //
    // Tool invocations must not bootstrap or contact the daemon. They are
    // used heavily from agent hooks, and starting another clud-owned daemon
    // path for every hook fire pollutes process managers and adds avoidable
    // latency. Inline install is cheap on the steady state: one stat + read
    // per tool, no writes when content matches.
    ensure_tools_installed();

    if !tool_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "bundled tool not found at {}; either `{}` is not a \
                 registered BUNDLED_TOOLS entry, or the inline \
                 `ensure_installed` failed (see prior `[clud] note: …` line)",
                tool_path.display(),
                rel_path,
            ),
        ));
    }

    let tool_bytes = fs::read(&tool_path)?;
    let cache_dir = resolved_uv_cache_dir_from(std::env::var_os("UV_CACHE_DIR"));
    let mut env: Vec<(String, String)> = build_child_env(&cache_dir);
    let mut watchdog = watchdog_for_tool(rel_path, &tool_path, &tools_root);
    let requested = std::env::var("CLUD_PR_MERGE_WATCH_TIMEOUT").ok();
    let requested = watcher_deadline(rel_path, args, requested.as_deref())?;
    // Debug-only integration fixtures deliberately shorten the wrapper cap.
    #[cfg(debug_assertions)]
    let test_override = std::env::var("CLUD_TEST_TOOL_COMMAND_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .is_some();
    #[cfg(not(debug_assertions))]
    let test_override = false;
    if !test_override {
        if let Some(deadline) = requested {
            watchdog.command_timeout = deadline;
        }
    }
    if watchdog.command_timeout != Duration::MAX {
        env.push((
            "CLUD_TOOL_COMMAND_TIMEOUT_SECS".to_string(),
            watchdog.command_timeout.as_secs().to_string(),
        ));
    }
    let argv = build_tool_argv(&tool_path, &tool_bytes, args, &tools_root, &env)?;
    let run_state = ToolRunState {
        telemetry: ToolTelemetry::start(rel_path),
        watchdog,
    };

    // Resolve session context up front. None means no daemon / CI fallback;
    // we run the tool in plain passthrough mode (no capture, no tee, no
    // index — same behavior as before #427 slice 1).
    let session_ctx = SessionContext::from_env();
    let tool_id = session_ctx
        .as_ref()
        .and_then(|ctx| allocate_next_id(ctx).ok());

    // Two execution modes:
    //   - With session: capture + tee + index lifecycle events.
    //   - Without session: original passthrough (no capture, no tee).
    // The session-less path keeps `clud tool run` runnable in CI / minimal
    // containers where the daemon isn't present.
    match (session_ctx.as_ref(), tool_id) {
        (Some(ctx), Some(tool_id)) => {
            run_with_session(ctx, tool_id, rel_path, args, argv, env, run_state)
        }
        _ => run_passthrough(rel_path, args, argv, env, run_state),
    }
}

/// Give the PR watcher time to finish its requested wait and emit a timeout.
/// Reject oversized requests instead of silently clamping them at the wrapper.
fn watcher_deadline(
    rel_path: &str,
    args: &[String],
    configured: Option<&str>,
) -> io::Result<Option<Duration>> {
    if rel_path != "github/pr_merge_watch.py" {
        return Ok(None);
    }
    const DEFAULT_SECS: u64 = 3600;
    const MAX_SECS: u64 = 86400;
    const EXIT_MARGIN_SECS: u64 = 60;
    let mut value = configured;
    let mut explicit = false;
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--")
        .collect();
    let mut index = 0;
    while index < args.len() {
        let argument = args[index];
        let (option, attached) = argument
            .split_once('=')
            .map_or((argument, None), |(option, value)| (option, Some(value)));
        // argparse allows unambiguous long-option prefixes. `--t` is
        // ambiguous with --triggers-for-base; `--ti` and longer are unique.
        if option.len() >= 4 && "--timeout".starts_with(option) {
            explicit = true;
            if let Some(attached) = attached {
                value = Some(attached);
            } else {
                value = args.get(index + 1).copied();
                index += 1;
            }
        }
        index += 1;
    }
    let seconds = match value {
        None => DEFAULT_SECS,
        Some(raw) => {
            let raw = raw.trim();
            let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
            let valid_digits = !digits.is_empty()
                && !digits.starts_with('_')
                && !digits.ends_with('_')
                && !digits.contains("__")
                && digits
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'_');
            if !valid_digits {
                if !explicit
                    && !raw
                        .chars()
                        .any(|character| !character.is_ascii() && character.is_numeric())
                {
                    return Ok(Some(Duration::from_secs(DEFAULT_SECS + EXIT_MARGIN_SECS)));
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid PR watcher timeout",
                ));
            }
            let digits = digits.replace('_', "");
            if raw.starts_with('-') {
                0
            } else {
                let seconds = digits.parse::<u64>().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "PR watcher timeout exceeds 86400 seconds",
                    )
                })?;
                if seconds > MAX_SECS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "PR watcher timeout exceeds 86400 seconds",
                    ));
                }
                seconds
            }
        }
    };
    Ok(Some(Duration::from_secs(seconds + EXIT_MARGIN_SECS)))
}

/// Plain passthrough: child stdout/stderr are captured only long enough to
/// immediately re-emit them and retain a stderr tail for tool telemetry.
/// There is still no session index or per-tool log in this fallback path.
fn run_passthrough(
    rel_path: &str,
    args: &[String],
    argv: Vec<String>,
    env: Vec<(String, String)>,
    run_state: ToolRunState,
) -> io::Result<i32> {
    let ToolRunState {
        telemetry,
        mut watchdog,
    } = run_state;
    let argv_for_diagnostic = argv.clone();
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().map_err(io::Error::other)?;
    #[cfg(test)]
    prepare_fixture_observer(&process, rel_path);
    let mut emitted = 0usize;
    let mut last_drain_count = 0usize;
    let mut progress_uncertain = false;
    #[cfg(test)]
    let mut fixture_timeout_forced = false;
    let (exit_code, delayed) = loop {
        #[cfg(test)]
        let wait_result = fixture_wait(&process, rel_path, &mut fixture_timeout_forced);
        #[cfg(not(test))]
        let wait_result = process.wait(Some(DRAIN_POLL_INTERVAL));
        match wait_result {
            Ok(code) => {
                #[cfg(test)]
                if rel_path == "__test_buffered_progress" {
                    assert!(progress_uncertain, "the buffered-progress path must run");
                    assert_eq!(code, 0, "fixture must finish successfully");
                }
                let delayed = delayed_observation(&watchdog)
                    .or(progress_uncertain.then_some(AbortReason::ProgressTimeout));
                break (code, delayed);
            }
            Err(ProcessError::Timeout) => {
                emitted = drain_passthrough_output(&process, emitted);
                if emitted > last_drain_count {
                    note_observed_progress(&mut watchdog, &mut progress_uncertain);
                    #[cfg(test)]
                    release_buffered_fixture(rel_path, args, progress_uncertain);
                    last_drain_count = emitted;
                }
                match watchdog.check() {
                    WatchdogDecision::Continue => continue,
                    WatchdogDecision::KillAndAbort(reason) => {
                        let _ = process.kill();
                        let _ = drain_passthrough_output(&process, emitted);
                        let stderr_tail = stderr_tail_200(&process.captured_stderr().concat());
                        let _ = emit_passthrough_abort_diagnostic(
                            rel_path,
                            args,
                            &argv_for_diagnostic,
                            watchdog.started_at.elapsed(),
                            reason,
                            stderr_tail.as_deref(),
                        );
                        telemetry.finish(WATCHDOG_EXIT_CODE, stderr_tail);
                        return Ok(WATCHDOG_EXIT_CODE);
                    }
                    WatchdogDecision::ResumeLater(reason) => {
                        let _ = drain_passthrough_output(&process, emitted);
                        let mut err = io::stderr().lock();
                        let _ = writeln!(err, "{}", watchdog.render_in_progress(reason));
                        let _ = err.flush();
                        telemetry.finish(
                            WATCHDOG_EXIT_CODE,
                            stderr_tail_200(&process.captured_stderr().concat()),
                        );
                        return Ok(WATCHDOG_EXIT_CODE);
                    }
                }
            }
            Err(other) => return Err(io::Error::other(other)),
        }
    };
    let _ = drain_passthrough_output(&process, emitted);
    let reported_code = uncertain_exit_code(exit_code, delayed);
    if let Some(reason) = delayed {
        let payload = serde_json::json!({
            "v": 1,
            "status": "observation-delayed",
            "reason": reason.label(),
            "deadline_outcome": "unknown",
            "exit_code": exit_code,
        });
        let _ = writeln!(io::stderr().lock(), "{payload}");
    }
    telemetry.finish(
        reported_code,
        stderr_tail_200(&process.captured_stderr().concat()),
    );
    Ok(reported_code)
}

#[cfg(test)]
fn release_buffered_fixture(rel_path: &str, args: &[String], progress_uncertain: bool) {
    if rel_path == "__test_buffered_progress" && progress_uncertain {
        std::fs::write(&args[0], b"release").expect("release fixture child");
    }
}

#[cfg(test)]
fn prepare_fixture_observer(process: &NativeProcess, rel_path: &str) {
    if rel_path == "__test_delayed_observer" {
        process
            .wait(Some(Duration::from_secs(10)))
            .expect("fixture must exit");
    } else if rel_path == "__test_buffered_progress" {
        wait_for_buffered_fixture_output(process);
    }
}

#[cfg(test)]
fn fixture_wait(
    process: &NativeProcess,
    rel_path: &str,
    forced: &mut bool,
) -> Result<i32, ProcessError> {
    if rel_path == "__test_buffered_progress" && !*forced {
        assert!(
            process.returncode().is_none(),
            "fixture must still be running"
        );
        *forced = true;
        Err(ProcessError::Timeout)
    } else {
        process.wait(Some(DRAIN_POLL_INTERVAL))
    }
}

#[cfg(test)]
fn wait_for_buffered_fixture_output(process: &NativeProcess) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let marker = b"buffered progress fixture";
    let has_marker = || {
        process.captured_combined().iter().any(|event| {
            event
                .line
                .windows(marker.len())
                .any(|window| window == marker)
        })
    };
    while !has_marker() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(has_marker(), "fixture marker must be buffered");
    assert!(
        process.returncode().is_none(),
        "fixture must still be running"
    );
}

/// A late observation cannot establish when the child exited or last wrote output.
fn delayed_observation(watchdog: &Watchdog) -> Option<AbortReason> {
    match watchdog.check() {
        WatchdogDecision::Continue => None,
        WatchdogDecision::KillAndAbort(reason) | WatchdogDecision::ResumeLater(reason) => {
            Some(reason)
        }
    }
}

/// Buffered output has no producer timestamp; a late drain cannot prove when it arrived.
fn note_observed_progress(watchdog: &mut Watchdog, uncertain: &mut bool) {
    if matches!(
        delayed_observation(watchdog),
        Some(AbortReason::ProgressTimeout)
    ) {
        *uncertain = true;
    }
    watchdog.note_output();
}

fn uncertain_exit_code(exit_code: i32, delayed: Option<AbortReason>) -> i32 {
    if exit_code == 0 && delayed.is_some() {
        WATCHDOG_EXIT_CODE
    } else {
        exit_code
    }
}

fn render_passthrough_abort_payload(
    rel_path: &str,
    args: &[String],
    argv: &[String],
    elapsed: Duration,
    reason: AbortReason,
    stderr_tail: Option<&str>,
) -> String {
    serde_json::json!({
        "v": 1,
        "tool": rel_path,
        "args": crate::secret_redaction::redact_args(args),
        "argv": crate::secret_redaction::redact_args(argv),
        "status": "aborted",
        "reason": reason.label(),
        "elapsed_ms": elapsed.as_millis() as u64,
        "stderr_tail": stderr_tail.map(crate::secret_redaction::redact_text),
    })
    .to_string()
}

fn emit_passthrough_abort_diagnostic(
    rel_path: &str,
    args: &[String],
    argv: &[String],
    elapsed: Duration,
    reason: AbortReason,
    stderr_tail: Option<&str>,
) -> io::Result<()> {
    let payload =
        render_passthrough_abort_payload(rel_path, args, argv, elapsed, reason, stderr_tail);
    let safe_argv = crate::secret_redaction::redact_args(argv);
    let argv_json = serde_json::to_string(&safe_argv).unwrap_or_else(|_| format!("{safe_argv:?}"));
    let mut err = io::stderr().lock();
    writeln!(err, "{payload}")?;
    writeln!(
        err,
        "TIMEOUT after {} on {} in session-less tool runner.",
        format_elapsed(elapsed),
        rel_path
    )?;
    writeln!(err, "  command argv: {argv_json}")?;
    if let Some(tail) = stderr_tail {
        writeln!(
            err,
            "  stderr tail: {}",
            crate::secret_redaction::redact_text(tail)
        )?;
    }
    err.flush()
}

/// Session-attached run: capture stdout/stderr through `running_process`,
/// poll-drain into the [`TeeWriter`] every 100ms (so the user still sees
/// live output, modulo the poll interval), and append Started/Finished
/// events to the session index.
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn run_with_session(
    ctx: &SessionContext,
    tool_id: u32,
    rel_path: &str,
    args: &[String],
    argv: Vec<String>,
    env: Vec<(String, String)>,
    run_state: ToolRunState,
) -> io::Result<i32> {
    let ToolRunState {
        telemetry,
        mut watchdog,
    } = run_state;
    // Open the per-invocation log dir + JSONL writers BEFORE starting the
    // subprocess so any open-time failure surfaces immediately (no
    // half-spawned child with no log destination).
    let log_dir = ctx.tool_log_dir(tool_id);
    let mut tee = TeeWriter::open(&log_dir)?;

    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().map_err(io::Error::other)?;
    #[cfg(test)]
    prepare_fixture_observer(&process, rel_path);

    // #1204: record the child's real OS PID and its start time so
    // `clud tool log --pid` / `clud tool info --pid` can match this
    // invocation. `NativeProcess::pid()` is only valid while the wrapper
    // still owns the child, so read it here -- before the poll-drain
    // loop -- and fall back to 0 / UNKNOWN_START_TIME when the platform
    // or a just-reaped child gives us nothing.
    let pid = process.pid().unwrap_or(0);
    let pid_start_time = if pid == 0 {
        UNKNOWN_START_TIME
    } else {
        start_time_of(pid)
    };

    let started_at_ms = unix_millis_now();
    let _ = append_event(
        ctx,
        &IndexEvent::Started {
            tool_id,
            tool: rel_path.to_string(),
            args: args.to_vec(),
            pid,
            pid_start_time,
            started_at_ms,
        },
    );

    // Slice 5 of #427: build the watchdog from the tool's BundledTool
    // entry (kill_semantics, command_timeout, progress_timeout, quiet_ok).
    // Unknown tools default to Killable + 60m + no progress watchdog.

    // Poll-drain loop. `wait(Some(100ms))` returns `Err(ProcessError::Timeout)`
    // while the child is still running; we drain captured output each
    // iteration and emit it via the tee. On `Ok` (child exited), we do a
    // final drain so any output written during the last 100ms still lands
    // in the log.
    //
    // Each iteration also asks the watchdog whether either timer has
    // fired. If so we either kill (killable) or emit an in-progress
    // terminal (resumable) and break out.
    let mut emitted = 0usize;
    let mut last_drain_count = 0usize;
    let mut progress_uncertain = false;
    #[cfg(test)]
    let mut fixture_timeout_forced = false;
    let (exit_code, delayed) = loop {
        #[cfg(test)]
        let wait_result = fixture_wait(&process, rel_path, &mut fixture_timeout_forced);
        #[cfg(not(test))]
        let wait_result = process.wait(Some(DRAIN_POLL_INTERVAL));
        match wait_result {
            Ok(code) => {
                #[cfg(test)]
                if rel_path == "__test_buffered_progress" {
                    assert!(progress_uncertain, "the buffered-progress path must run");
                    assert_eq!(code, 0, "fixture must finish successfully");
                }
                let delayed = delayed_observation(&watchdog)
                    .or(progress_uncertain.then_some(AbortReason::ProgressTimeout));
                break (code, delayed);
            }
            Err(ProcessError::Timeout) => {
                emitted = drain_into_tee(&process, &mut tee, emitted);
                if emitted > last_drain_count {
                    note_observed_progress(&mut watchdog, &mut progress_uncertain);
                    #[cfg(test)]
                    release_buffered_fixture(rel_path, args, progress_uncertain);
                    last_drain_count = emitted;
                }
                match watchdog.check() {
                    WatchdogDecision::Continue => continue,
                    WatchdogDecision::KillAndAbort(reason) => {
                        let _ = process.kill();
                        let _ = drain_into_tee(&process, &mut tee, emitted);
                        let _ = tee.flush();
                        let ended_at_ms = unix_millis_now();
                        let _ = append_event(
                            ctx,
                            &IndexEvent::Aborted {
                                tool_id,
                                reason: reason.label().to_string(),
                                ended_at_ms,
                            },
                        );
                        // Slice 6: structured JSON payload + pointer block.
                        let _ = emit_termination(
                            ctx.session_pid,
                            tool_id,
                            rel_path,
                            args,
                            started_at_ms,
                            ended_at_ms,
                            watchdog.started_at.elapsed(),
                            ExitKind::Aborted(reason),
                            None,
                        );
                        telemetry.finish(
                            WATCHDOG_EXIT_CODE,
                            stderr_tail_200(&process.captured_stderr().concat()),
                        );
                        return Ok(WATCHDOG_EXIT_CODE); // standard "command timed out" exit.
                    }
                    WatchdogDecision::ResumeLater(reason) => {
                        // Resumable: the world owns the state; the
                        // observer has not completed. Detach and exit 124 with the
                        // in-progress payload so the caller can re-invoke.
                        let _ = drain_into_tee(&process, &mut tee, emitted);
                        let _ = tee.flush();
                        let ended_at_ms = unix_millis_now();
                        let _ = append_event(
                            ctx,
                            &IndexEvent::Aborted {
                                tool_id,
                                reason: format!("resumable:{}", reason.label()),
                                ended_at_ms,
                            },
                        );
                        let _ = emit_termination(
                            ctx.session_pid,
                            tool_id,
                            rel_path,
                            args,
                            started_at_ms,
                            ended_at_ms,
                            watchdog.started_at.elapsed(),
                            ExitKind::InProgress(reason),
                            None,
                        );
                        telemetry.finish(
                            WATCHDOG_EXIT_CODE,
                            stderr_tail_200(&process.captured_stderr().concat()),
                        );
                        return Ok(WATCHDOG_EXIT_CODE);
                    }
                }
            }
            Err(other) => return Err(io::Error::other(other)),
        }
    };
    let _ = drain_into_tee(&process, &mut tee, emitted);
    let _ = tee.flush();

    let ended_at_ms = unix_millis_now();
    let event = if let Some(reason) = delayed {
        IndexEvent::ObservationDelayed {
            tool_id,
            reason: reason.label().to_string(),
            exit_code,
            ended_at_ms,
        }
    } else {
        IndexEvent::Finished {
            tool_id,
            exit_code,
            ended_at_ms,
        }
    };
    let _ = append_event(ctx, &event);

    // Slice 6: emit the structured payload + pointer block on every
    // non-zero exit so the agent always sees how to find the log.
    // We deliberately skip emission for the zero-exit happy path —
    // pointers there would be noise after a successful run.
    let exit_kind = delayed
        .map(ExitKind::ObservationDelayed)
        .or_else(|| (exit_code != 0).then_some(ExitKind::Failed));
    if let Some(kind) = exit_kind {
        let _ = emit_termination(
            ctx.session_pid,
            tool_id,
            rel_path,
            args,
            started_at_ms,
            ended_at_ms,
            watchdog.started_at.elapsed(),
            kind,
            Some(exit_code),
        );
    }

    let reported_code = uncertain_exit_code(exit_code, delayed);
    telemetry.finish(
        reported_code,
        stderr_tail_200(&process.captured_stderr().concat()),
    );
    Ok(reported_code)
}

fn drain_passthrough_output(process: &NativeProcess, emitted: usize) -> usize {
    let combined = process.captured_combined();
    for event in combined.iter().skip(emitted) {
        match event.stream {
            running_process::StreamKind::Stdout => {
                let _ = write_captured_line(&mut io::stdout().lock(), &event.line);
            }
            running_process::StreamKind::Stderr => {
                let _ = write_captured_line(&mut io::stderr().lock(), &event.line);
            }
        }
    }
    combined.len()
}

#[cfg(test)]
fn drain_passthrough_output_to(
    process: &NativeProcess,
    emitted: usize,
    stdout_writer: &mut impl Write,
    stderr_writer: &mut impl Write,
) -> usize {
    let combined = process.captured_combined();
    for event in combined.iter().skip(emitted) {
        let writer: &mut dyn Write = match event.stream {
            running_process::StreamKind::Stdout => stdout_writer,
            running_process::StreamKind::Stderr => stderr_writer,
        };
        let _ = write_captured_line(writer, &event.line);
    }
    combined.len()
}

/// `running-process` exposes captured output as logical lines with the line
/// terminator removed. Restore that delimiter before forwarding so callers
/// reading a pipe receive each line immediately instead of waiting for EOF.
fn write_captured_line(writer: &mut dyn Write, line: &[u8]) -> io::Result<()> {
    writer.write_all(line)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Drain captured combined output from `emitted..` and route it through
/// the tee writer. Returns the new emitted index. Best-effort: emit
/// errors are swallowed so a failing tee write never prevents the tool
/// from completing.
fn drain_into_tee(process: &NativeProcess, tee: &mut TeeWriter, emitted: usize) -> usize {
    let combined = process.captured_combined();
    if combined.len() > emitted {
        let _ = tee.emit_captured_batch(&combined[emitted..]);
    }
    combined.len()
}

/// Apply the installed reporter's lifetime policy to absolute paths and symlinks too.
pub(crate) fn watchdog_for_tool(rel_path: &str, tool_path: &Path, tools_root: &Path) -> Watchdog {
    let reporter = tools_root.join("diagnostics/test_report.py");
    let is_reporter = same_file::is_same_file(tool_path, &reporter).unwrap_or(false);
    Watchdog::for_rel_path(if is_reporter {
        "diagnostics/test_report.py"
    } else {
        rel_path
    })
}

/// Resolve a bundled-tool path under the supplied tools-root. Trivially
/// thin, but extracted so the join semantics can be regression-tested
/// without spinning up a process or touching the user's real home.
///
/// `tools_root` is expected to already be `~/.clud/tools/`. Callers must
/// NOT pass `~/.clud/` here — the historical pattern of calling
/// `target_path_at(tools_root.parent(), rel_path)` produced a double
/// `.clud/tools` segment and broke every real tool lookup.
fn resolve_tool_path(tools_root: &std::path::Path, rel_path: &str) -> PathBuf {
    tools_root.join(rel_path)
}

fn build_tool_argv(
    tool_path: &Path,
    tool_bytes: &[u8],
    args: &[String],
    tools_root: &Path,
    env: &[(String, String)],
) -> io::Result<Vec<String>> {
    if is_cpp_source(tool_path) {
        let exe = ensure_cpp_executable(tool_path, env)?;
        return Ok(direct_argv(&exe, args));
    }

    if looks_executable(tool_path, tool_bytes) {
        return Ok(direct_argv(tool_path, args));
    }

    let Some(tool_body) = looks_like_text(tool_bytes) else {
        return Ok(direct_argv(tool_path, args));
    };

    if let Some(shebang) = parse_shebang(&tool_body) {
        return Ok(shebang_argv(&shebang, tool_path, args));
    }

    match tool_extension(tool_path).as_deref() {
        Some("py") => {
            let python = ensure_managed_python(tools_root, env)?;
            Ok(plain_python_argv(&python, tool_path, args))
        }
        Some("sh") => Ok(interpreter_argv("sh", tool_path, args)),
        Some("bash") => Ok(interpreter_argv("bash", tool_path, args)),
        Some("ps1") => Ok(interpreter_argv("pwsh", tool_path, args)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} looks like text but has no supported shebang or extension",
                tool_path.display()
            ),
        )),
    }
}

fn uv_script_argv(tool_path: &Path, args: &[String]) -> Vec<String> {
    let mut argv = vec![
        "uv".to_string(),
        "run".to_string(),
        "--no-project".to_string(),
        "--script".to_string(),
        tool_path.to_string_lossy().into_owned(),
    ];
    argv.extend(args.iter().cloned());
    argv
}

fn direct_argv(program: &Path, args: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    argv
}

fn plain_python_argv(python: &Path, tool_path: &Path, args: &[String]) -> Vec<String> {
    interpreter_argv(&python.to_string_lossy(), tool_path, args)
}

fn interpreter_argv(interpreter: &str, tool_path: &Path, args: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(args.len() + 2);
    argv.push(interpreter.to_string());
    argv.push(tool_path.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    argv
}

fn looks_like_text(bytes: &[u8]) -> Option<String> {
    let sniff = &bytes[..bytes.len().min(TEXT_SNIFF_LIMIT)];
    if sniff.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(ToOwned::to_owned)
}

fn looks_executable(path: &Path, bytes: &[u8]) -> bool {
    if let Some("exe" | "com" | "cmd" | "bat") = tool_extension(path).as_deref() {
        return true;
    }
    bytes.starts_with(b"MZ")
        || bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xce])
        || bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
        || bytes.starts_with(&[0xce, 0xfa, 0xed, 0xfe])
        || bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
}

fn parse_shebang(tool_body: &str) -> Option<Vec<String>> {
    let first = tool_body.lines().next()?;
    let rest = first.strip_prefix("#!")?.trim();
    let parts: Vec<String> = rest.split_whitespace().map(ToOwned::to_owned).collect();
    if parts.is_empty() {
        return None;
    }
    Some(normalize_shebang_parts(parts))
}

fn normalize_shebang_parts(parts: Vec<String>) -> Vec<String> {
    let program = normalize_interpreter_path(&parts[0]);
    if program == "env" {
        let mut idx = 1;
        while idx < parts.len() {
            let flag = parts[idx].as_str();
            idx += 1;
            if flag == "-S" {
                break;
            }
            if !flag.starts_with('-') {
                idx -= 1;
                break;
            }
        }
        if idx < parts.len() {
            return parts[idx..].to_vec();
        }
        return vec!["env".to_string()];
    }

    let mut normalized = parts;
    normalized[0] = program;
    normalized
}

fn normalize_interpreter_path(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    // Windows always keeps the bare file name; elsewhere only the system bin
    // directories are stripped.
    if crate::platform::host::WINDOWS || path.starts_with("/bin/") || path.starts_with("/usr/bin/")
    {
        name.to_string()
    } else {
        path.to_string()
    }
}

fn shebang_argv(shebang: &[String], tool_path: &Path, args: &[String]) -> Vec<String> {
    if shebang.len() >= 3 && shebang[0] == "uv" && shebang[1] == "run" && shebang[2] == "--script" {
        return uv_script_argv(tool_path, args);
    }

    let mut argv = Vec::with_capacity(shebang.len() + args.len() + 1);
    argv.extend_from_slice(shebang);
    argv.push(tool_path.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    argv
}

fn tool_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
}

fn is_cpp_source(path: &Path) -> bool {
    matches!(
        tool_extension(path).as_deref(),
        Some("cpp" | "cc" | "cxx" | "c++")
    )
}

fn cpp_executable_path(source: &Path) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default();
    let exe_name = if crate::platform::host::WINDOWS {
        format!("{}.exe", stem.to_string_lossy())
    } else {
        stem.to_string_lossy().into_owned()
    };
    source.with_file_name(exe_name)
}

fn ensure_cpp_executable(source: &Path, env: &[(String, String)]) -> io::Result<PathBuf> {
    let exe = cpp_executable_path(source);
    if exe.exists() {
        return Ok(exe);
    }

    let lock_path = source.with_extension("cpp-build.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    FileExt::lock_exclusive(&lock_file)?;
    let _guard = CppBuildLock(lock_file);

    if exe.exists() {
        return Ok(exe);
    }

    let clang = resolve_clangxx(env)?;
    let parent = exe.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let argv = vec![
        clang.to_string_lossy().into_owned(),
        source.to_string_lossy().into_owned(),
        "-std=c++20".to_string(),
        "-O2".to_string(),
        "-o".to_string(),
        exe.to_string_lossy().into_owned(),
    ];
    let code = run_argv_passthrough(argv, env.to_vec())?;
    if code != 0 {
        return Err(io::Error::other(format!(
            "C++ tool compile failed with exit code {code}: {}",
            source.display()
        )));
    }
    Ok(exe)
}

struct CppBuildLock(fs::File);

impl Drop for CppBuildLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn resolve_clangxx(env: &[(String, String)]) -> io::Result<PathBuf> {
    if let Ok(path) = which::which("clang++") {
        return Ok(path);
    }

    let install_path = clang_tool_chain_install_path(env)?;
    for name in clangxx_names() {
        if let Some(path) = find_named_executable(&install_path, &[name], 0) {
            return Ok(path);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "clang++ not found after clang-tool-chain-bins install at {}",
            install_path.display()
        ),
    ))
}

fn clang_tool_chain_install_path(env: &[(String, String)]) -> io::Result<PathBuf> {
    let output = run_argv_capture(
        vec![
            "clang-tool-chain-bins".to_string(),
            "install".to_string(),
            "clang".to_string(),
            "--dry-run".to_string(),
        ],
        env.to_vec(),
    )?;
    if output.exit_code != 0 {
        return Err(io::Error::other(format!(
            "clang-tool-chain-bins install clang --dry-run failed with exit code {}",
            output.exit_code
        )));
    }
    if let Some(path) = first_install_path(&output.stdout) {
        let installed = run_argv_passthrough(
            vec![
                "clang-tool-chain-bins".to_string(),
                "install".to_string(),
                "clang".to_string(),
            ],
            env.to_vec(),
        )?;
        if installed != 0 {
            return Err(io::Error::other(format!(
                "clang-tool-chain-bins install clang failed with exit code {installed}"
            )));
        }
        return Ok(path);
    }
    Err(io::Error::other(
        "clang-tool-chain-bins did not report an install_path for clang",
    ))
}

fn first_install_path(stdout: &[u8]) -> Option<PathBuf> {
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(path) = value.get("install_path").and_then(|v| v.as_str()) {
            return Some(PathBuf::from(path));
        }
        if let Some(items) = value.as_array() {
            for item in items {
                if let Some(path) = item.get("install_path").and_then(|v| v.as_str()) {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }
    None
}

fn clangxx_names() -> &'static [&'static str] {
    if crate::platform::host::WINDOWS {
        &["clang++.exe", "clang-cl.exe"]
    } else {
        &["clang++", "clang"]
    }
}

fn run_argv_passthrough(argv: Vec<String>, env: Vec<(String, String)>) -> io::Result<i32> {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env),
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().map_err(io::Error::other)?;
    process.wait(None).map_err(io::Error::other)
}

fn run_argv_capture(
    argv: Vec<String>,
    env: Vec<(String, String)>,
) -> io::Result<running_process::RunOutput> {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().map_err(io::Error::other)?;
    let exit_code = process.wait(None).map_err(io::Error::other)?;
    Ok(running_process::RunOutput {
        stdout: process.captured_stdout().concat(),
        stderr: process.captured_stderr().concat(),
        exit_code,
    })
}

fn managed_python_install_dir(tools_root: &Path) -> PathBuf {
    tools_root.join(MANAGED_PYTHON_DIR)
}

fn ensure_managed_python(tools_root: &Path, env: &[(String, String)]) -> io::Result<PathBuf> {
    let install_dir = managed_python_install_dir(tools_root);
    if let Some(path) = find_managed_python(&install_dir) {
        return Ok(path);
    }

    let _lock = shim_uv::acquire_install_lock(&install_dir)?;
    if let Some(path) = find_managed_python(&install_dir) {
        return Ok(path);
    }

    fs::create_dir_all(&install_dir)?;
    let argv = vec![
        "uv".to_string(),
        "python".to_string(),
        "install".to_string(),
        MANAGED_PYTHON_VERSION.to_string(),
        "--install-dir".to_string(),
        install_dir.to_string_lossy().into_owned(),
    ];
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env.to_vec()),
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().map_err(io::Error::other)?;
    let code = process.wait(None).map_err(io::Error::other)?;
    if code != 0 {
        return Err(io::Error::other(format!(
            "`uv python install {MANAGED_PYTHON_VERSION}` failed with exit code {code}"
        )));
    }

    find_managed_python(&install_dir).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "`uv python install {MANAGED_PYTHON_VERSION}` completed but no Python executable was found under {}",
                install_dir.display()
            ),
        )
    })
}

fn find_managed_python(install_dir: &std::path::Path) -> Option<PathBuf> {
    find_named_executable(install_dir, python_executable_names(), 0)
}

fn find_named_executable(dir: &std::path::Path, names: &[&str], depth: usize) -> Option<PathBuf> {
    if depth > 5 {
        return None;
    }
    let entries = fs::read_dir(dir).ok()?;
    let mut child_dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if names.contains(&name) {
                return Some(path);
            }
        } else if path.is_dir() {
            child_dirs.push(path);
        }
    }
    for child in child_dirs {
        if let Some(path) = find_named_executable(&child, names, depth + 1) {
            return Some(path);
        }
    }
    None
}

/// Every venv ships `python` (`Scripts\\python.exe` on Windows), and clud
/// standardizes on that name; see `ci/banned_python3.py`.
fn python_executable_names() -> &'static [&'static str] {
    if crate::platform::host::WINDOWS {
        &["python.exe"]
    } else {
        &["python"]
    }
}

/// Returns the `UV_CACHE_DIR` that should be in effect for this invocation.
/// Respects a parent-set value (the `clud tool` dispatch in `main.rs` sets
/// it from [`clud_uv_cache_dir`]) and falls back to the same default.
///
/// Takes the env value as a parameter so tests can exercise both branches
/// without mutating `std::env`, which would race other parallel tests.
fn resolved_uv_cache_dir_from(env_value: Option<OsString>) -> PathBuf {
    env_value
        .map(PathBuf::from)
        .unwrap_or_else(clud_uv_cache_dir)
}

/// Materialize the child's environment: start from this process's env (so
/// `PATH`, terminal colors, etc. propagate), then unconditionally pin
/// `UV_CACHE_DIR` to `cache_dir`. Listed explicitly in code so a forgotten
/// Layer-3 parent set never leaks the user's global uv cache into a
/// bundled-tool invocation. Strip `CLUD_ALLOW_DAEMON_SPAWN` so tool scripts
/// that shell out to clud cannot inherit authority to create or replace the
/// shared daemon.
fn build_child_env(cache_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            k != "UV_CACHE_DIR" && k != "CLUD_EXE" && k != crate::daemon::ENV_ALLOW_DAEMON_SPAWN
        })
        .collect();
    env.push((
        "UV_CACHE_DIR".to_string(),
        cache_dir.to_string_lossy().into_owned(),
    ));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe) = exe.to_str() {
            env.push(("CLUD_EXE".to_string(), exe.to_string()));
        }
    }
    env
}

#[path = "tool_run_telemetry.rs"]
mod tool_run_telemetry;
use tool_run_telemetry::{stderr_tail_200, ToolTelemetry};

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn abort_diagnostic_payload_never_serializes_a_provider_key() {
        let key = "sk-0123456789abcdef0123456789abcdef";
        let args = vec![key.to_string()];
        let argv = vec!["sh".to_string(), key.to_string()];
        let payload = render_passthrough_abort_payload(
            "tool.py",
            &args,
            &argv,
            Duration::from_secs(1),
            AbortReason::CommandTimeout,
            Some(key),
        );
        assert!(!payload.contains(key));
        assert!(payload.contains("****cdef"));
    }
}

#[cfg(test)]
mod watcher_deadline_tests {
    use super::*;

    #[test]
    fn watcher_deadlines_honor_requested_duration_with_a_bounded_margin() {
        let args = vec!["493".into(), "--timeout".into(), "3600".into()];
        assert_eq!(
            watcher_deadline("github/pr_merge_watch.py", &args, None).unwrap(),
            Some(Duration::from_secs(3660))
        );
        assert_eq!(
            watcher_deadline("github/pr_merge_watch.py", &[], None).unwrap(),
            Some(Duration::from_secs(3660))
        );
        assert_eq!(
            watcher_deadline("github/pr_merge_watch.py", &[], Some("120")).unwrap(),
            Some(Duration::from_secs(180))
        );
        assert_eq!(
            watcher_deadline(
                "github/pr_merge_watch.py",
                &["--timeout=240".into()],
                Some("120")
            )
            .unwrap(),
            Some(Duration::from_secs(300))
        );
        assert!(watcher_deadline(
            "github/pr_merge_watch.py",
            &["--timeout=86401".into()],
            None
        )
        .is_err());
        assert_eq!(
            watcher_deadline("github/pr_merge_watch.py", &[], Some(" 7_200 ")).unwrap(),
            Some(Duration::from_secs(7260))
        );
        assert_eq!(
            watcher_deadline("github/pr_merge_watch.py", &["--timeo=7_200".into()], None).unwrap(),
            Some(Duration::from_secs(7260))
        );
        assert!(watcher_deadline(
            "github/pr_merge_watch.py",
            &["--ti".into(), "86_401".into()],
            None
        )
        .is_err());
        assert!(watcher_deadline(
            "github/pr_merge_watch.py",
            &["--timeout=7__200".into()],
            None
        )
        .is_err());
        assert_eq!(
            watcher_deadline("github/pr_merge_watch.py", &[], Some("  ")).unwrap(),
            Some(Duration::from_secs(3660))
        );
        assert_eq!(
            watcher_deadline("github/pr_merge_watch.py", &[], Some("bad")).unwrap(),
            Some(Duration::from_secs(3660))
        );
        assert_eq!(
            watcher_deadline(
                "github/pr_merge_watch.py",
                &["--timeout".into(), "--".into(), "7200".into()],
                None
            )
            .unwrap(),
            Some(Duration::from_secs(7260))
        );
        assert!(watcher_deadline("github/pr_merge_watch.py", &[], Some("٧٢٠٠")).is_err());
        assert_eq!(watcher_deadline("other.py", &args, None).unwrap(), None);
    }
}

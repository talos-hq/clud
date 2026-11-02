use super::runner_exit::normalize_exit_code;
use super::runner_terminal;
use super::*;
use super::{lease_shared_rx, merge_extra_rx};
use crate::foreground_runtime::ForegroundRuntime;

pub(super) fn run_with_inherited_stdio(
    process: &subprocess::ManagedSubprocess,
    interrupted: &AtomicBool,
    batch_wrapped: bool,
) -> ProcessOutcome {
    loop {
        match process.poll() {
            Ok(Some(code)) => {
                if interrupted.load(Ordering::SeqCst) {
                    return ProcessOutcome::Interrupted;
                }
                return ProcessOutcome::Exited(code);
            }
            Ok(None) => {
                if interrupted.load(Ordering::SeqCst) {
                    teardown_interrupted_child(process, batch_wrapped);
                    return ProcessOutcome::Interrupted;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[clud] error waiting for process: {}", e);
                return ProcessOutcome::Error;
            }
        }
    }
}

/// Stream-JSON path: drain captured stdout line-by-line, pipe each line
/// through `stream_json::render_line`, and print the rendered progress
/// line to our own stderr (so it shows alongside the existing
/// `[clud] iteration X/Y` banner and is easily distinguished from any
/// real claude stdout payload).
///
/// `captured_output` accumulates the **raw** lines (before stream-json
/// rendering) so the loop runner can scan for the
/// `<<<CLUD_LOOP_DONE: ...>>>` token fallback (issue #95). The raw form
/// is what the agent emits in a non-JSON-wrapped chunk; we keep the
/// payload as-is so token recognition isn't confused by event framing.
pub(super) fn run_with_stream_json_renderer(
    process: &subprocess::ManagedSubprocess,
    interrupted: &AtomicBool,
    captured_output: &mut String,
    batch_wrapped: bool,
    iteration: u32,
) -> ProcessOutcome {
    use running_process::ReadStatus;
    use std::time::Duration;

    let timeout = Duration::from_millis(100);
    loop {
        if interrupted.load(Ordering::SeqCst) {
            teardown_interrupted_child(process, batch_wrapped);
            return ProcessOutcome::Interrupted;
        }
        match process.read_stdout(Some(timeout)) {
            ReadStatus::Line(bytes) => {
                emit_rendered_line(&bytes, captured_output);
            }
            ReadStatus::Timeout => {
                // No new data within the window; check if the child has
                // exited. On Windows that terminal poll closes the Job and
                // its descendant-held writers. Keep reading until EOF: the
                // reader threads may still be enqueueing buffered final
                // output and EOF is their completion barrier.
                let _ = process.poll();
            }
            ReadStatus::Eof => {
                break;
            }
        }
    }
    let summary = stream_json::IterationSummary::from_output(captured_output);
    if summary != stream_json::IterationSummary::default() {
        eprintln!("{}", summary.render_panel(iteration));
    }

    match process.wait(Some(Duration::from_secs(2))) {
        Ok(code) => ProcessOutcome::Exited(code),
        Err(_) => match process.returncode() {
            // EOF on the pipe doesn't imply the OS-level wait succeeded;
            // fall back to whatever the shared returncode tracker has.
            Some(code) => ProcessOutcome::Exited(code),
            None => ProcessOutcome::Exited(0),
        },
    }
}

fn emit_rendered_line(bytes: &[u8], captured_output: &mut String) {
    let line = String::from_utf8_lossy(bytes);
    let trimmed = line.trim_end_matches(['\r', '\n']);
    // Issue #95: keep the raw text around so we can scan for the
    // `<<<CLUD_LOOP_DONE: ...>>>` token fallback after the iteration ends.
    captured_output.push_str(trimmed);
    captured_output.push('\n');
    if let Some(rendered) = stream_json::render_line(trimmed) {
        eprintln!("{rendered}");
    }
}

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub fn run_plan_pty(
    plan: &command::LaunchPlan,
    job_tracker: Option<&crate::job_orphan_reaper::ForegroundJobTracker>,
    verbose: bool,
    interrupted: &AtomicBool,
    dnd_enabled: bool,
    mut loop_session: Option<&mut loop_artifacts::LoopSession>,
    cpu_banner_cfg: cpu_banner::CpuBannerCfg,
    toast_cfg: crate::toast::ToastLaunchCfg,
) -> i32 {
    // #1189: one toast hub for the whole launch. Every PTY iteration builds a
    // compositor over it, so the banner thread safely outlives iterations.
    let toast_hub = crate::toast::ToastHub::new();
    let status_writer = crate::toast::launch::statusline_writer(plan, toast_cfg);
    let banner_sink = if toast_cfg.enabled {
        crate::toast::ToastSink::Hub(std::sync::Arc::clone(&toast_hub))
    } else {
        crate::toast::ToastSink::Discard
    };
    // Issue #466: CPU-burn banner. Same shape as the subprocess runner —
    // background thread is stopped on drop at function exit, with a bounded
    // wait (#1172). Inert when cfg.enabled = false.
    crate::toast::launch::publish_demo_toast(&banner_sink);
    let _cpu_banner = cpu_banner::BannerWatcher::spawn(cpu_banner_cfg, banner_sink);

    // Issue #79 / #65 / #66: register the console IDropTarget for PTY
    // launches. The injector writes into `dnd_rx` which the pump drains
    // and forwards to the PTY master. Held for the full launch — the
    // refresh worker thread needs to keep displacing Claude Code's
    // own IDropTarget across iterations. Issue #1360: the receiver is
    // shared across iterations; each iteration leases it through
    // `lease_shared_rx` instead of moving it, so drops reach every child.
    let (_dnd_pty_guard, dnd_rx) = crate::platform::runner_execution::register_dnd_pty(dnd_enabled);
    let dnd_shared = dnd_rx.map(|rx| std::sync::Arc::new(std::sync::Mutex::new(rx)));

    let statusline = toast_cfg
        .claude_statusline
        .then(|| {
            status_writer
                .as_deref()
                .and_then(crate::toast::launch::injection_for)
        })
        .flatten();
    let (runtime, statusline_injected) = match ForegroundRuntime::start_with_statusline(
        plan,
        child_env_for_backend(plan.backend),
        statusline.as_ref(),
        status_writer.as_ref(),
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            // #998: see the subprocess runner -- same named failure, same
            // record.
            crate::launch_log::record_failure_reason(format_args!(
                "failed to start provider bridge: {error}"
            ));
            eprintln!("[clud] failed to start provider bridge: {error}");
            if verbose {
                verbose_log::log("[clud] provider bridge startup failed (pty)");
            }
            return 1;
        }
    };
    let mut last_exit = 0i32;
    let terminal_capabilities = (plan.graphics.mode != crate::graphics::GraphicsMode::Off)
        .then(crate::graphics::detect_current_terminal);
    let graphics_decision =
        crate::graphics::decide_sixel(&plan.graphics, terminal_capabilities.as_ref());
    if verbose {
        verbose_log::log(format_args!(
            "[clud] graphics: {} ({})",
            graphics_decision.reason,
            crate::graphics::capability_summary(terminal_capabilities.as_ref())
        ));
    }
    let toast_tier = if toast_cfg.enabled {
        let env = |name: &str| std::env::var(name).ok();
        crate::toast::tier::decide(
            terminal_capabilities.as_ref(),
            &env,
            false,
            crate::platform::host::WINDOWS,
        )
    } else {
        crate::toast::tier::TierDecision {
            tier: crate::toast::tier::ToastTier::Off,
            reason: "toasts disabled".to_string(),
        }
    };
    let toast_fallback = crate::toast::launch::fallback_after_injection(
        plan,
        status_writer.as_ref(),
        toast_cfg.claude_statusline && statusline_injected,
    );
    if verbose {
        verbose_log::log(format_args!(
            "[clud] toasts: tier={:?} fallback={:?} ({})",
            toast_tier.tier, toast_fallback, toast_tier.reason
        ));
    }

    for iteration in 0..plan.iterations {
        let (terminal_rows, cols) = get_terminal_size();
        let mut rows = terminal_rows;
        let header = if graphics_decision.enabled {
            match crate::graphics::render_header(&plan.graphics, terminal_rows, cols) {
                Ok(Some(header)) => {
                    rows = header.text_rows;
                    Some(header)
                }
                Ok(None) => {
                    if verbose {
                        verbose_log::log(format_args!(
                            "[clud] graphics: skipped header for terminal rows={terminal_rows} cols={cols}"
                        ));
                    }
                    None
                }
                Err(err) => {
                    eprintln!("[clud] warning: failed to render graphics header: {err}");
                    if verbose {
                        verbose_log::log(format_args!("[clud] graphics: render failed: {err}"));
                    }
                    None
                }
            }
        } else {
            None
        };
        if verbose {
            verbose_log::log(format_args!(
                "[clud] pty: terminal size rows={terminal_rows} cols={cols} pty_rows={rows}"
            ));
        }

        // Re-check the interrupted flag at the top of every iteration. See
        // the matching guard in `run_plan_subprocess` — same rationale.
        if interrupted.load(Ordering::SeqCst) {
            if verbose {
                verbose_log::log("[clud] interrupted via Ctrl+C (pty)");
            }
            return 130;
        }

        let iter_num = iteration + 1;
        if plan.iterations > 1 {
            eprintln!("[clud] iteration {}/{}", iter_num, plan.iterations);
        }
        if let Some(s) = loop_session.as_deref_mut() {
            s.on_iteration_start(iter_num);
        }

        if verbose {
            verbose_log::log(format_args!(
                "[clud] exec (pty): {}",
                display_verbose_command(&plan.command)
            ));
        }

        let process = match runtime.spawn_pty(plan.command.clone(), plan.cwd.clone(), rows, cols) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[clud] failed to create pty: {}", e);
                if verbose {
                    verbose_log::log(format_args!("[clud] pty: create failed: {e}"));
                }
                if let Some(s) = loop_session.as_deref_mut() {
                    s.on_iteration_end(iter_num, 1, Some(format!("pty create failed: {e}")));
                }
                return 1;
            }
        };

        // Echo off: the running-process-core PTY reader thread would
        // otherwise auto-write child output to our stdout via
        // `std::io::stdout().write_all`, bypassing our OSC filter. We
        // take chunks from `read_chunk_impl` inside the pump and run
        // them through `OscTitleStripper` before writing to stdout
        // ourselves.
        process.set_echo(false);

        if let Err(e) = process.start_impl() {
            eprintln!("[clud] failed to execute {}: {}", plan.command[0], e);
            if verbose {
                verbose_log::log(format_args!("[clud] pty: start failed: {e}"));
            }
            if let Some(s) = loop_session.as_deref_mut() {
                s.on_iteration_end(iter_num, 1, Some(format!("pty start failed: {e}")));
            }
            return 1;
        }
        if verbose {
            verbose_log::log("[clud] pty: started");
        }
        if let (Some(pid), Some(tracker)) = (process.pid().ok().flatten(), job_tracker) {
            tracker.register_backend(pid, plan.backend.executable_name());
        }
        // Issue #541: wedge watchdog. See the matching comment in
        // `run_plan_subprocess` — same per-iteration lifetime.
        let _wedge_watchdog = wedge_watchdog::WedgeWatchdog::spawn_for_pid(
            process.pid().ok().flatten(),
            plan.backend.executable_name(),
        );

        if let Some(header) = &header {
            runner_terminal::write_terminal_bytes(&header.bytes);
        }
        let header_restore = header.as_ref().map(|header| header.restore_bytes.clone());
        let graphics_resize = header.as_ref().map(|_| plan.graphics.clone());
        let mut hooks = voice::VoiceMode::from_env();

        // Issues #141 and #575: native Windows console capture is scoped to
        // one PTY iteration, exactly like the receiver consumed by the pump.
        // Recreate it for every loop iteration so no detached core can keep
        // draining the console after its channel has been retired.
        // Off Windows both halves are `None` stubs.
        let (console_input_rx, _console_input_guard) =
            crate::platform::runner_execution::spawn_console_input_for_iteration();

        // Start TerminalInputCore first so it snapshots the true original
        // Windows console mode. VT input is layered on afterward and restored
        // before the core returns to that original mode. On the byte-stream
        // fallback (or POSIX), this remains the normal VT-input guard.
        let _console_guard = enable_console_vt_input();
        let raw_guard = session::enter_raw_mode_if_tty();

        // Issue #1360: the OLE drag-drop receiver lives for the whole
        // process and is leased per iteration; the lease guard stops its
        // forwarder at iteration end so the next lease is the only reader.
        let (dnd_for_iteration, _dnd_lease) = match &dnd_shared {
            Some(shared) => {
                let (rx, lease) = lease_shared_rx(shared);
                (Some(rx), Some(lease))
            }
            None => (None, None),
        };
        let extra_rx = merge_extra_rx(dnd_for_iteration, console_input_rx);
        // #1181: only Codex gets the bare-LF -> CRLF output filter; see
        // `codex_lf.rs` for why it must stay off for every other backend.
        let normalize_bare_lf = matches!(plan.backend, crate::backend::Backend::Codex);
        let toasts = toast_cfg
            .enabled
            .then(|| crate::toast::compositor::ToastPumpOptions {
                hub: std::sync::Arc::clone(&toast_hub),
                // The Sixel header owns a scroll region the compositor does
                // not model; keep toasts out of the grid while it is shown.
                tier: if header.is_some() {
                    crate::toast::tier::ToastTier::Fallback
                } else {
                    toast_tier.tier
                },
                fallback: toast_fallback.clone(),
                usage: status_writer.clone(),
                rows,
                cols,
                image_id: crate::toast::kitty::image_id_for_process(std::process::id()),
            });
        let exit_code = session::run_raw_pty_pump_with_extras(
            &process,
            interrupted,
            &mut hooks,
            io::stdin(),
            extra_rx,
            verbose,
            session::PumpExtras {
                graphics: graphics_resize,
                normalize_bare_lf,
                toasts,
                keyboard_enhancement_tracker: raw_guard
                    .as_ref()
                    .map(session::RawTerminalGuard::child_keyboard_enhancement_tracker),
            },
        );
        // The pump's scoped reader has joined by now. Undo the modes and
        // keyboard protocol frames the child left on (Ctrl+C kills it before
        // its own exit path runs) before dropping clud's own frame, keeping
        // the terminal state that predated this session.
        if let Some(guard) = raw_guard.as_ref() {
            guard.restore_child_terminal_state();
        }
        drop(raw_guard);
        drop(_console_guard);
        // Windows: the guard is a real `ConsoleInputHandle` whose Drop
        // retires the reader's channel, and moving it out here is what scopes
        // capture to one PTY iteration (#141, #575). Everywhere else it is an
        // inert stub. Note `let _ = guard` is NOT an alternative: a wildcard
        // pattern does not move, so it would silently stop dropping the real
        // handle on Windows.
        crate::platform::runner_execution::end_console_input_iteration(_console_input_guard);
        if let Some(bytes) = header_restore.as_deref() {
            runner_terminal::write_terminal_bytes(bytes);
        }
        last_exit = normalize_exit_code(exit_code);
        if verbose {
            verbose_log::log(format_args!("[clud] pty: exited code {last_exit}"));
        }
        if let Some(s) = loop_session.as_deref_mut() {
            let err = if last_exit == 130 {
                Some("Interrupted by user".to_string())
            } else {
                None
            };
            s.on_iteration_end(iter_num, last_exit, err);
        }

        if last_exit != 0 && plan.iterations > 1 {
            eprintln!(
                "[clud] iteration {} failed with exit code {}",
                iter_num, last_exit
            );
            note_silent_bridge(&runtime, last_exit);
            return last_exit;
        }

        if let Some(code) = check_loop_markers(plan, iter_num) {
            return code;
        }
    }

    if let Some(code) = loop_unconverged_exit(plan) {
        return code;
    }

    // The PTY stream is the backend's rendered TUI, not a log, so there is
    // nothing quotable to lift from it. What clud can still say is whether the
    // harness ever asked the bridge for a turn (#998).
    note_silent_bridge(&runtime, last_exit);
    last_exit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_terminal_size_uses_probe_when_present() {
        // Input is (cols, rows) from the terminal_size crate. Output is the
        // (rows, cols) pair we pass to NativePtyProcess::new.
        assert_eq!(resolve_terminal_size(Some((120, 40))), (40, 120));
    }

    #[test]
    fn resolve_terminal_size_caps_fallback_at_200_cols() {
        // Issue #31 T3: the previous `(24, 32767)` fallback blew up ratatui
        // layout math inside the child. The cap keeps us in normal terminal
        // territory.
        let (rows, cols) = resolve_terminal_size(None);
        assert_eq!(rows, 24);
        assert_eq!(cols, 200);
        assert!(cols <= 1024, "fallback cols must stay sane: {}", cols);
    }

    #[test]
    fn launch_mode_defaults_to_subprocess() {
        let launch_mode = crate::backend::LaunchMode::Subprocess;
        assert_eq!(launch_mode.as_str(), "subprocess");
    }

    #[test]
    fn display_verbose_command_strips_program_paths() {
        let command = vec![
            r"C:\tools\node\claude.exe".to_string(),
            "--verbose".to_string(),
            "-p".to_string(),
            "hello".to_string(),
        ];
        assert_eq!(
            display_verbose_command(&command),
            "claude.exe --verbose -p hello"
        );

        let command = vec![
            "/usr/local/bin/codex".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
        ];
        assert_eq!(
            display_verbose_command(&command),
            "codex --dangerously-bypass-approvals-and-sandbox"
        );
    }

    #[test]
    fn display_verbose_command_keeps_plain_program_names() {
        let command = vec![
            "claude".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(display_verbose_command(&command), "claude --model opus");
    }

    // Host-specific `child_env` tests live in
    // `platform/{windows,unix}/runner_execution.rs`.

    fn env_lookup(env: &[(String, String)], key: &str) -> Option<String> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn child_env_for_backend_does_not_set_shell_vars_when_toggle_off() {
        let home = tempfile::tempdir().unwrap();
        for backend in [Backend::Claude, Backend::Codex] {
            let env = child_env_for_backend_at(backend, Some(home.path()));
            assert_eq!(env_lookup(&env, "CLUD_DISABLE_POWERSHELL"), None);
            assert_eq!(env_lookup(&env, "CLAUDE_CODE_USE_POWERSHELL_TOOL"), None);
            assert_eq!(env_lookup(&env, "CLAUDE_CODE_GIT_BASH_PATH"), None);
        }
    }

    #[test]
    fn child_env_for_backend_sets_clud_disable_for_both_backends_when_top_level_true() {
        let home = tempfile::tempdir().unwrap();
        clud_settings::save_shell_disable_powershell_at(home.path(), true).unwrap();

        let codex_env = child_env_for_backend_at(Backend::Codex, Some(home.path()));
        assert_eq!(
            env_lookup(&codex_env, "CLUD_DISABLE_POWERSHELL").as_deref(),
            Some("1"),
            "Codex still gets the advisory env var so skills can branch"
        );
        // Codex has no env-var equivalent for the PowerShell tool itself
        // — the load-bearing enforcement ships as a PreToolUse hook in a
        // follow-up PR. Make sure we don't accidentally set Claude-only
        // vars on the Codex path here.
        assert_eq!(
            env_lookup(&codex_env, "CLAUDE_CODE_USE_POWERSHELL_TOOL"),
            None
        );
        assert_eq!(env_lookup(&codex_env, "CLAUDE_CODE_GIT_BASH_PATH"), None);
    }

    /// Pre-stage the resolver's warm-path artifacts under `home` so the
    /// runner tests don't trigger a real 9 MB network fetch. Mirrors what
    /// `git_bash_resolver::resolve_or_fetch_with` writes on a successful
    /// fetch — the directory tree plus the sibling `.complete` sentinel.
    fn warm_cache_vendored_bash(home: &Path) -> std::path::PathBuf {
        let manifest = crate::shell::git_bash_resolver::embedded_manifest().unwrap();
        let sha = &manifest.git_bash_bin.sha256;
        let extraction = crate::shell::git_bash_resolver::extraction_dir(home, sha);
        let bash_path = extraction.join(&manifest.git_bash_bin.relative_bash_path);
        std::fs::create_dir_all(bash_path.parent().unwrap()).unwrap();
        std::fs::write(&bash_path, b"#!/fake/bash test stub").unwrap();
        std::fs::write(
            crate::shell::git_bash_resolver::sentinel_path(home, sha),
            b"warm",
        )
        .unwrap();
        bash_path
    }

    #[test]
    fn child_env_for_backend_sets_claude_kill_switch_when_claude_enabled() {
        let home = tempfile::tempdir().unwrap();
        clud_settings::save_shell_disable_powershell_at(home.path(), true).unwrap();
        let expected_bash = warm_cache_vendored_bash(home.path());

        let env = child_env_for_backend_at(Backend::Claude, Some(home.path()));
        assert_eq!(
            env_lookup(&env, "CLUD_DISABLE_POWERSHELL").as_deref(),
            Some("1")
        );
        assert_eq!(
            env_lookup(&env, "CLAUDE_CODE_USE_POWERSHELL_TOOL").as_deref(),
            Some("0"),
            "Claude env-var kill-switch must be set when the toggle is on"
        );
        assert_eq!(
            env_lookup(&env, "CLAUDE_CODE_GIT_BASH_PATH").as_deref(),
            Some(expected_bash.to_string_lossy().as_ref()),
            "CLAUDE_CODE_GIT_BASH_PATH must resolve to the warm-cached bash"
        );
    }

    #[test]
    fn child_env_for_backend_backend_override_blocks_top_level() {
        let home = tempfile::tempdir().unwrap();
        clud_settings::save_shell_disable_powershell_at(home.path(), true).unwrap();
        clud_settings::save_shell_disable_powershell_for_backend_at(
            home.path(),
            Backend::Claude,
            Some(false),
        )
        .unwrap();

        let claude_env = child_env_for_backend_at(Backend::Claude, Some(home.path()));
        assert_eq!(
            env_lookup(&claude_env, "CLUD_DISABLE_POWERSHELL"),
            None,
            "Claude override=false must short-circuit env injection"
        );
        assert_eq!(
            env_lookup(&claude_env, "CLAUDE_CODE_USE_POWERSHELL_TOOL"),
            None
        );

        let codex_env = child_env_for_backend_at(Backend::Codex, Some(home.path()));
        assert_eq!(
            env_lookup(&codex_env, "CLUD_DISABLE_POWERSHELL").as_deref(),
            Some("1"),
            "Codex with null override should still inherit top-level true"
        );
    }

    /// Turns a literal pair list into the `Vec<(String, String)>` shape the
    /// policy builders take, so the tests below read as plain data.
    fn env_pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    /// Borrowing lookup helper. Named distinctly from the `env_lookup` fn and
    /// the inline `lookup` closure the Windows tests above use, so neither is
    /// shadowed and the returned borrow needs no clone.
    fn value_of<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn unsafe_child_env_preserves_marker_and_removes_clud_shell_guards() {
        let home = tempfile::tempdir().unwrap();
        let home_str = home.path().to_string_lossy().into_owned();
        let base = env_pairs(&[
            ("HOME", home_str.as_str()),
            ("PATH", "/base/bin"),
            (crate::runner::UNSAFE_MODE_ENV, "1"),
            (crate::shell::cmd_gate::GATE_KEY, "1"),
        ]);
        let env = apply_child_env_policy_with(base, false);
        assert_eq!(value_of(&env, crate::runner::UNSAFE_MODE_ENV), Some("1"));
        assert_eq!(value_of(&env, crate::shell::cmd_gate::GATE_KEY), None);
        if let Some(script) = value_of(&env, crate::shell::nounset::BASH_ENV_KEY) {
            let body = std::fs::read_to_string(script).unwrap();
            assert!(!body.contains("then set -u;"));
        }
    }

    /// #1209 RED, platform-neutral: the Windows UTF-8 stdio pair existed only
    /// in the runner's copy of the policy, so Windows daemon sessions spawned
    /// without it. `windows_stdio` is injected rather than read from `cfg!`
    /// precisely so a Linux lane can assert the layer at all.
    #[test]
    fn forcing_windows_stdio_sets_the_python_utf8_pair_exactly_once() {
        let home = tempfile::tempdir().unwrap();
        let home_str = home.path().to_string_lossy().into_owned();
        let base = env_pairs(&[
            ("HOME", home_str.as_str()),
            ("USERPROFILE", home_str.as_str()),
            ("PATH", "/base/bin"),
            ("PYTHONUTF8", "0"),
            ("PYTHONIOENCODING", "latin-1"),
        ]);

        let env = apply_child_env_policy_with(base, true);

        assert_eq!(value_of(&env, "PYTHONIOENCODING"), Some("utf-8"));
        assert_eq!(value_of(&env, "PYTHONUTF8"), Some("1"));
        assert_eq!(
            env.iter().filter(|(k, _)| k == "PYTHONIOENCODING").count(),
            1,
            "stale PYTHONIOENCODING must be replaced, not shadowed by a second entry"
        );
        assert_eq!(
            env.iter().filter(|(k, _)| k == "PYTHONUTF8").count(),
            1,
            "stale PYTHONUTF8 must be replaced, not shadowed by a second entry"
        );
    }

    /// Off Windows the pair is not ours to touch: an inherited value must
    /// survive untouched. Merging the two builders must not start deleting it.
    #[test]
    fn without_windows_stdio_an_inherited_python_setting_survives() {
        let home = tempfile::tempdir().unwrap();
        let home_str = home.path().to_string_lossy().into_owned();
        let base = env_pairs(&[
            ("HOME", home_str.as_str()),
            ("USERPROFILE", home_str.as_str()),
            ("PATH", "/base/bin"),
            ("PYTHONUTF8", "0"),
        ]);

        let env = apply_child_env_policy_with(base, false);

        assert_eq!(value_of(&env, "PYTHONUTF8"), Some("0"));
        assert_eq!(
            env.iter().filter(|(k, _)| k == "PYTHONUTF8").count(),
            1,
            "PYTHONUTF8 must appear exactly once"
        );
        assert_eq!(value_of(&env, "PYTHONIOENCODING"), None);
    }

    /// The IN_CLUD/originator tag is a replacement, not an addition: a
    /// relaunch inside a clud session must not hand the child two values.
    #[test]
    fn the_clud_tag_replaces_any_inherited_one() {
        let home = tempfile::tempdir().unwrap();
        let home_str = home.path().to_string_lossy().into_owned();
        let base = env_pairs(&[
            ("HOME", home_str.as_str()),
            ("USERPROFILE", home_str.as_str()),
            ("PATH", "/base/bin"),
            ("IN_CLUD", "stale"),
            (running_process::ORIGINATOR_ENV_VAR, "OTHER:1"),
        ]);

        let env = apply_child_env_policy(base);

        let in_clud: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k == "IN_CLUD")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(in_clud, vec!["1"], "IN_CLUD must be exactly one 1");

        let originator: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k == running_process::ORIGINATOR_ENV_VAR)
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(originator.len(), 1, "exactly one originator entry");
        assert!(
            originator[0].starts_with("CLUD:"),
            "originator value must start with CLUD:, got {}",
            originator[0]
        );
    }

    /// `child_env_policy_keys()` is what the daemon/runner drift guard in
    /// `daemon/io_helpers.rs` compares over, so a policy key missing from the
    /// list would make that guard silently blind. Assert the list covers every
    /// key the builder actually adds or changes.
    #[test]
    fn the_policy_key_list_covers_every_key_the_builder_touches() {
        let home = tempfile::tempdir().unwrap();
        let home_str = home.path().to_string_lossy().into_owned();
        let base = env_pairs(&[
            ("HOME", home_str.as_str()),
            ("USERPROFILE", home_str.as_str()),
            ("PATH", "/base/bin"),
        ]);

        let before = base.clone();
        let env = apply_child_env_policy_with(before, true);
        let policy_keys = child_env_policy_keys();

        for (key, value) in &env {
            let unchanged = value_of(&base, key) == Some(value.as_str());
            if unchanged {
                continue;
            }
            assert!(
                policy_keys.contains(&key.as_str()) || key == "PATH",
                "policy touched {key:?} but child_env_policy_keys() doesn't list it \
                 (add it there so the daemon/runner drift guard can see it)"
            );
        }

        for &key in WINDOWS_STDIO_KEYS {
            assert!(
                policy_keys.contains(&key),
                "child_env_policy_keys() must include the Windows stdio key {key:?}"
            );
        }
    }
}

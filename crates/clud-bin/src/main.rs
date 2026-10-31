use clud::{
    args, auth, backend, backend_bootstrap, claude_files, clud_settings, codex_auth, command,
    config, console_setup, console_title, cpu_banner, crash_report, ctrl_c_track, daemon, failover,
    gc, graphics, grind, harness_picker, hook_health, job_orphan_reaper, kitty_term,
    large_file_guard, launch_log, launch_setup, log_event, loop_artifacts, loop_spec, multicall,
    openrouter_catalog, optimize, orphan_reaper, provider_auth, runner, runtime_cache,
    self_install, settings_tui, skills, soldr_activate, stage_trace, startup, symbols,
    test_runtime, toast, tool_cli, tool_install, tools, trampoline, trash, ui, uv_run_hook_guard,
    verbose_log, wasm, webterm, workspace_trust, worktrees,
};

use std::io::{self, IsTerminal, Read, Write};

fn main() {
    // #1551: `clud` is the only executable clud ships. A helper name in
    // argv[0] (`clud-cmd-scan`, `rm`, `gh`, ...) selects its function here,
    // before the console, clap or any other startup work, so a hook or shim
    // call costs what the old dedicated binaries did.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(code) = multicall::maybe_run(&argv) {
        std::process::exit(code);
    }
    // #1743: a session launched by an older clud keeps its alias dir until
    // something relinks it. Its statusline, hooks and `clud tool` calls run
    // the installed clud by path, so the first of them after an upgrade
    // brings that session's `gh`/`git`/`rm` up to date. A few stats when
    // nothing is stale; see docs/architecture/shim-dispatch.md.
    clud::shim_install::refresh_running_session_aliases();
    // #1374: before anything can write an escape sequence (clap's help, a
    // colored notice, a selector, a PTY session), so every launch path gets
    // VT output processing on a Windows console, not only those that happen
    // to show a selector first.
    console_setup::enable_console_vt_output();
    run(parse_args());
}

fn parse_args() -> args::Args {
    if !clud::platform::host::WINDOWS {
        return args::Args::parse_with_passthrough();
    }
    // Clap builds the complete nested command tree while parsing argv. Keep
    // only that work off Windows' small default main-thread stack so adding a
    // maintenance command cannot make even `clud --help` overflow, while the
    // rest of clud remains on the process main thread for console/OLE APIs.
    std::thread::Builder::new()
        .name("clud-arg-parser".to_string())
        .stack_size(4 * 1024 * 1024)
        .spawn(args::Args::parse_with_passthrough)
        .expect("spawn clud argument parser")
        .join()
        .expect("clud argument parser panicked")
}

/// The whole `clud` launch, as an ordered list of phases.
///
/// Every phase that can end the process does so by returning an exit code
/// (or calling `std::process::exit` itself), so the order below is the
/// contract: a self-contained utility mode must exit before the phase that
/// would start a daemon, resolve a backend, or write setup for it.
fn run(mut args: args::Args) {
    args.normalize_explicit_run();
    // Before provider inference and selection resolution, so `--allow-model`
    // without `--model` still launches with a main model inside its own
    // boundary (#1257).
    args.normalize_model_allowlist();
    if let Some(code) = dispatch_before_startup(&args) {
        std::process::exit(code);
    }
    // A nested launch makes a fresh policy choice. Never inherit an unsafe
    // marker from the parent session when this invocation omitted the flag.
    unsafe {
        std::env::remove_var(runner::UNSAFE_MODE_ENV);
        if args.unsafe_mode {
            std::env::set_var(runner::UNSAFE_MODE_ENV, "1");
        }
        // #1984: `--worktree` asks for a worktree before any prompt exists;
        // the flag is the user's consent, carried to every hook.
        if clud::worktree_consent::launch_args_ask_for_worktree(&args.passthrough) {
            std::env::set_var(clud::worktree_consent::ALLOW_ENV, "1");
        }
    }
    init_foreground_process(&args);
    if let Some(code) = dispatch_maintenance(&args) {
        std::process::exit(code);
    }
    resolve_do_target(&mut args);
    enter_video_command(&args);

    let launch = resolve_launch(&mut args);
    let auto_fix_hooks = resolve_auto_fix_hooks(&args, launch);
    prepare_launch_inputs(&mut args, launch.target);

    let interrupted = startup::install_ctrl_c_flag(args.verbose);
    if let Some(exit_code) = daemon::handle_special_command(&args, interrupted.as_ref()) {
        flush_ctrl_c_exit_event(ctrl_c_track::InvocationKind::Attach, exit_code);
        std::process::exit(exit_code);
    }
    prepare_launch_host(&args, launch.target, auto_fix_hooks);
    admit_provider_bridge(&args, launch.target);
    set_daemon_spawn_authority(&args);
    let foreground_client_lease = start_early_daemon(&args);
    let foreground_checkout_presence =
        start_checkout_presence(&args, foreground_client_lease.is_some(), &interrupted);

    // #569: the persistent daemon deliberately starts before this foreground
    // process joins the Windows tracking job, so it can outlive the CLI.
    let job_orphan_reaper = job_orphan_reaper::ForegroundJobTracker::install();
    run_uv_run_hook_guard(&args);

    let plan = build_backend_plan(&mut args, launch);
    if args.dry_run {
        print_dry_run_and_exit(&args, &plan, launch.target);
    }
    prepare_video_launch(&args);
    emit_launch_notices(&args, launch.target);
    let exit_code = launch_and_clean_up(&args, &plan, interrupted.as_ref(), job_orphan_reaper);
    drop(foreground_checkout_presence);
    if let Some(lease) = foreground_client_lease {
        lease.release();
    }
    std::process::exit(exit_code);
}

/// `clud video [DIR] [--update | --path]` (#1851, #1866): `--path` prints the
/// checkout and exits; `--update` reinstalls the pinned
/// video-use checkout and exits; otherwise the session runs in `DIR`.
fn enter_video_command(args: &args::Args) {
    let Some(args::Command::Video { dir, update, path }) = &args.command else {
        return;
    };
    if *path {
        std::process::exit(clud::video::run_path());
    }
    if *update {
        std::process::exit(clud::video::run_update());
    }
    if let Some(dir) = dir {
        if let Err(error) = std::env::set_current_dir(dir) {
            eprintln!("[clud] error: cannot enter {}: {error}", dir.display());
            std::process::exit(2);
        }
    }
}

/// Media tools, the managed install and the ElevenLabs key for a real
/// `clud video` launch. Runs after the dry-run exit, so `--dry-run` never
/// installs anything or reads the vault. The key is placed in this process's
/// environment only now, immediately before the harness child inherits it.
fn prepare_video_launch(args: &args::Args) {
    if !matches!(args.command, Some(args::Command::Video { .. })) {
        return;
    }
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    match clud::video::prepare_launch(interactive) {
        Ok(key) => unsafe {
            std::env::set_var(clud::video::ELEVENLABS_ENV, key);
        },
        Err(error) => {
            eprintln!("[clud] error: {error}");
            std::process::exit(2);
        }
    }
}

fn start_checkout_presence(
    args: &args::Args,
    daemon_ready: bool,
    interrupted: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<daemon::ForegroundCheckoutPresence> {
    if args.dry_run {
        return None;
    }
    let (tool, run, claim) = match args.command.as_ref() {
        Some(args::Command::Grind { url }) => ("grind", url.clone(), true),
        Some(args::Command::Do { target }) => ("do", target.clone(), true),
        Some(args::Command::Fix { url }) => ("fix", url.clone(), true),
        _ => ("clud", None, false),
    };
    if !daemon_ready {
        if claim {
            eprintln!("[clud] cannot claim checkout: daemon is unavailable; use a new worktree");
            std::process::exit(1);
        }
        return None;
    }
    let result = args
        .daemon_state_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(daemon::default_state_dir)
        .and_then(|state_dir| {
            let cwd = std::env::current_dir()?;
            let presence = daemon::start_foreground_presence(
                &state_dir,
                &cwd,
                tool,
                run,
                claim,
                std::sync::Arc::clone(interrupted),
            )?;
            if args.daemon_state_dir.is_some() {
                std::env::set_var(daemon::ENV_STATE_DIR, &state_dir);
            }
            Ok(presence)
        });
    match result {
        Ok(presence) => presence,
        Err(error) if claim => {
            eprintln!("[clud] cannot claim checkout: {error}");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("[clud] note: checkout presence unavailable: {error}");
            None
        }
    }
}

/// Modes that must run before the crash reporter, the runtime-cache hop and
/// every other piece of launch machinery. `Some` is the process exit code.
fn dispatch_before_startup(args: &args::Args) -> Option<i32> {
    if let Some(code) = clud::openrouter_routing::handle_setting(args) {
        return Some(code);
    }
    if let Some(code) = clud_settings::authorized::handle(args) {
        return Some(code);
    }
    if let Some(code) = self_install::entry::run_explicit(args) {
        return Some(code);
    }
    if let Some(code) = self_install::entry::run_auto_offer(args) {
        return Some(code);
    }
    if let Some(exit_code) = kitty_term::handle(args) {
        return Some(exit_code);
    }
    if let Some(exit_code) = webterm::handle(args) {
        return Some(exit_code);
    }
    // Daemon-spawn authority is process-local and never trusted from the
    // parent environment. The normal launch path grants it later, after all
    // utility and internal dispatch has returned.
    unsafe {
        std::env::remove_var(daemon::ENV_ALLOW_DAEMON_SPAWN);
    }
    if let Some(code) = dispatch_fast_path_command(args.command.as_ref()) {
        return Some(code);
    }
    dispatch_credential_command(args)
}

/// Hook, statusline and tool invocations: they run many times per session,
/// so they must not touch the daemon, the runtime cache, or any launch
/// machinery.
fn session_stats(session: Option<&str>) -> i32 {
    let id = session
        .map(str::to_string)
        .or_else(|| std::env::var(clud::grind_facts::SESSION_ENV).ok())
        .filter(|id| !id.is_empty());
    let Some(id) = id else {
        eprintln!(
            "clud session stats: no session id; run it inside a Claude Code session or pass --session <id>"
        );
        return 2;
    };
    match clud::block_bad_cmd::session_stats(&id) {
        Some(text) => {
            print!("{text}");
            0
        }
        None => {
            eprintln!("clud session stats: nothing recorded for session {id}");
            1
        }
    }
}

fn dispatch_fast_path_command(cmd: Option<&args::Command>) -> Option<i32> {
    let code = match cmd? {
        // #1189: Claude Code runs `clud statusline` as its statusLine command
        // every couple of seconds. Like `clud tool`, it must not touch the daemon,
        // the runtime cache, or any launch machinery.
        args::Command::Statusline {
            session_pid,
            state_dir,
            chain_b64,
        } => toast::statusline::run(&toast::statusline::RunArgs {
            session_pid: *session_pid,
            state_dir: state_dir.clone(),
            chain_b64: chain_b64.clone(),
        }),
        // #922: Claude Code runs `clud session-hook` at session start, compact and
        // end. Like `statusline`, it must not touch the daemon or launch machinery.
        args::Command::SessionHook {
            event,
            route,
            state_dir,
            recovery_file,
        } => clud::session_history::hook::run(&clud::session_history::hook::HookArgs {
            event: event.clone(),
            route: route.clone(),
            state_dir: state_dir.clone(),
            recovery_file: recovery_file.clone(),
        }),
        // Fast tool path. Detect `clud tool ...` before
        // normal clud startup so hook/tool invocations do not connect to the
        // daemon, touch runtime-cache, start title keepers, or register as
        // foreground clud sessions.
        // #2122: reads the verify-ratio ledger only; no daemon, no launch.
        args::Command::Session {
            action: args::SessionAction::Stats { session },
        } => session_stats(session.as_deref()),
        args::Command::InternalStartCmd => clud::platform::start_terminal::run(),
        // Hidden test seam for the OpenAI Chat Completions ingress translator:
        // stdin in, Messages request body out. No session, backend, or daemon.
        args::Command::InternalOpenAiChatTranslate {
            response,
            stream,
            for_model,
            http,
        } => {
            if *response {
                clud::openai_ingress::run_translate_response_stdin(
                    *stream,
                    for_model.as_deref(),
                    *http,
                )
            } else {
                clud::openai_ingress::run_translate_stdin()
            }
        }
        args::Command::InternalGeminiClientTranslate {
            model,
            response,
            stream,
            http,
        } => {
            if *response {
                clud::gemini_client_ingress::run_translate_response_stdin(
                    *stream,
                    model.as_deref(),
                    *http,
                )
            } else {
                clud::gemini_client_ingress::run_translate_stdin(model.as_deref(), *stream)
            }
        }
        args::Command::DoPrompt { target } => command::do_prompt::run(target),
        args::Command::GrindScripts => command::grind_scripts::run(),
        args::Command::GrindFacts { args } => clud::grind_facts::run_cli(args),
        args::Command::Claim { action } => daemon::run_claim_command(action),
        args::Command::SafeRm { args } => clud::rm_tool::run(args),
        args::Command::InstallAssets { home } => install_assets(home.as_deref()),
        args::Command::Tool { subcommand } => {
            unsafe {
                std::env::set_var("UV_CACHE_DIR", tools::clud_uv_cache_dir());
                std::env::remove_var(daemon::ENV_ALLOW_DAEMON_SPAWN);
            }
            tool_cli::run(subcommand)
        }
        _ => return None,
    };
    Some(code)
}

/// Codex updates, credential management and catalog queries: self-contained,
/// and none of them may resolve a backend or start a daemon.
fn dispatch_credential_command(args: &args::Args) -> Option<i32> {
    let code = match args.command.as_ref()? {
        args::Command::Serve {
            claude,
            codex,
            openrouter,
            port,
        } => clud::serve_gateway::run(*port, *claude, *codex, *openrouter),
        args::Command::CodexUpdate => {
            if !args.passthrough.is_empty() {
                eprintln!("codex-update accepts no passthrough arguments");
                return Some(2);
            }
            backend_bootstrap::run_trusted_codex_update()
        }
        args::Command::Ci {
            job,
            filter,
            workflow,
            json,
        } => clud::ci_local::run(&clud::ci_local::CiOptions {
            job: job.clone(),
            filter: filter.clone(),
            workflow: workflow.clone(),
            json: *json,
        }),
        args::Command::Secret(options) => clud::secret_prompt::run(options),
        args::Command::Doctor { json } => {
            if !args.passthrough.is_empty() {
                eprintln!("doctor accepts no passthrough arguments");
                return Some(2);
            }
            clud::doctor::run(*json)
        }
        args::Command::DshUpdate => {
            if !args.passthrough.is_empty() {
                eprintln!("dsh-update accepts no passthrough arguments");
                return Some(2);
            }
            clud::dsh_harness::run_update()
        }
        // Credential management is self-contained and must never resolve a backend,
        // start a daemon, or forward secrets to a harness.
        args::Command::Auth { subcommand } => {
            let interrupted = startup::install_ctrl_c_flag(args.verbose);
            auth::run(subcommand.as_ref(), interrupted.as_ref())
        }
        // #1256: catalog queries are self-contained and must not launch a backend
        // or grant daemon-spawn authority. A stale cache may perform one bounded,
        // fixed-origin refresh before rendering the baked/last-known-good result.
        args::Command::Models { subcommand } => match subcommand {
            args::ModelsSubcommand::Cheapest { json } => openrouter_catalog::run_cheapest(*json),
        },
        // Compatibility aliases remain through this major version. Keep their
        // provider implementation authoritative while giving callers the exact
        // action-first replacement.
        args::Command::CodexAuth { subcommand } => {
            eprintln!(
                "deprecated: use `{}`",
                auth::codex_alias_replacement(subcommand)
            );
            let interrupted = startup::install_ctrl_c_flag(args.verbose);
            codex_auth::run(subcommand, interrupted.as_ref())
        }
        args::Command::DeepseekAuth { subcommand } => {
            eprintln!(
                "deprecated: use `{}`",
                auth::deepseek_alias_replacement(subcommand)
            );
            provider_auth::run(subcommand)
        }
        _ => return None,
    };
    Some(code)
}

/// Process-wide setup every remaining path shares: crash reporting, the
/// `uv` cache pin, the runtime-cache hop, the console title and verbose
/// logging.
fn init_foreground_process(args: &args::Args) {
    // Install the crash reporter first so a panic during the rest of startup
    // (arg parsing, runtime-cache hop, drop-target registration, ...) still
    // writes a JSON report under ~/.clud/state/crashes/. Idempotent; the
    // daemon and worker process entries re-call install_native() with their
    // own role to retag any future crash without reinstalling the hook.
    //
    // `install_native` covers SIGSEGV / SIGBUS / SIGILL / SIGFPE / SIGABRT on
    // Unix and structured exceptions on Windows in addition to Rust panics.
    // It explicitly does NOT attach a SIGINT / CTRL_C_EVENT handler — the
    // existing `ctrlc`-based path (`startup::install_ctrl_c_flag` below /
    // #372 forensic capture) remains the authoritative Ctrl-C handler.
    crash_report::install_native("foreground");

    // Issue #1964: no process-wide `UV_CACHE_DIR`. #408's pin belongs to
    // bundled tools only, which `clud tool` sets below; pinning every
    // session child sent user projects' `uv` into `~/.clud/cache/uv` too.

    verbose_log::init_launch_clock();

    // #333: the daemon and worker roles are exempt — on Windows the hop cannot
    // preserve the PID, and theirs is recorded by other processes.
    let subcommand_name = args.command.as_ref().and_then(args::Command::internal_name);
    if let Err(err) = runtime_cache::hop_to_runtime_cache_if_enabled(subcommand_name) {
        eprintln!("[clud] warning: runtime cache hop failed: {err}");
    }

    // Windows: rename ourselves so pip can always overwrite clud.exe.
    trampoline::unlock_exe();

    // Stamp the console title with `clud <cwd-name>` so the active
    // window is identifiable at a glance. Windows-only effective; a
    // no-op on POSIX (out of scope per the originating request).
    //
    // The one-shot stamp gets overwritten as soon as the backend (and
    // its tool subprocesses) emit OSC 0/2 sequences, so we also kick
    // off a background keeper that re-applies the title whenever it
    // drifts. PTY mode additionally strips the OSC sequences upstream
    // (see session.rs) so the keeper rarely fires and the title doesn't
    // visibly flicker. In subprocess mode (the default Claude path on
    // Windows) the child inherits stdio directly, so the keeper is the
    // only way to defend the title.
    console_title::set_for_current_cwd();
    console_title::keep_setting_in_background();

    if args.verbose {
        enable_verbose_file_logging();
    }
}

fn enable_verbose_file_logging() {
    match verbose_log::enable_file_logging() {
        Ok(path) => {
            verbose_log::log(format_args!(
                "[clud] verbose log: {}",
                verbose_log::display_path(&path)
            ));
        }
        Err(err) => {
            verbose_log::log(format_args!("[clud] verbose log unavailable: {err}"));
        }
    }
    verbose_log::log(format_args!("[clud] pid {}", std::process::id()));
}

/// Self-contained maintenance modes, dispatched before backend resolution
/// so none of them ever starts an agent. `Some` is the process exit code.
fn dispatch_maintenance(args: &args::Args) -> Option<i32> {
    // Issue #233: standalone graphics smoke test. It must emit only the
    // Sixel payload plus status line, without backend or setup side effects.
    if args.demo_gfx_sixel {
        return Some(run_demo_gfx_sixel());
    }
    if let Some(code) = dispatch_maintenance_command(args) {
        return Some(code);
    }
    if let Some(code) = dispatch_local_tool_command(args) {
        return Some(code);
    }
    // Issue #83: `--clean-worktrees` is a self-contained maintenance path.
    // It never launches a backend, so handle it before backend resolution.
    if args.clean_worktrees {
        return Some(run_clean_worktrees(args));
    }
    None
}

fn run_demo_gfx_sixel() -> i32 {
    let terminal_cols = terminal_size::terminal_size().map(|(width, _height)| width.0);
    match graphics::render_demo_sixel_bytes(terminal_cols) {
        Ok(bytes) => {
            let mut out = io::stdout().lock();
            if let Err(err) = out.write_all(&bytes).and_then(|_| out.flush()) {
                eprintln!("error: failed to write Sixel demo: {err}");
                return 1;
            }
            0
        }
        Err(err) => {
            eprintln!("error: failed to render Sixel demo: {err}");
            1
        }
    }
}

fn dispatch_maintenance_command(args: &args::Args) -> Option<i32> {
    let code = match args.command.as_ref()? {
        // Issue #110: `clud gc <subcommand>` is a self-contained
        // maintenance path that never launches a backend. Dispatch before
        // backend resolution and before any session registry / dnd work
        // so a registry-less host can still run `clud gc reconcile`.
        args::Command::Gc { subcommand } => {
            // The one subcommand that is granted daemon-creation authority. `gc`
            // exists to operate on the registry, so it is not a utility mode that
            // merely happens to touch it: with no daemon there is nothing to
            // prune, and reporting success would tell the caller garbage was
            // collected when none was. `--no-daemon` still withholds the grant,
            // which is the documented `clud gc *` precondition.
            if !args.no_daemon && !args.dry_run {
                unsafe {
                    std::env::set_var(daemon::ENV_ALLOW_DAEMON_SPAWN, "1");
                }
            }
            gc::run(args, subcommand.clone())
        }
        // Issue #457: settings inspection/editing is self-contained. Dispatch
        // before backend resolution so `clud config show` never starts an agent.
        args::Command::Config { subcommand } => config::run(args, subcommand.clone()),
        // Issue #183: `clud ui` opens the local dashboard. Self-contained;
        // never launches a backend.
        args::Command::Ui { json, no_open } => ui::run(*json, *no_open),
        // Issue #182: `clud trash` is self-contained maintenance. Dispatch
        // before backend resolution so quarantining a locked artifact never
        // launches an agent process.
        args::Command::Trash {
            cross_volume,
            paths,
        } => trash::run(args, paths, *cross_volume),
        // Issue #469: `clud log --cmd "..."` posts one telemetry event to
        // the always-on daemon's HTTP server. Discovers the daemon via
        // `$CLUD_DAEMON_HTTP_SERVER`. Self-contained; never launches an
        // agent backend.
        args::Command::Log {
            cmd,
            fail_on_no_server,
        } => log_event::run(cmd, *fail_on_no_server),
        // #374 (PR 3): `clud symbols` inspects / verifies crash-report
        // symbolication against the running binary. Self-contained; never
        // launches a backend.
        args::Command::Symbols { subcommand } => symbols::run(args, subcommand.clone()),
        _ => return None,
    };
    Some(code)
}

fn dispatch_local_tool_command(args: &args::Args) -> Option<i32> {
    let code = match args.command.as_ref()? {
        // `clud extern` manages the per-machine trust allowlist for foreign
        // checkouts' hooks (#967 Phase 4). Self-contained; never launches a
        // backend and must not touch the daemon.
        args::Command::Extern { subcommand } => clud::extern_cli::run(subcommand.as_ref()),
        // #407: `clud test` records/reports per-bucket test runtimes. Self-
        // contained; never launches a backend. `run` returns the wrapped command's
        // exit code unchanged so prefixing it onto an existing invocation is safe.
        args::Command::Test { subcommand } => match subcommand {
            args::TestSubcommand::Run {
                bucket,
                target,
                command,
            } => test_runtime::cli::run(bucket, target.clone(), command),
            args::TestSubcommand::Capture {
                output_dir,
                timeout_secs,
                command,
            } => test_runtime::capture::run_cli(output_dir, *timeout_secs, command),
            args::TestSubcommand::Stats { bucket, json } => {
                test_runtime::cli::stats(bucket.as_deref(), *json)
            }
        },
        // `clud settings` is an interactive global-settings TUI, self-contained;
        // never launches a backend.
        args::Command::Settings { list, .. } => settings_tui::run(*list),
        // `clud optimize` is machine/repo setup and never launches a backend.
        args::Command::Optimize {
            target,
            global,
            repo,
            install_soldr,
            use_soldr_shims,
            soldr_version,
        } => optimize::run(
            args,
            *target,
            *global,
            *repo,
            *install_soldr,
            *use_soldr_shims,
            soldr_version,
        ),
        _ => return None,
    };
    Some(code)
}

fn run_clean_worktrees(args: &args::Args) -> i32 {
    let stale_after = match worktrees::parse_duration(&args.stale_after) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: invalid --stale-after value: {e}");
            return 2;
        }
    };
    let opts = worktrees::CleanOptions {
        stale_after,
        dry_run: args.dry_run,
        yes: args.yes,
        force: args.force,
    };
    worktrees::run(&opts)
}

/// `do` accepts an optional URL/free-form target. Resolve it before any
/// harness selection, credential preflight, setup write, or backend spawn.
/// Never consume piped stdin: in non-interactive/dry-run/background modes a
/// missing target is a deterministic usage error instead of a hidden block.
fn resolve_do_target(args: &mut args::Args) {
    if !matches!(args.command, Some(args::Command::Do { .. })) {
        return;
    }
    let stdin_is_terminal = io::stdin().is_terminal();
    let stderr_is_terminal = io::stderr().is_terminal();
    let resolved = {
        let stdin = io::stdin();
        let stderr = io::stderr();
        let mut input = stdin.lock();
        let mut output = stderr.lock();
        command::resolve_do_command_target(
            args,
            stdin_is_terminal,
            stderr_is_terminal,
            &mut input,
            &mut output,
        )
    };
    if let Err(error) = resolved {
        eprintln!("[clud] error: {error}");
        std::process::exit(2);
    }
    // Classify before setup and render the live contract directly: /goal
    // treats nested slash commands as text, and Skill(do) is explicit-only.
    if let Some(args::Command::Do {
        target: Some(target),
    }) = &args.command
    {
        let kind = command::do_kind::classify(
            target,
            std::env::var(command::do_kind::DO_KIND_ENV).ok().as_deref(),
            command::do_kind::query_sub_issues,
        );
        match kind {
            Ok(kind) => {
                args.do_meta = kind == command::do_kind::DoKind::Meta;
                args.do_rendered_prompt =
                    Some(command::do_prompt::render_current(target, &Ok(kind)));
            }
            Err(error) => {
                eprintln!("[clud] error: {error}");
                std::process::exit(2);
            }
        }
    }
}

/// The routed launch target plus the saved global preferences that later
/// setup prompts compare it against.
#[derive(Clone, Copy)]
struct LaunchResolution {
    target: backend::ResolvedLaunchTarget,
    global: clud_settings::GlobalLaunchPreferences,
}

/// Resolve which harness, provider and model this launch uses, and reject an
/// invalid combination before anything is bootstrapped. Writes the resolved
/// model selection into `args`.
fn resolve_launch(args: &mut args::Args) -> LaunchResolution {
    let mut preferences_error = None;
    // Resolve saved routing policy before the picker: installed executables
    // do not imply that their providers are authorized.
    let (launch_preferences, preferences_known) =
        match clud_settings::load_launch_preferences_read_only() {
            Ok(preferences) => (preferences, true),
            Err(error) => {
                preferences_error = Some(error.to_string());
                eprintln!(
                    "[clud] warning: failed to load launch preferences: {error}; using defaults"
                );
                (clud_settings::LaunchPreferencesSnapshot::default(), false)
            }
        };
    let global_launch_preferences = launch_preferences.global;
    // #1357: Git Bash mintty hands a native exe pipes, not a console, so
    // the launch silently downgrades to subprocess mode. Say so once.
    clud::session::warn_if_mintty_without_console();
    apply_session_history_picker(args, global_launch_preferences.harness);
    let fallback_is_eligible = preferences_known
        && harness_picker::deepseek_fallback_candidate(
            args,
            global_launch_preferences.model_provider,
        )
        && matches!(
            global_launch_preferences.harness,
            None | Some(backend::HarnessSelection::Default | backend::HarnessSelection::Claude)
        );
    let deepseek_credential_fallback = fallback_is_eligible
        && harness_picker::deepseek_only_fallback(
            args,
            global_launch_preferences.model_provider,
            harness_picker::credential_snapshot(),
        );
    let auto_selected_harness = if !deepseek_credential_fallback
        && harness_picker::should_select(
            args,
            io::stdin().is_terminal(),
            io::stderr().is_terminal(),
        ) {
        pick_installed_harness(args)
    } else {
        None
    };
    let (launch_target, provider_profile) = resolve_launch_target(
        args,
        &launch_preferences,
        auto_selected_harness,
        deepseek_credential_fallback,
    );
    if launch_target.model_provider == backend::ModelProvider::OpenRouter {
        if let Some(error) = preferences_error {
            eprintln!("[clud] cannot resolve saved OpenRouter privacy policy: {error}");
            std::process::exit(2);
        }
        args.saved_openrouter_zdr = launch_preferences.openrouter_zdr;
    }
    validate_launch_target(args, launch_target);
    resolve_model_selection(args, launch_target, provider_profile);
    validate_model_boundaries(args, launch_target);
    LaunchResolution {
        target: launch_target,
        global: global_launch_preferences,
    }
}

/// #922: interactive `clud -c` / `--last` on the Claude harness picks a
/// session for this cwd and rewrites the args (provider, resume target,
/// recovery) before the launch target is resolved from them.
fn apply_session_history_picker(
    args: &mut args::Args,
    saved_harness: Option<backend::HarnessSelection>,
) {
    if !clud::session_history::launch::applies(args, saved_harness) {
        return;
    }
    let environment = clud::daemon::default_state_dir().map(|state_dir| {
        let home = clud::home::user_home().unwrap_or_default();
        clud::session_history::launch::Environment {
            state_dir,
            claude_dir: clud::session_history::import::claude_config_dir(&home),
            cwd: std::env::current_dir().unwrap_or_default(),
            interactive: clud::session::terminals_are_interactive(),
        }
    });
    let environment = match environment {
        Ok(environment) => environment,
        Err(error) => {
            eprintln!("[clud] warning: session picker unavailable: {error}");
            return;
        }
    };
    let result = clud::session_history::launch::prepare(args, &environment, |candidates| {
        clud::session_history::picker::prompt(&mut io::stderr(), candidates)
    });
    match result {
        Ok(Some(note)) => eprintln!("{note}"),
        Ok(None) => {}
        Err(error) if error == "cancelled" => std::process::exit(130),
        Err(error) => {
            eprintln!("[clud] error: {error}");
            std::process::exit(2);
        }
    }
}

/// The launcher's installed-harness picker, remembering the choice.
fn pick_installed_harness(args: &args::Args) -> Option<backend::Backend> {
    let installed = harness_picker::discover_installed_with(|candidate| {
        backend_bootstrap::locate_installed_backend(candidate).is_some()
    });
    let saved = clud_settings::load_last_launcher_harness()
        .map_err(|error| {
            if args.verbose {
                eprintln!("[clud] note: could not read last harness choice: {error}");
            }
        })
        .ok()
        .flatten();
    let selected = match harness_picker::selection_flow(&installed, saved) {
        harness_picker::SelectionFlow::NoneInstalled => None,
        harness_picker::SelectionFlow::Immediate(backend) => Some(backend),
        harness_picker::SelectionFlow::Prompt(default) => {
            let stderr = io::stderr();
            let mut out = stderr.lock();
            match harness_picker::prompt(&mut out, installed, default) {
                Ok(harness_picker::PickerOutcome::Selected(backend)) => Some(backend),
                Ok(harness_picker::PickerOutcome::Cancelled) => {
                    eprintln!("[clud] harness selection cancelled");
                    std::process::exit(130);
                }
                Err(error) => {
                    eprintln!("[clud] harness selection failed: {error}");
                    std::process::exit(1);
                }
            }
        }
    };
    if let Some(selected) = selected {
        if let Err(error) = clud_settings::save_last_launcher_harness(selected) {
            eprintln!("[clud] note: could not remember harness choice: {error}");
        }
    }
    selected
}

fn resolve_launch_target<'a>(
    args: &args::Args,
    launch_preferences: &'a clud_settings::LaunchPreferencesSnapshot,
    auto_selected_harness: Option<backend::Backend>,
    deepseek_credential_fallback: bool,
) -> (
    backend::ResolvedLaunchTarget,
    Option<&'a clud_settings::ProviderProfile>,
) {
    let global_launch_preferences = launch_preferences.global;
    // Routing is resolved from a read-only snapshot after the separate
    // launcher-history write above. No provider preference is mutated merely
    // because the user chose an installed harness from the launcher.
    let cli_provider = args
        .explicit_model_provider()
        .or_else(|| auto_selected_harness.map(backend::Backend::as_model_provider))
        .or(deepseek_credential_fallback.then_some(backend::ModelProvider::DeepSeek));
    let cli_harness = args
        .harness
        .or_else(|| auto_selected_harness.map(backend::HarnessSelection::for_backend));
    let model_inferred_provider = args
        .model
        .as_deref()
        .and_then(clud::provider_catalog::infer_provider);
    let direct_provider = cli_provider
        .or(model_inferred_provider)
        .or(global_launch_preferences.model_provider)
        .unwrap_or(backend::ModelProvider::Claude);
    let explicit_provider_intent =
        args.explicit_model_provider().is_some() || model_inferred_provider.is_some();
    let provider_profile = (args.routing_mode() == backend::RoutingMode::Direct)
        .then(|| launch_preferences.profile(direct_provider))
        .flatten();
    let profile_harness = (explicit_provider_intent && cli_harness.is_none())
        .then(|| provider_profile.and_then(|profile| profile.harness))
        .flatten();
    let mut launch_target = match backend::resolve_routed_launch_target(
        args.routing_mode(),
        cli_provider.or(model_inferred_provider),
        cli_harness,
        global_launch_preferences.model_provider,
        profile_harness.or(global_launch_preferences.harness),
    ) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    if profile_harness.is_some() {
        launch_target.harness_source = backend::PreferenceSource::ProviderSetting;
    }
    if deepseek_credential_fallback {
        launch_target.provider_source = backend::PreferenceSource::CredentialFallback;
    }
    (launch_target, provider_profile)
}

/// `--provider-only` pins OpenRouter's upstream routing. The direct route hands
/// it to the harness through `CLAUDE_CODE_EXTRA_BODY`; the unified gateway
/// injects it into every OpenRouter request it forwards (#1863). Nothing else
/// can carry it.
fn validate_provider_only(args: &args::Args, launch_target: backend::ResolvedLaunchTarget) {
    let zdr = clud::openrouter_routing::effective_zdr(args, launch_target.model_provider);
    if args.provider_only.is_empty() && !zdr && !args.no_data_collection {
        return;
    }
    if launch_target.model_provider != backend::ModelProvider::OpenRouter
        || launch_target.effective_harness != backend::Backend::Claude
    {
        eprintln!(
            "[clud] error: OpenRouter routing requirements (--provider-only, --zdr, saved ZDR, \
             --no-data-collection) apply only to an OpenRouter launch using the Claude harness \
             (direct or --unified)"
        );
        std::process::exit(2);
    }
    if let Some(error) = clud::openrouter_routing::invalid_slug(&args.provider_only) {
        eprintln!("[clud] error: {error}");
        std::process::exit(2);
    }
}

fn validate_launch_target(args: &args::Args, launch_target: backend::ResolvedLaunchTarget) {
    if let Err(error) = backend::validate_provider_options(launch_target, args.model.as_deref()) {
        eprintln!("{error}");
        std::process::exit(2);
    }
    if let Some(error) =
        command::interactive_builtin_resume_error(args, launch_target.effective_harness)
    {
        eprintln!("[clud] error: {error}");
        std::process::exit(2);
    }
    if args.redact_secrets && launch_target.effective_harness != backend::Backend::Claude {
        eprintln!(
            "[clud] error: {}",
            clud::secret_redaction::policy::GATEWAY_REQUIRED
        );
        std::process::exit(2);
    }
    validate_provider_only(args, launch_target);
    if let Some(error) = command::grind_launch_error(args, launch_target)
        .or_else(|| command::video_launch_error(args, launch_target))
    {
        eprintln!("[clud] error: {error}");
        std::process::exit(2);
    }
    if launch_target.effective_harness == backend::Backend::DeepSeek {
        let clud_routes_model = launch_target.model_provider == backend::ModelProvider::OpenRouter;
        if let Some(option) = args.unsupported_deepseek_harness_option(clud_routes_model) {
            eprintln!(
                "unsupported option for DeepSeek Harness: {option}; pass native dsh options after --"
            );
            std::process::exit(2);
        }
    }
}

fn resolve_model_selection(
    args: &mut args::Args,
    launch_target: backend::ResolvedLaunchTarget,
    provider_profile: Option<&clud_settings::ProviderProfile>,
) {
    let saved_selection = provider_profile.map(clud_settings::ProviderProfile::selection_defaults);
    let direct_launch = launch_target.routing_mode == backend::RoutingMode::Direct;
    // Only a launch that would otherwise fall back to the catalog default asks
    // for the served model name, so an explicit or saved model never waits on
    // the network (#1192).
    let server_default = (direct_launch
        && args.model.is_none()
        && saved_selection.and_then(|saved| saved.model).is_none())
    .then(|| clud::server_settings::provider_default_model(launch_target.model_provider))
    .flatten();
    args.resolved_model_selection =
        match clud::provider_catalog::resolve_for_launch_with_server_default(
            launch_target.model_provider,
            args.model.as_deref(),
            args.effort.as_deref(),
            args.context_window.as_deref(),
            saved_selection,
            direct_launch,
            server_default,
        ) {
            Ok(selection) => selection,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        };
    refuse_unproven_free_model(args, launch_target);
    // #1304: an explicit `--openrouter --model <id>` becomes the saved default
    // the next plain `clud --openrouter` reuses. Saved before the credential
    // preflight so the choice survives even a launch that stops for a key.
    if let Some(model) = clud_settings::openrouter_model_to_remember(
        launch_target.model_provider,
        args.model.as_deref(),
        args.dry_run,
        args.resolved_model_selection
            .as_ref()
            .and_then(|selection| selection.model.as_deref()),
        saved_selection.and_then(|saved| saved.model),
    ) {
        match clud_settings::save_openrouter_model(model.clone()) {
            Ok(()) => eprintln!("[clud] saved OpenRouter model {model} as the default"),
            Err(error) => {
                eprintln!("[clud] warning: could not save the OpenRouter model: {error}")
            }
        }
    }
}

/// #1833: a `:free` OpenRouter id is a cost promise. Refuse it, before the
/// selection is saved as the default, unless the offline catalog proves the
/// row is free; `--dry-run` refuses the same way.
fn refuse_unproven_free_model(args: &args::Args, launch_target: backend::ResolvedLaunchTarget) {
    if launch_target.model_provider != backend::ModelProvider::OpenRouter {
        return;
    }
    let Some(wire) = args
        .resolved_model_selection
        .as_ref()
        .and_then(|selection| selection.wire_model.as_deref())
    else {
        return;
    };
    let catalog = openrouter_catalog::catalog_cached_or_embedded();
    let verdict = clud::openrouter_free::check(wire, &catalog);
    if let Some(message) = clud::openrouter_free::refusal(wire, &verdict, &catalog) {
        eprintln!("[clud] error: {message}");
        std::process::exit(2);
    }
    // Name the price on every OpenRouter launch, and say when it came from a
    // saved default rather than this command line (#1833).
    let source = args
        .resolved_model_selection
        .as_ref()
        .and_then(|selection| selection.model_source)
        .and_then(|source| serde_json::to_value(source).ok())
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(notice) = clud::openrouter_free::price_notice(wire, &catalog, &source) {
        eprintln!("{notice}");
    }
    // Free ids hit OpenRouter's caps mid-session, which Claude Code reports
    // as a malformed HTTP 200 response; say so before the session starts
    // (#1845).
    if let Some(notice) = clud::openrouter_free::free_limits_notice(wire) {
        eprintln!("{notice}");
    }
}

fn validate_model_boundaries(args: &args::Args, launch_target: backend::ResolvedLaunchTarget) {
    // A pin outside an explicit `--allow-model` fails here, before bootstrap
    // or the first turn: by the time a request is in flight the user has
    // waited, and the refusal would arrive wrapped in the harness's own
    // API-error framing (#1257).
    let allowed_models = args.model_allowlist();
    if let Err(error) = clud::provider_catalog::validate_model_allowlist(
        &allowed_models,
        args.resolved_model_selection
            .as_ref()
            .and_then(|selection| {
                selection
                    .wire_model
                    .as_deref()
                    .or(selection.model.as_deref())
            }),
    ) {
        eprintln!("[clud] error: {error}");
        std::process::exit(2);
    }
    if launch_target.routing_mode != backend::RoutingMode::Unified {
        return;
    }
    let Some(selection) = args
        .resolved_model_selection
        .as_ref()
        .filter(|selection| selection.provider != backend::ModelProvider::Claude)
    else {
        return;
    };
    let has_discovery_route = selection
        .model
        .as_deref()
        .and_then(clud::provider_catalog::model_by_cli_id)
        .and_then(|model| model.discovery_id)
        .is_some();
    if !has_discovery_route {
        eprintln!(
            "unified routing requires a registered Codex or DeepSeek model; '{}' has no gateway discovery route",
            selection
                .wire_model
                .as_deref()
                .or(selection.model.as_deref())
                .unwrap_or("unknown")
        );
        std::process::exit(2);
    }
}

/// `--no-fix-hooks` / `--fix-hooks` and the saved auto-repair setting.
/// Returns whether launch-time hook repairs are enabled; `--fix-hooks`
/// itself exits here.
fn resolve_auto_fix_hooks(args: &args::Args, launch: LaunchResolution) -> bool {
    if args.no_fix_hooks {
        if args.dry_run {
            println!("[clud] dry-run: would disable automatic hook-health repairs globally");
        } else if let Err(error) = clud_settings::save_auto_fix_hooks_enabled(false) {
            eprintln!("[clud] error: failed to persist --no-fix-hooks: {error}");
            std::process::exit(1);
        } else {
            eprintln!(
                "[clud] disabled automatic hook-health repairs globally; run `clud --fix-hooks` to re-enable"
            );
        }
    }
    let auto_fix_hooks = if args.no_fix_hooks {
        false
    } else {
        match clud_settings::load_auto_fix_hooks_enabled() {
            Ok(enabled) => enabled,
            Err(error) => {
                eprintln!(
                    "[clud] warning: failed to load hook-health settings: {error}; using default"
                );
                true
            }
        }
    };

    // Issue #112: explicit hook-parity remediation path. This flag resets the
    // sticky opt-out, applies deterministic repairs, and asks the selected
    // effective harness to migrate hook definitions when semantic translation
    // is needed.
    if args.fix_hooks {
        if !args.dry_run {
            if let Err(error) = clud_settings::save_auto_fix_hooks_enabled(true) {
                eprintln!("[clud] error: failed to persist --fix-hooks: {error}");
                std::process::exit(1);
            }
        }
        std::process::exit(hook_health::run_fix_hooks(args, launch.target));
    }
    auto_fix_hooks
}

/// Inputs that shape the launch but are not routing: piped stdin, the
/// `wasm`/`loop`/`grind` commands, an inline API key, and the provider
/// credential preflight.
fn prepare_launch_inputs(args: &mut args::Args, launch_target: backend::ResolvedLaunchTarget) {
    // Pipe mode: if stdin is not a terminal, read it as the prompt.
    if args.prompt.is_none()
        && args.message.is_none()
        && args.command.is_none()
        && !console_setup::atty_is_terminal()
    {
        let mut input = String::new();
        if io::stdin().read_to_string(&mut input).is_ok() && !input.trim().is_empty() {
            args.prompt = Some(input.trim().to_string());
        }
    }

    if let Some(args::Command::Wasm { module, invoke }) = &args.command {
        std::process::exit(run_wasm(args.dry_run, module, invoke));
    }

    if let Some(args::Command::Loop {
        repeat,
        done,
        no_done,
        ..
    }) = &args.command
    {
        if let Some(msg) =
            command::repeat_implies_no_done_warning(repeat.as_deref(), *no_done, done.as_deref())
        {
            eprintln!("{}", msg);
        }
    }

    resolve_grind_target(args);
    store_inline_api_key(args);
    preflight_provider_credentials(args, launch_target);
}

fn run_wasm(dry_run: bool, module: &str, invoke: &str) -> i32 {
    if dry_run {
        let json = serde_json::json!({
            "mode": "wasm",
            "module": module,
            "invoke": invoke,
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
        return 0;
    }

    match wasm::run_file(module, invoke) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

/// `clud grind` resolves the current repo's issues page from the `origin`
/// remote (GitHub `<repo>/issues`, GitLab `<repo>/-/issues`), prints a green
/// notice, then seeds its native interactive `/loop` prompt. The harness
/// owns repetition and completion. An explicit URL argument is passed through
/// verbatim.
fn resolve_grind_target(args: &mut args::Args) {
    let Some(args::Command::Grind { url }) = args.command.clone() else {
        return;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // `clud grind reconcile` (#1393) runs the feature-branch reconcile
    // pass instead of launching a session.
    if args::is_reconcile(url.as_deref()) {
        match clud::grind_reconcile::run(&cwd) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("[clud] error: {error}");
                std::process::exit(2);
            }
        }
    }
    match grind::resolve_grind_target(&cwd, url.as_deref()) {
        Ok(issues_url) => {
            let color = std::io::IsTerminal::is_terminal(&io::stderr());
            eprintln!("{}", grind::grind_notice(&issues_url, color));
            args.command = Some(args::Command::Grind {
                url: Some(issues_url),
            });
        }
        Err(error) => {
            eprintln!("[clud] error: {error}");
            std::process::exit(2);
        }
    }
}

/// `clud --deepseek <API_KEY>` (and `--kimi` / `--openrouter`): the key was
/// lifted out of the backend argv at parse time, so it is never sent to the
/// model as a prompt. Store it before the preflight looks for one. A dry
/// run never touches the vault.
fn store_inline_api_key(args: &mut args::Args) {
    let Some(key) = args.inline_api_key.take() else {
        return;
    };
    let descriptor = args
        .explicit_model_provider()
        .and_then(clud::provider_registry::descriptor_for);
    let (Some(descriptor), false) = (descriptor, args.dry_run) else {
        return;
    };
    match provider_auth::store_inline_api_key(descriptor, key.expose()) {
        Ok(Some(backend)) => eprintln!(
            "{}",
            provider_auth::inline_key_saved_notice(descriptor, key.expose(), backend)
        ),
        Ok(None) => {}
        Err(error) => {
            eprintln!(
                "{}: could not store the API key passed on the command line: {error}",
                descriptor.settings_id
            );
            std::process::exit(2);
        }
    }
}

/// A dsh launch borrows clud's key only for a provider clud passes through
/// (#1829): OpenRouter always, DeepSeek when the user asked for it by flag.
/// A bare `--harness deepseek` keeps working on a key dsh stores itself.
fn dsh_needs_clud_credential(launch_target: backend::ResolvedLaunchTarget) -> bool {
    match launch_target.model_provider {
        backend::ModelProvider::OpenRouter => true,
        backend::ModelProvider::DeepSeek => {
            launch_target.provider_source == backend::PreferenceSource::Cli
        }
        _ => false,
    }
}

/// Provider credentials must exist before foreground or daemon-backed work
/// is accepted. DeepSeek Harness owns its own provider credentials, so only
/// clud-managed Claude/Codex routes use this preflight.
/// Anthropic-compat-provider (DeepSeek today) work is accepted. Dry-runs
/// intentionally remain vault-free -- `launch_preflight_target` returns
/// `None` for every dry run regardless of provider.
fn preflight_provider_credentials(args: &args::Args, launch_target: backend::ResolvedLaunchTarget) {
    if launch_target.effective_harness == backend::Backend::DeepSeek
        && !dsh_needs_clud_credential(launch_target)
    {
        return;
    }
    let Some(descriptor) =
        provider_auth::launch_preflight_target(launch_target.model_provider, args.dry_run)
    else {
        return;
    };
    let interactive = provider_auth::launch_is_interactive(
        args,
        launch_target.effective_harness,
        io::stdin().is_terminal(),
        io::stderr().is_terminal(),
    );
    if let Err(error) = provider_auth::preflight_native(descriptor, interactive) {
        eprintln!("{}: {}", descriptor.settings_id, error.describe(descriptor));
        std::process::exit(2);
    }
}

/// Host-side preparation for a real launch: Python aliases, soldr shims,
/// bundled tools, hook health, and the large-file guard.
fn prepare_launch_host(
    args: &args::Args,
    launch_target: backend::ResolvedLaunchTarget,
    auto_fix_hooks: bool,
) {
    // Prepare the Python shim aliases only after the Ctrl+C handler is in
    // place. The extract writes ~150 MB of aliases into a cold HOME, which is
    // slow enough to push the SIGINT handler past the interactive test's
    // 0.5 s budget when it runs before the handler is installed (#1180).
    if let Err(error) = clud::shim_install::prepare_current_session() {
        if args.verbose {
            eprintln!("[clud] note: could not prepare Python aliases: {error}");
        }
    }

    // zackees/clud#343: backend launches from repos with `.clud/settings.json`
    // and `rust.use_soldr = true` route cargo / rustc / rustfmt /
    // clippy-driver / rustdoc through soldr by prepending soldr's shim
    // dir to PATH in-process. Run after self-contained utility commands
    // have exited (`clud log`, `clud gc`, `clud config`, etc.) so those fast
    // paths don't block on toolchain probing, but before daemon/backend
    // startup so every launched agent subprocess inherits the shim PATH.
    // `--dry-run` intentionally skips this because it never launches the
    // backend process whose toolchain PATH we need to modify.
    if !args.dry_run {
        soldr_activate::activate_soldr_shims_if_requested();
    }

    // Bundled Python tools are embedded in this binary via BUNDLED_TOOLS.
    // Refresh managed copies during normal foreground startup so an
    // upgraded clud binary replaces stale `~/.clud/tools/...` commands even
    // when an older daemon is already running. `clud tool run` keeps its own
    // inline self-heal path for hook invocations; dry-run remains no-write.
    if !args.dry_run {
        tool_install::ensure_installed();
        clud::block_bad_cmd_rollout::run_startup_checks(auto_fix_hooks);
    }

    if hook_health::should_check_launch(args, launch_target) {
        if auto_fix_hooks && !args.dry_run {
            if let Err(error) = hook_health::apply_default_repairs() {
                eprintln!("[clud] warning: failed to auto-repair hook health: {error}");
            }
        }
        if args.verbose {
            verbose_log::log("[clud] hooks: checking launch parity");
        }
        hook_health::emit_launch_warnings();
    } else if hook_health::should_warn_uv_launch(args) {
        hook_health::emit_uv_launch_warnings();
    }

    // Before any child starts, so the command hook inherits the switch.
    if let Ok(cwd) = std::env::current_dir() {
        clud::clud_repo_dev::apply(&loop_spec::git_root_from(&cwd));
    }

    // Large-file guard runs only on actual backend launches (bare `clud`,
    // `clud --claude`, `clud --codex`, or piped/prompted variants). Skip
    // for every subcommand path: `clud tool run`, `clud loop`, `clud gc`,
    // `clud attach/kill/list/logs`, etc. — those are utility invocations
    // (including compatibility hook shims such as `clud tool run ...`)
    // where the warning would be noise, not signal. The subcommands that
    // already short-circuit above via `std::process::exit` never reached
    // this code anyway; this gate also covers `clud loop` and any future
    // subcommand that falls through.
    if args.command.is_none() && !args.clean_worktrees && !args.fix_hooks {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let root = loop_spec::git_root_from(&cwd);
        if args.verbose {
            verbose_log::log("[clud] large-file guard: scanning project");
        }
        large_file_guard::run(&root);
    }
}

fn admit_provider_bridge(args: &args::Args, launch_target: backend::ResolvedLaunchTarget) {
    // Build only the route metadata needed for credential admission before the
    // daemon can start. The canonical plan below still uses the resolved
    // executable path after the normal bootstrap sequence.
    let bridge_preflight_plan = command::build_launch_plan_for_target(
        args,
        launch_target,
        launch_target.effective_harness.executable_name(),
    );
    // Admit a direct Codex-to-Claude bridge before any daemon bootstrap. In
    // particular, detached and centralized launches must not create a daemon
    // or immediately-dead session when their bridge credential is absent.
    // `--dry-run` intentionally stays credential-free and side-effect-free.
    if args.dry_run {
        return;
    }
    let bridge_interactive = provider_auth::launch_is_interactive(
        args,
        launch_target.effective_harness,
        io::stdin().is_terminal(),
        io::stderr().is_terminal(),
    );
    if let Err(error) = clud::foreground_runtime::admit_codex_bridge_with_cli_import(
        &bridge_preflight_plan,
        bridge_interactive,
    ) {
        launch_log::record_failure_reason(format_args!("failed to start provider bridge: {error}"));
        eprintln!("[clud] failed to start provider bridge: {error}");
        std::process::exit(1);
    }
}

/// Grant daemon-mutation authority only after every utility/special mode
/// has exited and only for the command-less normal launch shape (`clud run`
/// was normalized to this above). Non-launch modes never inherit it.
fn set_daemon_spawn_authority(args: &args::Args) {
    if args.command.is_none() && !args.no_daemon && !args.dry_run {
        unsafe {
            std::env::set_var(daemon::ENV_ALLOW_DAEMON_SPAWN, "1");
        }
    } else {
        unsafe {
            std::env::remove_var(daemon::ENV_ALLOW_DAEMON_SPAWN);
        }
    }
}

/// Issue #135: always-on clud daemon. One background process per user
/// hosts the GC registry (redb owner + worker thread) and is the
/// execution target for `--detach` / `--detachable` / repeat jobs /
/// `--experimental-daemon-centralized`. Foreground interactive
/// launches still use the direct runner by default (PR #152 reverted
/// the attach-pump default). Only a normal launch receives the positive
/// spawn capability. Skip on `--no-daemon` and `--dry-run` so tests that copy
/// the binary into a tempdir don't leave the daemon's `.old` exe
/// locked when tempdir cleanup runs. Never blocks a launch on
/// spawn failure.
fn start_early_daemon(args: &args::Args) -> Option<daemon::ForegroundClientLease> {
    // A centralized session runs its work *through* the daemon, so it needs one
    // just as much as a command-less launch does -- and it needs it started
    // **here**, before the job tracker below, or on Windows the daemon joins
    // this process's Job Object and dies with the CLI (#569). #983 narrowed
    // this to `command.is_none()`, which pushed the centralized path's daemon
    // start after the tracker and left `clud loop --repeat` with a daemon that
    // did not outlive the launch.
    let needs_early_daemon = args.command.is_none()
        || daemon::experimental_enabled(args)
        || matches!(
            args.command.as_ref(),
            Some(
                args::Command::Grind { .. } | args::Command::Do { .. } | args::Command::Fix { .. }
            )
        );
    if !needs_early_daemon || args.no_daemon || args.dry_run {
        if args.verbose {
            verbose_log::log("[clud] daemon: skipped");
        }
        return None;
    }
    // The capability is granted for the same launches that need the
    // daemon; `run_centralized_session` re-grants it harmlessly.
    unsafe {
        std::env::set_var(daemon::ENV_ALLOW_DAEMON_SPAWN, "1");
    }
    if args.verbose {
        verbose_log::log("[clud] daemon: ensure running");
    }
    let state_dir = match args
        .daemon_state_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(daemon::default_state_dir)
    {
        Ok(state_dir) => state_dir,
        Err(e) => {
            eprintln!("[clud] note: cannot resolve state dir: {}", e);
            return None;
        }
    };
    if let Err(e) = daemon::ensure_daemon(&state_dir) {
        if daemon::is_incompatible_daemon_error(&e) {
            daemon::print_incompatible_daemon_error(&e);
            std::process::exit(1);
        }
        eprintln!("[clud] note: daemon unavailable: {}", e);
        if args.verbose {
            verbose_log::log(format_args!("[clud] daemon: unavailable: {e}"));
        }
        return None;
    }
    // #1637: centralized launches take the lease too. Their
    // session only holds the daemon once `Create` lands, and
    // the startup work before it can outlast a short idle
    // timeout; the daemon then retired and `Create` spawned a
    // replacement *after* the job tracker below (#569).
    let lease = match daemon::acquire_foreground_client_lease(&state_dir) {
        Ok(lease) => Some(lease),
        Err(error) => {
            eprintln!("[clud] note: foreground lease unavailable: {error}");
            None
        }
    };
    // Issue #183: record one row in the `repo_visits` table
    // per (repo_root, current launch). Errors are non-fatal:
    // failing to record a visit must never block a launch.
    record_repo_visit_best_effort(&state_dir, args.verbose);
    lease
}

/// After foreground startup has refreshed bundled tools, fire the
/// hook-config scanner against the cwd. Warns on bare
/// `uv run` in Pre/PostToolUse hooks of Python+Rust polyglot
/// repos — the failure mode that turns every hook fire into a
/// multi-minute Rust rebuild on maturin-backed projects (see the
/// tool's docstring for the setuptools-co-existing-with-Cargo
/// variant). Same gate as the large-file guard so the warning
/// only fires for backend launches, not subcommand utility calls
/// (`clud tool run`, `clud loop`, etc.).
fn run_uv_run_hook_guard(args: &args::Args) {
    if args.command.is_some() || args.clean_worktrees || args.fix_hooks {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let root = loop_spec::git_root_from(&cwd);
    if args.verbose {
        verbose_log::log("[clud] uv-run hook guard: scanning agent hooks");
    }
    uv_run_hook_guard::run(&root, args.verbose);
    if args.verbose {
        verbose_log::log("[clud] uv-run hook guard: complete");
    }
}

/// Resolve the backend executable, run launch setup for it, and build the
/// canonical launch plan.
fn build_backend_plan(args: &mut args::Args, launch: LaunchResolution) -> command::LaunchPlan {
    let launch_target = launch.target;
    if args.verbose {
        verbose_log::log("[clud] backend: resolving executable");
    }
    let backend = launch_target.effective_harness;
    let backend_path = resolve_backend_executable(args, backend);
    if args.verbose {
        verbose_log::log("[clud] backend: executable resolved");
    }
    if !args.dry_run {
        require_backend_version(launch_target, &backend_path);
    }
    configure_launch_setup(args, launch, backend);
    if matches!(backend, backend::Backend::Codex) {
        load_codex_config_overrides(args);
    }

    let plan = command::build_launch_plan_for_target(args, launch_target, &backend_path);
    if args.verbose {
        verbose_log::log(format_args!(
            "[clud] plan: backend={} mode={} iterations={} stream_json={}",
            backend.executable_name(),
            plan.launch_mode.as_str(),
            plan.iterations,
            plan.stream_json_progress
        ));
    }

    // Validate the ladder before either path continues, so `--dry-run` catches
    // a bad rung instead of leaving it for the launch that actually costs
    // something. The gateway re-parses it later; this is the early gate, not
    // the authority.
    if let Some(spec) = plan.failover.as_deref() {
        if let Err(error) = failover::FailoverLadder::parse(spec, plan.failover_allow_metered) {
            eprintln!("[clud] --failover: {error}");
            std::process::exit(2);
        }
        if launch_target.routing_mode != backend::RoutingMode::Unified {
            eprintln!(
                "[clud] warning: --failover only applies to `--unified`; a direct launch has no gateway to fail over inside"
            );
        }
    }
    plan
}

fn resolve_backend_executable(args: &args::Args, backend: backend::Backend) -> String {
    let mut bootstrap_host = backend_bootstrap::ProductionBackendBootstrapHost;
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    let stdin = io::stdin();
    let stderr = io::stderr();
    let mut input = stdin.lock();
    let mut err = stderr.lock();
    match backend_bootstrap::resolve_backend_path(
        backend,
        args.dry_run,
        interactive,
        &mut input,
        &mut err,
        &mut bootstrap_host,
    ) {
        Ok(path) => path,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            std::process::exit(error.exit_code());
        }
    }
}

fn require_backend_version(launch_target: backend::ResolvedLaunchTarget, backend_path: &str) {
    let discovery_version = if launch_target.routing_mode == backend::RoutingMode::Unified {
        Some(backend_bootstrap::require_unified_claude_version(
            std::path::Path::new(backend_path),
        ))
    } else if launch_target.model_provider == backend::ModelProvider::Codex
        && launch_target.effective_harness == backend::Backend::Claude
    {
        Some(backend_bootstrap::require_codex_bridge_claude_version(
            std::path::Path::new(backend_path),
        ))
    } else {
        None
    };
    if let Some(Err(error)) = discovery_version {
        eprintln!("{error}");
        std::process::exit(2);
    }
}

/// Issue #242: mutable harness setup is scoped per launch until the user
/// opts into a global selection. Dry-runs read provider/harness preferences
/// for faithful metadata but skip mutable setup. Interactive launches with
/// an explicit provider or harness can opt into global setup through the
/// inline selector and are prompted again when an explicit choice differs
/// from its stored default.
fn configure_launch_setup(args: &args::Args, launch: LaunchResolution, backend: backend::Backend) {
    let launch_target = launch.target;
    let setup_interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    let configured_scope = if args.dry_run {
        None
    } else {
        match clud_settings::load_launch_setup_scope(backend) {
            Ok(scope) => scope,
            Err(error) => {
                if args.verbose {
                    eprintln!("[clud] note: could not read clud settings: {error}");
                }
                None
            }
        }
    };
    let mut persist_prompted_global_selection = false;
    let setup_scope = if let Some(scope) = launch_setup::scope_for_launch_selection(
        args,
        setup_interactive,
        configured_scope,
        launch.global.model_provider,
        launch_target.model_provider,
        launch.global.harness,
        launch_target.requested_harness,
    ) {
        scope
    } else {
        let scope = prompt_launch_setup_scope();
        persist_prompted_global_selection =
            launch_setup::should_persist_prompted_default_backend(args, scope);
        scope
    };
    if persist_prompted_global_selection {
        let explicit_provider = args
            .explicit_model_provider()
            .map(|_| launch_target.model_provider);
        if let Err(error) = clud_settings::save_global_launch_preferences(
            explicit_provider,
            args.harness,
            setup_scope,
        ) {
            eprintln!("[clud] note: could not save global setup preference: {error}");
        }
    }
    if matches!(setup_scope, launch_setup::LaunchSetupScope::Global) {
        let mut err = io::stderr().lock();
        if let Err(error) = launch_setup::run_setup(setup_scope, backend, args.verbose, &mut err) {
            eprintln!("[clud] note: global setup failed: {error}");
        }
    }
    if args.verbose {
        verbose_log::log(format_args!("[clud] setup scope: {}", setup_scope.as_str()));
    }
}

fn prompt_launch_setup_scope() -> launch_setup::LaunchSetupScope {
    let mut err = io::stderr().lock();
    match launch_setup::prompt_scope(&mut err) {
        Ok(scope) => scope,
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            eprintln!("[clud] launch setup cancelled");
            std::process::exit(130);
        }
        Err(error) => {
            eprintln!(
                "[clud] note: could not read launch setup scope ({error}); using session-only"
            );
            launch_setup::LaunchSetupScope::SessionOnly
        }
    }
}

fn load_codex_config_overrides(args: &mut args::Args) {
    match clud_settings::load_or_init_codex_config_overrides(!args.dry_run) {
        Ok(overrides) => {
            args.codex_config_overrides = overrides;
        }
        Err(error) => {
            eprintln!(
                "[clud] warning: failed to load Codex settings: {error}; using default Codex config overrides"
            );
            args.codex_config_overrides = clud_settings::default_codex_config_overrides();
        }
    }
}

fn print_dry_run_and_exit(
    args: &args::Args,
    plan: &command::LaunchPlan,
    launch_target: backend::ResolvedLaunchTarget,
) -> ! {
    let backend = launch_target.effective_harness;
    let dry_run_command = if backend == backend::Backend::Claude {
        clud::foreground_runtime::dry_run_claude_command(plan).unwrap_or_else(|error| {
            eprintln!("[clud] cannot render Claude settings: {error}");
            std::process::exit(2);
        })
    } else {
        plan.command.clone()
    };
    let json = serde_json::json!({
        "command": clud::secret_redaction::redact_args(&dry_run_command),
        "unsafe_mode": plan.unsafe_mode,
        "allow_plan_mode": plan.allow_plan_mode,
        "iterations": plan.iterations,
        "backend": backend.executable_name(),
        "routing_mode": launch_target.routing_mode.as_str(),
        "model_provider": launch_target.model_provider.as_str(),
        "requested_harness": launch_target.requested_harness.as_str(),
        "effective_harness": launch_target.effective_harness.executable_name(),
        "provider_source": launch_target.provider_source.as_str(),
        "harness_source": launch_target.harness_source.as_str(),
        "launch_mode": plan.launch_mode.as_str(),
        "graphics": {
            "mode": plan.graphics.mode.to_string(),
            "image": plan.graphics.image_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        },
        "repeat_interval_secs": plan.repeat_schedule.as_ref().map(|s| s.interval_secs),
        // The resolved Codex selection, expanded from whatever short form
        // was typed, so a dry run shows what will actually be billed.
        "codex_model": plan.codex_model,
        "model_selection": plan.model_selection,
        "codex_model_source": plan.model_selection.as_ref()
            .and_then(|selection| selection.model.as_deref())
            .and_then(|model| clud::codex_runtime::active_choice().source_for(model))
            .map(clud::codex_runtime::Source::as_str),
        // The cost boundary, auditable without a paid request: every
        // model this launch may reach, not just the one it starts on
        // (#1257).
        "allowed_models": plan.allowed_models,
        // OpenRouter upstream providers every request is pinned to, fallbacks
        // off (`--provider-only`).
        "provider_only": plan.provider_only,
        "zdr": plan.zdr,
        "zdr_source": clud::openrouter_routing::zdr_source(args, launch_target.model_provider),
        // Whether that boundary was typed or inherited from the previous
        // selection -- the runtime prints a green startup line for the
        // inherited case (#1257).
        "pinned_from_previous_selection": plan.pinned_from_previous_selection,
        // #1855: every routing decision this launch made, resolved once and
        // carried on the plan. The audit surface the migration is validated
        // against: slots, allowlist, context, effort and check verdicts are
        // the same values the backends render.
        "route": plan.route,
        // #1833: what the offline catalog says about a `:free` id.
        "free_check": plan.model_selection.as_ref()
            .and_then(|selection| selection.wire_model.as_deref())
            .filter(|_| plan.model_provider() == backend::ModelProvider::OpenRouter)
            .map(|wire| clud::openrouter_free::check(
                wire,
                &openrouter_catalog::catalog_cached_or_embedded(),
            ).as_str()),
        "coauthor": plan.coauthor,
        // Routing must be auditable without a paid request, and a ladder
        // is routing: it decides which account serves the turn after the
        // first one declines.
        "failover": plan.failover,
        "failover_allow_metered": plan.failover_allow_metered,
        // #1675: the launch-context record this launch would write
        // (session unbound, launched_at 0). Claude harness only.
        "launch_context": (plan.effective_harness() == backend::Backend::Claude).then(|| {
            let ambient: Vec<(String, String)> = std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect();
            clud::foreground_runtime::launch_context_plan_facts(plan).preview(&ambient)
        }),
        "transcript": args.transcript.as_ref().map(|p| p.to_string_lossy().to_string()),
        "loop_markers": plan.loop_markers.as_ref().map(|m| serde_json::json!({
            "done_path": m.done_path,
            "blocked_path": m.blocked_path,
        })),
    });
    println!("{}", serde_json::to_string_pretty(&json).unwrap());
    std::process::exit(0);
}

fn emit_launch_notices(args: &args::Args, launch_target: backend::ResolvedLaunchTarget) {
    if let Some(notice) =
        backend::saved_harness_override_notice(launch_target, io::stderr().is_terminal(), false)
    {
        eprintln!("{notice}");
    }

    if let Some(notice) = command::plan_mode_suppression_notice(
        args,
        launch_target,
        io::stderr().is_terminal(),
        false,
    ) {
        eprintln!("{notice}");
    }

    // Issue #1610: worktrees size warning from the daemon's cached walk. Reads
    // one small file; no walk, no daemon round trip, silent on any failure.
    if let Some(line) = gc::worktree_size_cache::launch_warning() {
        eprintln!("{line}");
    }
    // Issue #1327: same cached, warn-only check for ~/.clud/tmp.
    if let Some(line) = gc::worktree_size_cache::tmp_launch_warning() {
        eprintln!("{line}");
    }
    // Issue #1691: same cached, warn-only check for ~/.clud/cache.
    if let Some(line) = gc::worktree_size_cache::clud_cache_launch_warning() {
        eprintln!("{line}");
    }
}

/// Run the backend for `plan` and every exit-side cleanup stage. Returns the
/// launch's exit code.
fn launch_and_clean_up(
    args: &args::Args,
    plan: &command::LaunchPlan,
    interrupted: &std::sync::atomic::AtomicBool,
    job_orphan_reaper: Option<job_orphan_reaper::ForegroundJobTracker>,
) -> i32 {
    let dnd_subprocess_guard = register_subprocess_drop_target(args, plan);

    // Issue #73 / #138: enforce the live-session cap. Opens the redb file
    // inside a cross-process advisory lock, performs gc / cap-check /
    // register-self, and **closes redb before returning**. The returned
    // guard holds nothing but a "we registered" flag; on Drop it briefly
    // re-acquires the lock to remove our row. Holding redb for the
    // lifetime of `main` would race with concurrent `clud` launches and
    // print spurious `Database already open` warnings (issue #138).
    if args.verbose {
        verbose_log::log("[clud] session registry: enforcing cap");
    }
    let session_guard = startup::enforce_session_cap();

    register_gc_watch_roots(args.verbose);
    clear_loop_markers(args.verbose, plan);
    let mut loop_session = start_loop_session(args, plan);

    let centralized = daemon::experimental_enabled(args);
    if args.verbose {
        verbose_log::log(if centralized {
            "[clud] launch: centralized daemon"
        } else {
            "[clud] launch: direct runner"
        });
    }
    let launch_log = start_launch_log(plan, centralized);
    // Issue #466: build the CPU-burn banner cfg from CLI flags + settings.
    // Suppressed in non-interactive modes (`--dry-run`, `--detach`,
    // `--detachable`, `--repeat`), by `--no-cpu-banner`, and by the
    // `[foreground.cpu_banner] enabled = false` settings toggle. Builds an
    // inert cfg in any of those cases so `BannerWatcher::spawn` is a no-op.
    let cpu_banner_cfg = build_cpu_banner_cfg(args, plan);
    // #1189: where the banner's toasts render; off whenever the banner is.
    let toast_cfg = build_toast_launch_cfg(&cpu_banner_cfg);

    // Issue #1102: say once, here, that the harness has no trust decision for
    // this workspace and will therefore ignore its `.claude/settings*.json`.
    // Deliberately outside every iteration loop and outside both launch
    // modes' bodies, so an unattended `clud grind` gets one line rather than
    // 200 — and so the centralized-daemon path is covered by the same call.
    if !args.dry_run {
        workspace_trust::warn_if_workspace_untrusted(
            plan.backend,
            plan.cwd.as_deref(),
            plan.iterations,
        );
    }

    // #1168: the same trace file the exit stages below write to was empty on
    // a wedged `clud -p` whose backend had already finished, so the launch
    // phase gets breadcrumbs too. Computed here rather than with the exit
    // stages because both phases share the stderr opt-in.
    let exit_timing = stage_trace::stderr_enabled(args.verbose);
    let launch_started = stage_trace::begin(exit_timing, stage_trace::Phase::Launch, "backend_run");
    let exit_code = if centralized {
        daemon::run_centralized_session(args, plan, interrupted)
    } else {
        run_backend_direct(
            args,
            plan,
            interrupted,
            job_orphan_reaper.as_ref(),
            loop_session.as_mut(),
            BannerCfgs {
                cpu_banner: cpu_banner_cfg,
                toast: toast_cfg,
            },
        )
    };
    stage_trace::done(
        exit_timing,
        stage_trace::Phase::Launch,
        "backend_run",
        launch_started,
    );
    if let Some(handle) = &launch_log {
        stage_trace::scoped(
            exit_timing,
            stage_trace::Phase::Launch,
            "launch_log_finish",
            || handle.finish(exit_code),
        );
    }
    if let Some(session) = loop_session.as_mut() {
        let (summary, err) = runner::summarize_loop_outcome(exit_code);
        stage_trace::scoped(
            exit_timing,
            stage_trace::Phase::Launch,
            "loop_session_end",
            || session.on_loop_end(summary, err),
        );
    }
    stage_trace::scoped(
        exit_timing,
        stage_trace::Phase::Launch,
        "session_guard_drop",
        || drop(session_guard),
    );
    stage_trace::scoped(
        exit_timing,
        stage_trace::Phase::Launch,
        "dnd_guard_drop",
        || drop(dnd_subprocess_guard),
    );
    run_exit_stages(args, exit_timing, job_orphan_reaper);
    if args.verbose {
        verbose_log::log(format_args!("[clud] exit: code {exit_code}"));
    }
    let kind = if centralized {
        ctrl_c_track::InvocationKind::Centralized
    } else {
        ctrl_c_track::InvocationKind::Direct
    };
    flush_ctrl_c_exit_event(kind, exit_code);
    exit_code
}

/// Issue #79 / #65 / #66: register `clud` as the IDropTarget for
/// the console window so dropped files reach the backend. Held for
/// the lifetime of the launch; dropped on graceful exit so the
/// refresh worker thread joins and `RevokeDragDrop` runs. POSIX
/// skips this — terminals there already deliver drops as stdin
/// bytes that the #63 normalizer handles. `--no-dnd` opts out.
///
/// PTY mode wires the registration *inside* `run_plan_pty` so the
/// injector can write into the live PTY via a channel. Subprocess
/// mode registers up-front because the `subprocess_console_injector`
/// operates on the shared console input buffer, no per-iteration
/// state required.
fn register_subprocess_drop_target(
    args: &args::Args,
    plan: &command::LaunchPlan,
) -> Option<clud::dnd::console_drop_target::ConsoleDropTargetGuard> {
    if startup::should_register_drop_target(args)
        && plan.launch_mode == backend::LaunchMode::Subprocess
    {
        if args.verbose {
            verbose_log::log("[clud] dnd: registering subprocess drop target");
        }
        startup::try_register_console_drop_target_subprocess()
    } else {
        if args.verbose {
            verbose_log::log("[clud] dnd: subprocess drop target skipped");
        }
        None
    }
}

/// Issues #545/#546: register conventional discovery roots with the
/// long-lived daemon. This is one best-effort IPC exchange; the daemon
/// deduplicates roots across foreground clients and owns all watching.
fn register_gc_watch_roots(verbose: bool) {
    if verbose {
        verbose_log::log("[clud] gc watch roots: registering with daemon");
    }
    let roots = gc::watch_roots_for_current_repo();
    if let Ok(state_dir) = daemon::default_state_dir() {
        let _ = daemon::try_register_gc_watch(&state_dir, &roots);
    }
}

/// Clear stale DONE/BLOCKED markers from a prior run so that loops don't
/// short-circuit on iteration 1. See loop_spec for semantics.
fn clear_loop_markers(verbose: bool, plan: &command::LaunchPlan) {
    if let Some(ref markers) = plan.loop_markers {
        if verbose {
            verbose_log::log("[clud] loop markers: clearing stale DONE/BLOCKED files");
        }
        loop_spec::clear_markers_at(&loop_spec::MarkerPaths {
            done: std::path::PathBuf::from(&markers.done_path),
            blocked: std::path::PathBuf::from(&markers.blocked_path),
        });
    }
}

/// Issue #96: durable `.clud/loop/` artifacts (info.json, log.txt,
/// motivation.md, working copy of LOOP.md / task file, .gitignore
/// auto-injection). Only active when the user actually ran
/// `clud loop`; other commands skip the bookkeeping entirely.
fn start_loop_session(
    args: &args::Args,
    plan: &command::LaunchPlan,
) -> Option<loop_artifacts::LoopSession> {
    let Some(args::Command::Loop { task, .. }) = &args.command else {
        return None;
    };
    if args.verbose {
        verbose_log::log("[clud] loop artifacts: starting session");
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let git_root = loop_spec::git_root_from(&cwd);
    let _ = loop_spec::ensure_loop_dir(&git_root);
    loop_artifacts::ensure_loop_in_gitignore(&git_root);
    if let Some(t) = task {
        let spec = loop_spec::classify(t);
        let _ = loop_artifacts::materialize_working_copy(&git_root, &spec);
    }
    Some(loop_artifacts::LoopSession::start(
        &git_root,
        plan.iterations,
    ))
}

fn start_launch_log(
    plan: &command::LaunchPlan,
    centralized: bool,
) -> Option<launch_log::LaunchLogHandle> {
    let state_dir = daemon::default_state_dir().ok()?;
    let source = if centralized { "centralized" } else { "direct" };
    match launch_log::start_launch(&state_dir, plan, source) {
        Ok(handle) => Some(handle),
        Err(err) => {
            eprintln!("[clud] warning: failed to record launch start: {err}");
            None
        }
    }
}

/// The CPU-burn banner and its toast sink for one direct launch.
struct BannerCfgs {
    cpu_banner: cpu_banner::CpuBannerCfg,
    toast: toast::ToastLaunchCfg,
}

fn run_backend_direct(
    args: &args::Args,
    plan: &command::LaunchPlan,
    interrupted: &std::sync::atomic::AtomicBool,
    job_orphan_reaper: Option<&job_orphan_reaper::ForegroundJobTracker>,
    loop_session: Option<&mut loop_artifacts::LoopSession>,
    banners: BannerCfgs,
) -> i32 {
    match plan.launch_mode {
        backend::LaunchMode::Subprocess => runner::run_plan_subprocess(
            plan,
            job_orphan_reaper,
            args.verbose,
            interrupted,
            loop_session,
            banners.cpu_banner,
            banners.toast,
        ),
        backend::LaunchMode::Pty => runner::run_plan_pty(
            plan,
            job_orphan_reaper,
            args.verbose,
            interrupted,
            startup::should_register_drop_target(args),
            loop_session,
            banners.cpu_banner,
            banners.toast,
        ),
    }
}

/// Timed exit stages: each one's breadcrumb is written as it starts and
/// finishes, and the totals are summarized at the end. Sinks and format:
/// `stage_trace`.
struct ExitStages {
    enabled: bool,
    stages: Vec<(&'static str, u128)>,
}

impl ExitStages {
    fn begin(&self, name: &str) -> std::time::Instant {
        stage_trace::begin(self.enabled, stage_trace::Phase::Exit, name)
    }

    fn done(&mut self, name: &'static str, started: std::time::Instant) {
        let ms = stage_trace::done(self.enabled, stage_trace::Phase::Exit, name, started);
        self.stages.push((name, ms));
    }

    fn report(&self) {
        if !self.enabled || self.stages.is_empty() {
            return;
        }
        let detail = self
            .stages
            .iter()
            .map(|(name, ms)| format!("{name}={ms}ms"))
            .collect::<Vec<_>>()
            .join(" ");
        let total: u128 = self.stages.iter().map(|(_, ms)| *ms).sum();
        verbose_log::log(format_args!("[clud] exit timing: total={total}ms {detail}"));
    }
}

fn run_exit_stages(
    args: &args::Args,
    exit_timing: bool,
    job_orphan_reaper: Option<job_orphan_reaper::ForegroundJobTracker>,
) {
    // Issue #340: detect env-tagged orphans we are about to leave behind and
    // (unless --keep-orphans) reap them. Skip for detached / detachable
    // sessions — those descendants are intentionally outliving us and are
    // owned by the daemon now.
    // #594: the timeout victims on the Windows lanes finish their payload — the
    // backend's full report is already on stdout — and then miss the budget on
    // the way out, so the open question is which exit stage holds the process
    // open. Each stage below is `O(host processes)` or a daemon round trip, and
    // none of them is currently attributable from a timed-out run.
    //
    // Opt-in only: `verbose_log::log` writes to stderr, and an unconditional
    // line there would break every test that asserts clean stderr -- the exact
    // failure mode #594 already documents. Default runs emit nothing.
    //
    // Breadcrumbs are emitted as each stage starts and finishes, not batched
    // into the summary at the end of this block. The summary only prints on a
    // run that survives every stage -- and the case #594 needs attributed is
    // exactly the one that does not, because the harness kills the process at
    // the timeout. A `begin` with no matching `done` in the captured partial
    // stderr names the stage that held the process open; the summary alone
    // would say nothing at all about it. Sinks and format: `stage_trace`.
    let mut exit_stages = ExitStages {
        enabled: exit_timing,
        stages: Vec::new(),
    };
    if !args.detach && !args.detachable {
        if let Some(tracker) = job_orphan_reaper.as_ref() {
            finish_job_tracker(args, tracker, &mut exit_stages);
        }
        reap_orphans_at_exit(args, &mut exit_stages);
    }

    // Dropping the tracker explicitly, rather than letting it fall off the end
    // of `main`, is what makes its `Drop` attributable. #594 names the
    // completion-port listener join as a suspect precisely because it can wait
    // on a queue that a churny CI host is still feeding -- and an unattributed
    // stall inside an implicit drop is invisible to every stage above. This is
    // the last read of `job_orphan_reaper`, so this only makes the existing
    // drop point explicit; it does not move work.
    let started = exit_stages.begin("tracker_drop");
    // `ForegroundJobTracker`'s `Drop` lives in `#[cfg(windows)] mod imp`, so on
    // every other platform this type has no destructor and clippy's
    // `drop_non_drop` correctly reports the call as a no-op. Suppressed rather
    // than obeyed: the join being timed is the Windows completion-port
    // listener, which is the only platform #594 fires on, and keeping the call
    // unconditional keeps the stage list identical across platforms.
    #[allow(clippy::drop_non_drop)]
    drop(job_orphan_reaper);
    exit_stages.done("tracker_drop", started);
    exit_stages.report();
}

fn finish_job_tracker(
    args: &args::Args,
    tracker: &job_orphan_reaper::ForegroundJobTracker,
    exit_stages: &mut ExitStages,
) {
    // #673 Phase 2d: exits the job tracker gave up on at runtime get one
    // last replan against a fresh process table. Runs before the
    // originator scan because it can reach descendants whose environment
    // was rebuilt and therefore carry no `CLUD:` tag for that scan to
    // find. Skipped with the rest of the exit cleanup on the detached
    // paths, where those descendants are outliving us on purpose.
    if !args.keep_orphans {
        let started = exit_stages.begin("sweep_abandoned_at_exit");
        let swept = tracker.sweep_abandoned_at_exit();
        exit_stages.done("sweep_abandoned_at_exit", started);
        if args.verbose && swept > 0 {
            verbose_log::log(format_args!(
                "[clud] reaper: re-planned {swept} abandoned tool-shell exit(s)"
            ));
        }
    }
    // #673 Phase 5: reaping is destructive and was silent, which is
    // how #651 could be closed while the same symptom kept growing.
    // Suppressed entirely when nothing was tracked.
    let started = exit_stages.begin("finish_and_report");
    let report_lines = tracker.finish_and_report(args.verbose);
    exit_stages.done("finish_and_report", started);
    for line in report_lines {
        if args.quiet_orphans {
            verbose_log::log(format_args!("{line}"));
        } else {
            eprintln!("{line}");
        }
    }
}

fn reap_orphans_at_exit(args: &args::Args, exit_stages: &mut ExitStages) {
    let opts = orphan_reaper::ReapOpts {
        keep: args.keep_orphans,
        quiet: args.quiet_orphans,
        explain: args.explain_orphans,
    };
    let started = exit_stages.begin("scan_and_report");
    let outcome = orphan_reaper::scan_and_report(std::process::id(), &opts);
    exit_stages.done("scan_and_report", started);
    if args.verbose && outcome.found > 0 {
        verbose_log::log(format_args!(
            "[clud] orphan reaper: found={} reaped={}",
            outcome.found, outcome.reaped
        ));
    }

    // Have the daemon do a broader sweep on our behalf: any CLUD-tagged
    // process whose originator is gone (e.g., a sibling clud was
    // SIGKILL'd and never ran its own exit hook) gets reaped on the
    // daemon's background thread. Fire-and-forget with a tight
    // timeout; failure is silently absorbed — the daemon's periodic
    // heartbeat sweep will catch anything we miss, and the next
    // `clud slay` does the synchronous version.
    if !args.keep_orphans {
        if let Ok(state_dir) = daemon::default_state_dir() {
            let started = exit_stages.begin("request_orphan_reap");
            let _ = daemon::try_request_orphan_reap(&state_dir);
            exit_stages.done("request_orphan_reap", started);
        }
    }
}

#[path = "main_helpers.rs"]
mod main_helpers;
use main_helpers::{
    build_cpu_banner_cfg, build_toast_launch_cfg, flush_ctrl_c_exit_event,
    record_repo_visit_best_effort,
};

/// `clud install-assets`: run the launch-time installers on demand.
fn install_assets(home: Option<&std::path::Path>) -> i32 {
    let Some(home) = home
        .map(std::path::Path::to_path_buf)
        .or_else(clud::home::user_home)
    else {
        eprintln!("[clud] error: could not resolve a home directory; pass --home");
        return 2;
    };
    match skills::ensure_installed_at(&home) {
        Ok(reports) => {
            for (backend, report) in reports {
                println!(
                    "skills -> {}: {} installed, {} refreshed",
                    backend.skills_dir(&home).display(),
                    report.installed.len(),
                    report.refreshed.len()
                );
            }
        }
        Err(error) => {
            eprintln!("[clud] error: installing skills: {error}");
            return 1;
        }
    }
    match claude_files::ensure_installed_at(&home) {
        Ok(Some(report)) => println!(
            "claude files -> {}: {} installed, {} refreshed, {} removed",
            home.join(".claude").display(),
            report.installed.len(),
            report.refreshed.len(),
            report.purged.len()
        ),
        Ok(None) => println!(
            "claude files: {} has no .claude directory; skipped",
            home.display()
        ),
        Err(error) => {
            eprintln!("[clud] error: installing agents and workflows: {error}");
            return 1;
        }
    }
    0
}

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use crate::args::{Args, Command, DaemonSubcommand, TopSort};
use crate::backend::LaunchMode;
use crate::command::{has_noninteractive_prompt, LaunchPlan};
use crate::verbose_log;

use super::attach::{attach_to_session, run_attach};
use super::client::{
    daemon_client_proc_snapshot, ensure_daemon, probe_existing, request_daemon_shutdown,
    send_daemon_request,
};
use super::commands::{run_kill, run_list, run_logs};
use super::io_helpers::{read_json_file, resolve_backlog_bytes, terminal_dimensions};
use super::paths::{daemon_events_path, daemon_info_path, state_dir};
use super::server::run_daemon;
use super::sessions::{most_recent_session, most_recent_session_any};
use super::types::{
    DaemonInfo, DaemonRequest, DaemonResponse, SessionKind, WorkerLaunchSpec, ENV_FEATURE_FLAG,
};
use super::worker::run_worker;

const RUNNING_PROCESS_SERVICE_NAME: &str = "clud";
const RUNNING_PROCESS_SERVICE_DEF_DIR_ENV: &str = "RUNNING_PROCESS_SERVICE_DEF_DIR";
const RUNNING_PROCESS_DISABLE_ENV: &str = "RUNNING_PROCESS_DISABLE";
const RUNNING_PROCESS_BROKER_ENV: &str = "CLUD_RUNNING_PROCESS_BROKER";

/// True when the launch should be routed through the centralized session
/// daemon (`daemon::run_centralized_session`) instead of the direct
/// runner in `runner::run_plan_{subprocess,pty}`.
///
/// The centralized path is **opt-in**. Defaulting it on for interactive
/// launches (the PR #151 experiment) exposed a latent bug: the attach
/// pump (`run_remote_interactive`) read stdin through `crossterm::event`,
/// which drops DSR / DA / OSC replies the child TUI writes on startup
/// (same lossy-demultiplexer issue #46 already fixed for the local-PTY
/// runner). With nothing answering claude's `\x1b[6n` query, the TUI
/// hung and the user saw a blank screen. The attach pump now forwards
/// raw stdin bytes (#1355, `attach_input.rs`); the default was not
/// revisited with that fix, so plain `clud` stays on the direct runner.
///
/// Override matrix:
///
/// | Trigger                                  | Centralized? |
/// |------------------------------------------|--------------|
/// | `--detach` / `--detachable` / repeat job | **forced on** |
/// | `--transcript <path>`                    | **forced on** |
/// | `--experimental-daemon-centralized`      | **forced on** (legacy alias) |
/// | `CLUD_EXPERIMENTAL_DAEMON=1`             | **forced on** (legacy alias) |
/// | `--no-daemon`                             | off (kept for explicitness) |
/// | Everything else                          | off (direct runner) |
///
/// The function name `experimental_enabled` is preserved for back-compat
/// (one external call site in `main.rs`); a rename can land in a follow-up.
pub fn experimental_enabled(args: &Args) -> bool {
    let repeat_enabled = matches!(
        args.command,
        Some(Command::Loop {
            repeat: Some(_),
            ..
        })
    );

    args.detach
        || args.detachable
        || args.transcript.is_some()
        || repeat_enabled
        || args.experimental_daemon_centralized
        || env_truthy(ENV_FEATURE_FLAG)
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Whether a `clud daemon <subcommand>` may start a daemon.
///
/// #983 made daemon creation a positive capability, granted in `main.rs` only
/// for a command-less launch so that utility and special modes cannot spawn
/// one. `restart` is the exception the rule missed: starting a replacement is
/// the whole point of the command, and without the grant it fails with
/// `PermissionDenied` — reported as "failed to start replacement daemon" and,
/// when none was running, "no running daemon; starting one" followed by an
/// exit 1.
///
/// `stop` deliberately does not get it: refusing to start one is precisely
/// what it promises, and granting the capability there would let a race turn a
/// stop into a spawn.
fn daemon_subcommand_may_spawn(subcommand: &DaemonSubcommand) -> bool {
    match subcommand {
        DaemonSubcommand::Restart | DaemonSubcommand::ApiInfo { .. } => true,
        DaemonSubcommand::Stop
        | DaemonSubcommand::RunningProcess { .. }
        | DaemonSubcommand::OrphanStatus { .. } => false,
    }
}

fn grant_spawn_for_daemon_subcommand(subcommand: &DaemonSubcommand) {
    if daemon_subcommand_may_spawn(subcommand) {
        grant_daemon_spawn();
    }
}

/// Hand this process the daemon-creation capability.
///
/// SAFETY: single-threaded startup, before any worker exists -- the same point
/// at which `main.rs` grants it for a command-less launch.
fn grant_daemon_spawn() {
    unsafe {
        std::env::set_var(crate::daemon::ENV_ALLOW_DAEMON_SPAWN, "1");
    }
}

/// `clud inject` (docs/architecture/session-inject.md).
fn run_inject_command(args: &Args) -> i32 {
    let Some(Command::Inject {
        session,
        message,
        interrupt,
        client_msg_id,
        from,
        ticket,
        detach,
        timeout,
        json,
    }) = &args.command
    else {
        return 2;
    };
    let state_dir = state_dir(args);
    grant_daemon_spawn();
    let options = super::inject_client::InjectOptions {
        session: session.clone(),
        message: message.clone(),
        interrupt: *interrupt,
        client_msg_id: client_msg_id.clone(),
        from: from.clone(),
        ticket: ticket.clone(),
        detach: *detach,
        timeout_secs: *timeout,
        json: *json,
    };
    super::inject_client::run(&state_dir, &options)
}

fn run_daemon_subcommand(state_dir: &Path, subcommand: &DaemonSubcommand) -> i32 {
    match subcommand {
        DaemonSubcommand::ApiInfo { json } => {
            if let Err(err) = ensure_daemon(state_dir) {
                eprintln!("[clud] daemon unavailable: {err}");
                return 1;
            }
            match super::http::read_api_info(state_dir) {
                Ok((pid, Some(port), Some(token), version)) if *json => {
                    println!(
                        "{}",
                        serde_json::json!({"base_url": format!("http://127.0.0.1:{port}"), "token": token, "pid": pid, "version": version})
                    );
                    0
                }
                Ok((_pid, Some(port), Some(_token), _version)) => {
                    println!("daemon API available on port {port}; use --json for credentials");
                    0
                }
                Ok(_) => {
                    eprintln!("[clud] API listener unavailable; restart daemon");
                    1
                }
                Err(err) => {
                    eprintln!("[clud] cannot read API info: {err}");
                    1
                }
            }
        }
        DaemonSubcommand::Restart => match request_daemon_shutdown(state_dir) {
            Ok(pid) => {
                eprintln!("[clud] daemon pid {pid} stopped");
                if let Err(err) = ensure_daemon(state_dir) {
                    eprintln!("[clud] failed to start replacement daemon: {err}");
                    return 1;
                }
                eprintln!("[clud] new daemon started");
                0
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                eprintln!("[clud] no running daemon; starting one");
                if let Err(err) = ensure_daemon(state_dir) {
                    eprintln!("[clud] failed to start daemon: {err}");
                    return 1;
                }
                0
            }
            Err(err) => {
                if super::client::is_incompatible_daemon_error(&err) {
                    super::client::print_incompatible_daemon_error(&err);
                    return 1;
                }
                eprintln!("[clud] daemon restart failed: {err}");
                1
            }
        },
        DaemonSubcommand::Stop => match request_daemon_shutdown(state_dir) {
            Ok(pid) => {
                eprintln!("[clud] daemon pid {pid} stopped");
                0
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                eprintln!("[clud] no running daemon");
                0
            }
            Err(err) => {
                if super::client::is_incompatible_daemon_error(&err) {
                    super::client::print_incompatible_daemon_error(&err);
                    return 1;
                }
                eprintln!("[clud] daemon stop failed: {err}");
                1
            }
        },
        DaemonSubcommand::RunningProcess { json } => {
            run_running_process_diagnostics(state_dir, *json)
        }
        DaemonSubcommand::OrphanStatus { json } => run_orphan_status(state_dir, *json),
    }
}

/// The freshness-relevant fields of the most recent `orphan_sweep_finished`
/// event (#465).
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanSweepStatus {
    ts_ms: u64,
    found: u64,
    reaped: u64,
}

/// Most recent `orphan_sweep_finished` event from the daemon event-log lines,
/// by `ts_ms` (newest wins). Pure, for testability.
fn latest_orphan_sweep(lines: impl Iterator<Item = String>) -> Option<OrphanSweepStatus> {
    let mut latest: Option<OrphanSweepStatus> = None;
    for line in lines {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("op").and_then(|op| op.as_str()) != Some("orphan_sweep_finished") {
            continue;
        }
        let status = OrphanSweepStatus {
            ts_ms: value.get("ts_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            found: value.get("found").and_then(|v| v.as_u64()).unwrap_or(0),
            reaped: value.get("reaped").and_then(|v| v.as_u64()).unwrap_or(0),
        };
        if latest
            .as_ref()
            .is_none_or(|prev| status.ts_ms >= prev.ts_ms)
        {
            latest = Some(status);
        }
    }
    latest
}

/// Stale when no sweep has ever run, or the last one is older than 2× the sweep
/// interval — evidence the sweep thread has stalled or died (#465 AC).
fn sweep_is_stale(status: Option<&OrphanSweepStatus>, now_ms: u64, interval_ms: u64) -> bool {
    match status {
        None => true,
        Some(status) => now_ms.saturating_sub(status.ts_ms) > interval_ms.saturating_mul(2),
    }
}

/// Event-log files to search for the last sweep, newest-written first: the
/// active log plus the single rotated backup.
///
/// The backup matters in practice. The event log is shared and rotates at 1 MB,
/// and a burst of unrelated high-volume events (e.g. the foreground
/// tool-shell tracker's `foreground_tool_shell_*` stream) can push every
/// `orphan_sweep_finished` record out of the active file within minutes.
/// Reading only the active log then reports a false "no sweep recorded — STALE"
/// on a daemon that is sweeping perfectly well — observed on a live daemon
/// whose two log files held 3.4k tool-shell events and zero sweep events.
fn orphan_status_log_paths(state_dir: &Path) -> Vec<PathBuf> {
    let active = daemon_events_path(state_dir);
    let rotated = match active.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => active.with_extension(format!("{ext}.1")),
        None => active.with_extension("1"),
    };
    vec![active, rotated]
}

/// Parse the dedicated `orphan-sweep.last` sentinel, which the sweep rewrites
/// on every completed pass. Preferred over the shared event log because it
/// cannot be rotated away by unrelated traffic.
fn sentinel_orphan_sweep(state_dir: &Path) -> Option<OrphanSweepStatus> {
    let text =
        std::fs::read_to_string(super::server::orphan_sweep_sentinel_path(state_dir)).ok()?;
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    Some(OrphanSweepStatus {
        ts_ms: value.get("ts_ms").and_then(|v| v.as_u64())?,
        found: value.get("found").and_then(|v| v.as_u64()).unwrap_or(0),
        reaped: value.get("reaped").and_then(|v| v.as_u64()).unwrap_or(0),
    })
}

fn run_orphan_status(state_dir: &Path, json: bool) -> i32 {
    // Sentinel first; fall back to scanning the event logs so a daemon that
    // has not yet written one (older build, or no sweep since upgrade) still
    // reports whatever the logs still hold.
    let status = sentinel_orphan_sweep(state_dir).or_else(|| {
        let mut lines: Vec<String> = Vec::new();
        for path in orphan_status_log_paths(state_dir) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                lines.extend(text.lines().map(str::to_string));
            }
        }
        // `latest_orphan_sweep` picks the newest by `ts_ms`, so concatenation
        // order across the two files does not matter.
        latest_orphan_sweep(lines.into_iter())
    });
    let interval_ms = super::server::orphan_sweep_interval().as_millis() as u64;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let stale = sweep_is_stale(status.as_ref(), now_ms, interval_ms);
    let age_ms = status.as_ref().map(|s| now_ms.saturating_sub(s.ts_ms));

    if json {
        let payload = serde_json::json!({
            "last_sweep_ms": status.as_ref().map(|s| s.ts_ms),
            "found": status.as_ref().map(|s| s.found),
            "reaped": status.as_ref().map(|s| s.reaped),
            "age_ms": age_ms,
            "interval_ms": interval_ms,
            "stale": stale,
        });
        println!("{payload}");
    } else {
        match &status {
            Some(s) => println!(
                "orphan sweep: last {}s ago, found {} reaped {} (interval {}s) — {}",
                age_ms.unwrap_or(0) / 1000,
                s.found,
                s.reaped,
                interval_ms / 1000,
                if stale { "STALE" } else { "ok" }
            ),
            None => println!("orphan sweep: no sweep recorded yet — STALE"),
        }
    }
    i32::from(stale)
}

struct TopRunOptions<'a> {
    json: bool,
    once: bool,
    watch: bool,
    tree: bool,
    flat: bool,
    sort: TopSort,
    limit: usize,
    since: Option<&'a str>,
    originator: Option<&'a str>,
}

fn run_top(state_dir: &Path, opts: TopRunOptions<'_>) -> i32 {
    let since_ms = match super::top::parse_since_ms(opts.since) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("error: {err}");
            return 2;
        }
    };
    let once = opts.once || (opts.json && !opts.watch);
    let flat = opts.flat && !opts.tree;

    // `top` is a utility mode, so it never starts a daemon. With none running
    // there is simply nothing being sampled, which is an empty snapshot rather
    // than an error -- a caller parsing `--json` gets a well-formed answer
    // instead of a failure it has to special-case. Watch mode still fails,
    // because there is nothing to watch.
    if let Err(err) = ensure_daemon(state_dir) {
        eprintln!("[clud] note: daemon unavailable: {err}");
        if !once {
            return 1;
        }
        return render_top_snapshot(
            super::types::ProcTreeSnapshot::empty(super::proc_sampler::DEFAULT_SAMPLE_INTERVAL_MS),
            &opts,
            flat,
        );
    }

    if once {
        return run_top_once(state_dir, since_ms, &opts, flat);
    }
    run_top_live(state_dir, since_ms, &opts, flat)
}

fn run_top_once(state_dir: &Path, since_ms: u64, opts: &TopRunOptions<'_>, flat: bool) -> i32 {
    let snapshot = match daemon_client_proc_snapshot(state_dir, since_ms) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            eprintln!("error: daemon process snapshot unavailable: {err}");
            return 1;
        }
    };
    render_top_snapshot(snapshot, opts, flat)
}

/// Render one snapshot in whichever shape the caller asked for. Shared so the
/// no-daemon empty snapshot goes through exactly the same formatting as a real
/// one, rather than growing a second, drifting JSON shape.
fn render_top_snapshot(
    snapshot: super::types::ProcTreeSnapshot,
    opts: &TopRunOptions<'_>,
    flat: bool,
) -> i32 {
    if opts.json {
        let prepared =
            super::top::prepare_snapshot(snapshot, opts.sort, flat, opts.limit, opts.originator);
        match serde_json::to_string_pretty(&prepared) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("error: failed to render top JSON: {err}");
                return 1;
            }
        }
    } else {
        print!(
            "{}",
            super::top::render_snapshot(&snapshot, opts.sort, flat, opts.limit, opts.originator)
        );
    }
    0
}

fn run_top_live(state_dir: &Path, since_ms: u64, opts: &TopRunOptions<'_>, flat: bool) -> i32 {
    let raw_mode = if opts.json {
        false
    } else {
        crossterm::terminal::enable_raw_mode().is_ok()
    };
    let mut exit_code = 0;
    loop {
        let snapshot = match daemon_client_proc_snapshot(state_dir, since_ms) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                eprintln!("error: daemon process snapshot unavailable: {err}");
                exit_code = 1;
                break;
            }
        };
        let interval_ms = snapshot.interval_ms.clamp(250, 5_000);
        if opts.json {
            let prepared = super::top::prepare_snapshot(
                snapshot,
                opts.sort,
                flat,
                opts.limit,
                opts.originator,
            );
            match serde_json::to_string(&prepared) {
                Ok(text) => println!("{text}"),
                Err(err) => {
                    eprintln!("error: failed to render top JSON: {err}");
                    exit_code = 1;
                    break;
                }
            }
        } else {
            print!("\x1b[2J\x1b[H");
            print!(
                "{}",
                super::top::render_snapshot(
                    &snapshot,
                    opts.sort,
                    flat,
                    opts.limit,
                    opts.originator,
                )
            );
            println!("\npress q to quit");
            let _ = io::stdout().flush();
        }
        if wait_for_top_tick_or_quit(Duration::from_millis(interval_ms), raw_mode) {
            break;
        }
    }
    if raw_mode {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    exit_code
}

fn wait_for_top_tick_or_quit(duration: Duration, raw_mode: bool) -> bool {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if raw_mode {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    if let Ok(crossterm::event::Event::Key(key)) = crossterm::event::read() {
                        if matches!(
                            key.code,
                            crossterm::event::KeyCode::Char('q')
                                | crossterm::event::KeyCode::Char('Q')
                                | crossterm::event::KeyCode::Esc
                        ) {
                            return true;
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        } else {
            thread::sleep(Duration::from_millis(100));
        }
    }
    false
}

fn run_running_process_diagnostics(state_dir: &Path, json: bool) -> i32 {
    let service_def_dir = running_process_service_def_dir();
    let service_def_path =
        service_def_dir.join(format!("{RUNNING_PROCESS_SERVICE_NAME}.servicedef"));
    let daemon_info_path = daemon_info_path(state_dir);
    let recorded_daemon = read_json_file::<DaemonInfo>(&daemon_info_path).ok();
    let live_daemon = probe_existing(state_dir);
    let current_exe = std::env::current_exe().ok();
    let broker_requested = env_flag_eq_one(RUNNING_PROCESS_BROKER_ENV);
    let broker_disabled = env_flag_eq_one(RUNNING_PROCESS_DISABLE_ENV);
    let servicedef_installed = service_def_path.exists();
    let wire_mode = super::rp_broker::WireMode::select();
    let mode = if broker_disabled {
        "disabled-direct-daemon"
    } else {
        "frame-lane-with-tcp-fallback"
    };
    let summary = if broker_disabled {
        "RUNNING_PROCESS_DISABLE=1 selects json-legacy + the direct TCP daemon endpoint; the broker frame lane is bypassed."
    } else {
        "Clud serves a running-process broker v1 frame lane (payload protocol 0x7C4C) next to its TCP wire; the client adopts the broker session (BrokerSession::adopt) and falls back to legacy JSON over TCP on any miss."
    };
    let deferred = [
        "broker-spawned backend adoption (the clud daemon remains self-managed)",
        "Phase 8 escape-hatch removal",
    ];

    if json {
        let payload = serde_json::json!({
            "service_name": RUNNING_PROCESS_SERVICE_NAME,
            "service_definition": {
                "file_name": format!("{RUNNING_PROCESS_SERVICE_NAME}.servicedef"),
                "directory": path_string(&service_def_dir),
                "path": path_string(&service_def_path),
                "directory_env_override": RUNNING_PROCESS_SERVICE_DEF_DIR_ENV,
                "isolation": "SHARED_BROKER",
                "min_version": super::rp_broker::RUNNING_PROCESS_MIN_VERSION,
                "installed_by_clud": servicedef_installed,
                "status": if servicedef_installed { "installed" } else { "pending_first_daemon_bringup" },
            },
            "daemon": {
                "state_dir": path_string(state_dir),
                "info_path": path_string(&daemon_info_path),
                "recorded": daemon_info_json(recorded_daemon.as_ref()),
                "live_reachable": live_daemon.is_some(),
                "recorded_version_matches_current": recorded_daemon
                    .as_ref()
                    .map(|info| info.version.as_deref() == Some(env!("CARGO_PKG_VERSION"))),
                "current_binary": current_exe.as_ref().map(|path| path_string(path)),
            },
            "mode": {
                "current": mode,
                "wire_mode": wire_mode.as_str(),
                "summary": summary,
                "uses_direct_daemon_fallback": broker_disabled,
                "broker_client_wired": !broker_disabled,
                "adopts_broker_session": !broker_disabled,
            },
            "environment": {
                "RUNNING_PROCESS_DISABLE": broker_disabled,
                "CLUD_RUNNING_PROCESS_BROKER": broker_requested,
            },
            "deferred": deferred,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("[clud] failed to render running-process diagnostics: {err}");
                return 1;
            }
        }
    } else {
        println!("running-process adoption status for clud");
        println!("service: {RUNNING_PROCESS_SERVICE_NAME}");
        println!("isolation: SHARED_BROKER");
        println!(
            "min_version: {}",
            super::rp_broker::RUNNING_PROCESS_MIN_VERSION
        );
        println!("servicedef: {}", service_def_path.display());
        println!("servicedef installed: {servicedef_installed}");
        println!("daemon state: {}", state_dir.display());
        println!("daemon info: {}", daemon_info_path.display());
        println!("live daemon reachable: {}", live_daemon.is_some());
        println!("mode: {mode}");
        println!("wire_mode: {}", wire_mode.as_str());
        println!("{summary}");
        println!("deferred:");
        for item in deferred {
            println!("- {item}");
        }
    }

    0
}

fn daemon_info_json(info: Option<&DaemonInfo>) -> serde_json::Value {
    match info {
        Some(info) => serde_json::json!({
            "pid": info.pid,
            "port": info.port,
            "dashboard_port": info.dashboard_port,
            "version": info.version.as_deref(),
        }),
        None => serde_json::Value::Null,
    }
}

fn env_flag_eq_one(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value == "1")
        .unwrap_or(false)
}

/// Single source of truth: running-process's own resolver (honors the
/// `RUNNING_PROCESS_SERVICE_DEF_DIR` override, then platform defaults).
/// The daemon writes `clud.servicedef` into the same directory at
/// bringup (`rp_broker::install_service_definition`).
fn running_process_service_def_dir() -> PathBuf {
    running_process::broker::server::service_definition_dir()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub fn handle_special_command(args: &Args, interrupted: &AtomicBool) -> Option<i32> {
    match &args.command {
        Some(Command::Attach {
            session_id: Some(session_id),
            last,
        }) if !last => {
            let state_dir = state_dir(args);
            if session_id == "-" {
                // "clud attach -" is shorthand for --last
                match most_recent_session(&state_dir) {
                    Some(session) => {
                        eprintln!("[clud] attaching to most recent session: {}", session.id);
                        Some(run_attach(&session.id, &state_dir, interrupted))
                    }
                    None => {
                        println!("No active sessions.");
                        Some(0)
                    }
                }
            } else {
                Some(run_attach(session_id, &state_dir, interrupted))
            }
        }
        Some(Command::Attach { last: true, .. }) => {
            let state_dir = state_dir(args);
            match most_recent_session(&state_dir) {
                Some(session) => {
                    eprintln!("[clud] attaching to most recent session: {}", session.id);
                    Some(run_attach(&session.id, &state_dir, interrupted))
                }
                None => {
                    println!("No active sessions.");
                    Some(0)
                }
            }
        }
        Some(Command::Attach {
            session_id: None,
            last: false,
        }) => {
            let state_dir = state_dir(args);
            let sessions = super::sessions::list_attachable_sessions(&state_dir);
            if sessions.is_empty() {
                println!("No active sessions.");
                println!("Start one with: clud --detach -p <prompt>");
                Some(0)
            } else if sessions.len() == 1 {
                eprintln!("[clud] auto-attaching to only session: {}", sessions[0].id);
                Some(run_attach(&sessions[0].id, &state_dir, interrupted))
            } else {
                Some(run_list(&state_dir))
            }
        }
        Some(Command::Kill { session_id, all }) => {
            let state_dir = state_dir(args);
            Some(run_kill(&state_dir, session_id.as_deref(), *all))
        }
        Some(Command::Slay) => {
            let state_dir = state_dir(args);
            Some(run_kill(&state_dir, None, true))
        }
        Some(Command::List) => {
            let state_dir = state_dir(args);
            Some(run_list(&state_dir))
        }
        Some(Command::Top {
            json,
            once,
            watch,
            tree,
            flat,
            sort,
            limit,
            since,
            originator,
        }) => {
            let state_dir = state_dir(args);
            Some(run_top(
                &state_dir,
                TopRunOptions {
                    json: *json,
                    once: *once,
                    watch: *watch,
                    tree: *tree,
                    flat: *flat,
                    sort: *sort,
                    limit: *limit,
                    since: since.as_deref(),
                    originator: originator.as_deref(),
                },
            ))
        }
        Some(Command::Logs {
            session_id,
            follow,
            lines,
            last,
        }) => {
            let state_dir = state_dir(args);
            // `--last` resolves to the most recently created session,
            // including exited ones — logs are valuable post-mortem.
            let resolved_id: Option<String> = if *last {
                match most_recent_session_any(&state_dir) {
                    Some(session) => {
                        eprintln!(
                            "[clud] showing logs for most recent session: {}",
                            session.id
                        );
                        Some(session.id)
                    }
                    None => {
                        eprintln!("[clud] no sessions found");
                        return Some(1);
                    }
                }
            } else {
                session_id.clone()
            };
            Some(run_logs(
                &state_dir,
                resolved_id.as_deref(),
                *follow,
                *lines,
                interrupted,
            ))
        }
        Some(Command::Daemon { subcommand }) => {
            let state_dir = state_dir(args);
            grant_spawn_for_daemon_subcommand(subcommand);
            Some(run_daemon_subcommand(&state_dir, subcommand))
        }
        Some(Command::Inject { .. }) => Some(run_inject_command(args)),
        Some(Command::InternalDaemon { state_dir }) => {
            super::startup_phases::install_panic_hook(state_dir);
            super::startup_phases::redirect_stderr_to_log(state_dir);
            Some(run_daemon(state_dir, interrupted))
        }
        Some(Command::InternalWorker {
            state_dir,
            session_id,
            daemon_pid,
            spec_file,
        }) => Some(run_worker(state_dir, session_id, *daemon_pid, spec_file)),
        _ => None,
    }
}

/// Pick the worker's `SessionKind` for a centralized-daemon launch.
///
/// The daemon worker's subprocess path wires the backend's stdin to a
/// NULL handle (see `worker::start_subprocess_session`). For interactive
/// claude that's fatal: claude detects no TTY and drops into its built-in
/// `--print` mode, which requires a prompt and errors otherwise
/// ("Input must be provided either through stdin or as a prompt
/// argument when using --print"). The direct runner avoided this by
/// inheriting clud's TTY; the daemon worker can't because the user's
/// terminal belongs to the foreground attach client, not the long-lived
/// worker. Force PTY for interactive launches so the backend gets a
/// pseudo-terminal it can drive.
///
/// `repeat_enabled` keeps overriding to subprocess — repeat jobs run in
/// the background, have their own prompt embedded, and never need a TTY.
fn select_session_kind_for_plan(
    args: &Args,
    plan: &LaunchPlan,
    repeat_enabled: bool,
) -> SessionKind {
    select_session_kind(
        plan.launch_mode,
        repeat_enabled,
        has_noninteractive_prompt(args, plan.backend),
    )
}

fn select_session_kind(
    plan_mode: LaunchMode,
    repeat_enabled: bool,
    noninteractive_prompt: bool,
) -> SessionKind {
    if repeat_enabled {
        return SessionKind::Subprocess;
    }
    if !noninteractive_prompt {
        return SessionKind::Pty;
    }
    match plan_mode {
        LaunchMode::Subprocess => SessionKind::Subprocess,
        LaunchMode::Pty => SessionKind::Pty,
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub fn run_centralized_session(args: &Args, plan: &LaunchPlan, interrupted: &AtomicBool) -> i32 {
    // Reject credential-less direct Codex bridge requests before the daemon
    // creates a session that would immediately die in its worker. `start`
    // repeats this admission check for workers/repeat launches, where a login
    // can disappear after submission.
    if let Err(error) = crate::foreground_runtime::ForegroundRuntime::preflight(plan) {
        crate::launch_log::record_failure_reason(format_args!(
            "failed to start provider bridge: {error}"
        ));
        eprintln!("[clud] failed to start provider bridge: {error}");
        return 1;
    }
    let state_dir = state_dir(args);
    if args.verbose {
        verbose_log::log(format_args!(
            "[clud] daemon: ensure state_dir={}",
            verbose_log::display_path(&state_dir)
        ));
    }
    // #983 made daemon creation a positive capability granted only for a
    // command-less launch. A centralized session is the other legitimate
    // starter: running the work *through* the daemon is the entire meaning of
    // this path, so `clud loop --repeat ...` under an experimental-daemon
    // launch was failing here with PermissionDenied.
    grant_daemon_spawn();
    // #2000: breadcrumbs for a launcher that never announces its session.
    let trace = crate::stage_trace::stderr_enabled(args.verbose);
    let ensured = crate::stage_trace::scoped(
        trace,
        crate::stage_trace::Phase::Launch,
        "daemon-ensure",
        || ensure_daemon(&state_dir),
    );
    if let Err(err) = ensured {
        if super::client::is_incompatible_daemon_error(&err) {
            super::client::print_incompatible_daemon_error(&err);
            return 1;
        }
        eprintln!("[clud] failed to start daemon: {}", err);
        if args.verbose {
            verbose_log::log(format_args!("[clud] daemon: ensure failed: {err}"));
        }
        return 1;
    }
    if args.verbose {
        verbose_log::log("[clud] daemon: ready");
    }

    let repeat_enabled = plan.repeat_schedule.is_some();
    let transcript_path = match args.transcript.as_deref() {
        Some(path) => match prepare_transcript_path(path) {
            Ok(path) => Some(path),
            Err(err) => {
                eprintln!(
                    "[clud] failed to prepare transcript {}: {}",
                    path.display(),
                    err
                );
                return 1;
            }
        },
        None => None,
    };
    let kind = select_session_kind_for_plan(args, plan, repeat_enabled);
    let (rows, cols) = terminal_dimensions();
    let backlog_bytes = resolve_backlog_bytes(args.backlog_size.as_deref());
    let name = args
        .session_name
        .clone()
        .or_else(|| repeat_enabled.then(|| plan.task_summary.clone()).flatten());
    let repeat_run_command = if repeat_enabled {
        match build_repeat_once_command(args, plan) {
            Ok(command) => Some(command),
            Err(err) => {
                eprintln!("[clud] failed to build repeat command: {}", err);
                return 1;
            }
        }
    } else {
        None
    };
    if args.verbose {
        verbose_log::log(format_args!(
            "[clud] daemon: create session kind={:?} detach={} repeat={}",
            kind,
            args.detach || args.detachable,
            repeat_enabled
        ));
    }
    let request = DaemonRequest::Create {
        spec: Box::new(WorkerLaunchSpec {
            plan: plan.clone(),
            kind,
            name,
            detachable: args.detach || args.detachable,
            background_on_launch: args.detach || repeat_enabled,
            attachable: !repeat_enabled,
            rows,
            cols,
            repeat_interval_secs: plan.repeat_schedule.as_ref().map(|s| s.interval_secs),
            repeat_run_command,
            backlog_bytes,
            transcript_path,
            // #933: what the initiator sees, captured now rather than
            // whenever the daemon happened to start.
            client_env: std::env::vars().collect(),
            // Filled by the daemon at admission. The client must never choose
            // the login baseline, or daemon auto-start becomes shell-racy again.
            login_env: Vec::new(),
        }),
    };
    let sent = crate::stage_trace::scoped(
        trace,
        crate::stage_trace::Phase::Launch,
        "daemon-create",
        || send_daemon_request(&state_dir, &request),
    );
    let response = match sent {
        Ok(response) => response,
        Err(err) => {
            eprintln!("[clud] daemon request failed: {}", err);
            if args.verbose {
                verbose_log::log(format_args!("[clud] daemon: request failed: {err}"));
            }
            return 1;
        }
    };

    match response {
        DaemonResponse::Created { session } => {
            if args.verbose {
                verbose_log::log(format_args!("[clud] daemon: session {}", session.id));
            }
            if repeat_enabled {
                eprintln!("[clud] repeat job {} running in background", session.id);
                eprintln!("[clud] list jobs with: clud list");
                return 0;
            }
            if args.detach {
                eprintln!("[clud] session {} running in background", session.id);
                eprintln!("[clud] attach with: clud attach {}", session.id);
                return 0;
            }
            eprintln!("[clud] daemon session {}", session.id);
            {
                attach_to_session(&state_dir, &session, interrupted)
            }
        }
        DaemonResponse::Error { message } => {
            eprintln!("[clud] daemon error: {}", message);
            if args.verbose {
                verbose_log::log(format_args!("[clud] daemon: error: {message}"));
            }
            1
        }
        DaemonResponse::Session { .. }
        | DaemonResponse::Terminated { .. }
        | DaemonResponse::ApiSessionKilled { .. }
        | DaemonResponse::Interrupted { .. }
        | DaemonResponse::AdoptKillAck { .. }
        | DaemonResponse::Gc { .. }
        | DaemonResponse::LiveCwds { .. }
        | DaemonResponse::ShutdownAck { .. }
        | DaemonResponse::ReapOrphansAck { .. }
        | DaemonResponse::Metrics { .. }
        | DaemonResponse::ProcSnapshot { .. }
        | DaemonResponse::ClientLeaseAcquired { .. }
        | DaemonResponse::ClientLeaseReleased { .. } => 1,
    }
}

fn prepare_transcript_path(path: &Path) -> io::Result<PathBuf> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if let Some(parent) = resolved.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&resolved)?;
    Ok(resolved)
}

fn build_repeat_once_command(args: &Args, plan: &LaunchPlan) -> io::Result<Vec<String>> {
    let exe = std::env::current_exe()?;
    let mut command = vec![exe.to_string_lossy().to_string()];
    if plan.routing_mode == crate::backend::RoutingMode::Unified {
        command.push("--unified".to_string());
    } else {
        match plan.model_provider() {
            crate::backend::ModelProvider::Claude => command.push("--claude".to_string()),
            crate::backend::ModelProvider::Codex => command.push("--codex".to_string()),
            crate::backend::ModelProvider::DeepSeek => command.push("--deepseek".to_string()),
            crate::backend::ModelProvider::Kimi => command.push("--kimi".to_string()),
            crate::backend::ModelProvider::OpenRouter => command.push("--openrouter".to_string()),
        }
    }
    command.push("--harness".to_string());
    // A repeat is a fresh process. Pin the resolved effective harness rather
    // than the original preference so a later config/environment change
    // cannot silently route a recorded cross-route job somewhere else.
    command.push(plan.effective_harness().executable_name().to_string());
    if args.safe {
        command.push("--safe".to_string());
    }
    if args.subprocess {
        command.push("--subprocess".to_string());
    }
    if args.pty {
        command.push("--pty".to_string());
    }
    if args.verbose {
        command.push("--verbose".to_string());
    }
    let normalized = plan.model_selection.as_ref();
    if let Some(model) = normalized
        .and_then(|selection| selection.wire_model.as_deref())
        .or(args.model.as_deref())
    {
        command.push("--model".to_string());
        command.push(model.to_string());
    }
    if let Some(effort) = normalized
        .and_then(|selection| selection.effort)
        .map(|effort| effort.as_str())
        .or(args.effort.as_deref())
    {
        command.push("--effort".to_string());
        command.push(effort.to_string());
    }
    if let Some(context_window) = normalized
        .and_then(|selection| selection.context_window.as_deref())
        .or(args.context_window.as_deref())
    {
        command.push("--context-window".to_string());
        command.push(context_window.to_string());
    }
    command.push("loop".to_string());
    if let Some(Command::Loop {
        task,
        loop_count,
        refresh,
        no_done,
        done,
        ..
    }) = &args.command
    {
        command.push("--loop-count".to_string());
        command.push(loop_count.to_string());
        if *refresh {
            command.push("--refresh".to_string());
        }
        if *no_done || done.is_none() {
            command.push("--no-done".to_string());
        }
        if let Some(path) = done.as_deref() {
            command.push("--done".to_string());
            command.push(path.to_string());
        }
        if let Some(task) = task.as_deref() {
            command.push(task.to_string());
        }
    }
    if !args.passthrough.is_empty() {
        command.push("--".to_string());
        command.extend(args.passthrough.iter().cloned());
    }
    Ok(command)
}

#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;

#[cfg(test)]
mod daemon_spawn_capability_tests {
    use super::*;

    /// #983 made daemon creation a positive capability and granted it only for
    /// a command-less launch, which left `clud daemon restart` unable to start
    /// the replacement that is its entire purpose. Every daemon integration
    /// lane went red on main.
    #[test]
    fn restart_may_spawn_because_starting_a_replacement_is_the_point() {
        assert!(daemon_subcommand_may_spawn(&DaemonSubcommand::Restart));
    }

    #[test]
    fn stop_may_not_spawn_because_not_spawning_is_what_it_promises() {
        // Granting the capability here would let a race turn a stop into a
        // spawn, which is exactly what `daemon stop` guarantees against.
        assert!(!daemon_subcommand_may_spawn(&DaemonSubcommand::Stop));
    }

    #[test]
    fn read_only_subcommands_may_not_spawn() {
        assert!(!daemon_subcommand_may_spawn(
            &DaemonSubcommand::RunningProcess { json: false }
        ));
        assert!(!daemon_subcommand_may_spawn(
            &DaemonSubcommand::OrphanStatus { json: false }
        ));
    }
}

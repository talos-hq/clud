//! `clud computer-use`: the MCP entrypoint and the operator commands.
//!
//! `mcp` speaks MCP on stdout, so nothing here may print to stdout on that
//! path: provisioning progress and errors go to stderr, which Claude Code
//! keeps in its MCP log.

use std::path::{Path, PathBuf};

use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

use super::pin::{self, Artifact};
use super::provision::{self, Driver, Resolution};
use super::session_env::{self, DisplayKind, PermissionMode};
use crate::platform::host;

fn home_and_artifact() -> Result<(PathBuf, &'static Artifact), String> {
    let artifact =
        pin::for_host().ok_or("cua-driver publishes no build for this OS/architecture")?;
    let home = crate::home::user_home().ok_or("home directory unavailable")?;
    Ok((home, artifact))
}

fn ensure_driver() -> Result<Driver, String> {
    let (home, artifact) = home_and_artifact()?;
    let inputs = provision::host_inputs(&home, artifact);
    if matches!(provision::resolve(&inputs), Resolution::Missing) {
        eprintln!(
            "[clud] computer use: downloading pinned cua-driver {} ({} MB, verified by SHA-256)",
            pin::VERSION,
            artifact.size / 1_000_000
        );
    }
    provision::ensure(&inputs)
}

fn driver_env(mode: PermissionMode) -> Vec<(String, String)> {
    session_env::child_env(
        std::env::vars().collect(),
        mode,
        host::LINUX,
        &session_env::host_probes(),
    )
}

/// Runs the driver with inherited stdio and returns its exit code.
fn run_driver(driver: &Path, args: &[&str], env: Vec<(String, String)>, hidden: bool) -> i32 {
    let mut argv = vec![driver.to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env),
        capture: false,
        stderr_mode: StderrMode::Pipe,
        stdin_mode: StdinMode::Inherit,
        create_process_group: false,
        creationflags: if hidden {
            crate::win_creation_flags::invisible_helper_creationflags()
        } else {
            None
        },
        nice: None,
        address_space_limit_bytes: None,
    });
    if let Err(error) = process.start() {
        eprintln!("[clud] computer use: cannot start cua-driver: {error}");
        return 1;
    }
    process.wait(None).unwrap_or(1)
}

/// The stdio MCP server: provision on first use, then hand stdio to
/// `cua-driver mcp` with clud's environment.
pub fn mcp() -> i32 {
    let mode = PermissionMode::from_env_value(std::env::var(session_env::MODE_ENV).ok().as_deref());
    let driver = match ensure_driver() {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("[clud] computer use unavailable: {error}");
            return 1;
        }
    };
    if host::MACOS {
        request_tcc_once_in_background(&driver.path);
    }
    run_driver(&driver.path, &["mcp"], driver_env(mode), true)
}

pub fn install() -> i32 {
    match ensure_driver() {
        Ok(driver) => {
            println!(
                "cua-driver {} ready at {} ({})",
                pin::VERSION,
                driver.path.display(),
                source_label(&driver)
            );
            0
        }
        Err(error) => {
            eprintln!("computer use: {error}");
            1
        }
    }
}

pub fn doctor() -> i32 {
    match ensure_driver() {
        Ok(driver) => run_driver(
            &driver.path,
            &["doctor"],
            driver_env(PermissionMode::Standard),
            false,
        ),
        Err(error) => {
            eprintln!("computer use: {error}");
            1
        }
    }
}

fn tcc_marker(home: &Path) -> PathBuf {
    home.join(".clud")
        .join("state")
        .join("computer-use")
        .join("tcc-grant-requested")
}

/// macOS cannot grant Accessibility or Screen Recording silently; the user
/// approves once. clud asks once per machine — the marker is written before
/// the request, so a declined or interrupted dialog is never re-raised.
/// `clud computer-use grant` asks again on demand.
fn request_tcc_once_in_background(driver: &Path) {
    let Some(home) = crate::home::user_home() else {
        return;
    };
    let marker = tcc_marker(&home);
    if marker.exists() || write_marker(&marker).is_err() {
        return;
    }
    let driver = driver.to_path_buf();
    std::thread::spawn(move || {
        let env = driver_env(PermissionMode::Standard);
        run_grant(&driver, env, StdinMode::Null)
    });
}

fn write_marker(marker: &Path) -> std::io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker, format!("cua-driver {}\n", pin::VERSION))
}

fn run_grant(driver: &Path, env: Vec<(String, String)>, stdin: StdinMode) -> i32 {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            driver.to_string_lossy().into_owned(),
            "permissions".into(),
            "grant".into(),
        ]),
        cwd: None,
        env: Some(env),
        // The background request must not write into the MCP stdout channel.
        capture: matches!(stdin, StdinMode::Null),
        stderr_mode: StderrMode::Pipe,
        stdin_mode: stdin,
        create_process_group: false,
        creationflags: None,
        nice: None,
        address_space_limit_bytes: None,
    });
    if process.start().is_err() {
        return 1;
    }
    process.wait(None).unwrap_or(1)
}

pub fn grant() -> i32 {
    if !host::MACOS {
        println!(
            "computer use needs no OS permission grant on this OS; Linux and Windows \
             desktops are reachable without one (see `clud computer-use status`)."
        );
        return 0;
    }
    let driver = match ensure_driver() {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("computer use: {error}");
            return 1;
        }
    };
    if let Some(home) = crate::home::user_home() {
        let _ = write_marker(&tcc_marker(&home));
    }
    run_grant(
        &driver.path,
        driver_env(PermissionMode::Standard),
        StdinMode::Inherit,
    )
}

fn source_label(driver: &Driver) -> &'static str {
    match driver.source {
        provision::Source::Override => "CLUD_CUA_DRIVER override",
        provision::Source::Managed => "clud managed",
    }
}

/// What `status` reports; serialized as-is for `--json`.
#[derive(Debug, serde::Serialize)]
pub struct Status {
    pub pinned_version: &'static str,
    pub target: Option<&'static str>,
    pub driver: Option<String>,
    pub source: Option<provision::Source>,
    pub problem: Option<String>,
    pub display: DisplayKind,
    pub xauthority_repaired: Option<String>,
    pub native_wayland: bool,
    pub default_permission_mode: PermissionMode,
    pub safe_permission_mode: PermissionMode,
    pub notes: Vec<String>,
}

pub fn collect_status() -> Status {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let display = session_env::display_kind(&env, host::LINUX);
    let probes = session_env::host_probes();
    let child = session_env::child_env(env.clone(), PermissionMode::Standard, host::LINUX, &probes);
    let repaired = session_env::repaired_xauthority(&env, probes.exists, probes.list_runtime_dir);
    let native_wayland = child
        .iter()
        .any(|(key, value)| key == "CUA_DRIVER_RS_ENABLE_WAYLAND" && value == "1");
    let mut status = Status {
        pinned_version: pin::VERSION,
        target: pin::for_host().map(|artifact| artifact.target),
        driver: None,
        source: None,
        problem: None,
        display,
        xauthority_repaired: repaired.map(|path| path.display().to_string()),
        native_wayland,
        default_permission_mode: PermissionMode::for_launch(false),
        safe_permission_mode: PermissionMode::for_launch(true),
        notes: display_notes(display),
    };
    fill_resolution(&mut status);
    status
}

fn fill_resolution(status: &mut Status) {
    let (home, artifact) = match home_and_artifact() {
        Ok(found) => found,
        Err(error) => {
            status.problem = Some(error);
            return;
        }
    };
    match provision::resolve(&provision::host_inputs(&home, artifact)) {
        Resolution::Ready(driver) => {
            status.driver = Some(driver.path.display().to_string());
            status.source = Some(driver.source);
        }
        Resolution::BadOverride(path) => {
            status.problem = Some(format!(
                "{}={} is not an executable file",
                provision::OVERRIDE_ENV,
                path.display()
            ));
        }
        Resolution::Missing => {
            status.problem = Some("not installed yet; first use downloads it".into());
        }
    }
}

/// The platform caveats an agent must know before relying on a route.
pub fn display_notes(display: DisplayKind) -> Vec<String> {
    let note = |text: &str| text.to_string();
    match display {
        DisplayKind::X11 => vec![note("X11: windows, screenshots and input all work.")],
        DisplayKind::Wayland { xwayland } => {
            let mut notes = vec![note(
                "Wayland: native windows are listed and read through the compositor and \
                 AT-SPI; pixel input into another app's window is refused by most \
                 compositors, and screenshots may need a one-time portal approval.",
            )];
            notes.push(if xwayland {
                note("XWayland apps behave as on X11 (full window capture and input).")
            } else {
                note("No DISPLAY: XWayland apps are not reachable.")
            });
            notes.push(note(
                "Browser work is most reliable through the browser_* tools (CDP), which do \
                 not depend on the display server.",
            ));
            notes
        }
        DisplayKind::Headless => vec![note(
            "No display: only browser_* tools on a driver-owned browser can work; start \
             Xvfb and export DISPLAY for desktop apps.",
        )],
        DisplayKind::Native if host::MACOS => vec![note(
            "macOS: Accessibility and Screen Recording must be approved once for \
             CuaDriver (System Settings > Privacy & Security); run `clud computer-use grant`.",
        )],
        DisplayKind::Native => vec![note(
            "Windows: works in an interactive desktop session, not in Session 0 (services/SSH).",
        )],
    }
}

pub fn status(json: bool) -> i32 {
    let status = collect_status();
    if json {
        match serde_json::to_string_pretty(&status) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("computer use: {error}");
                return 1;
            }
        }
        return 0;
    }
    print_status(&status);
    0
}

fn print_status(status: &Status) {
    println!(
        "cua-driver pinned: {} ({})",
        status.pinned_version,
        status.target.unwrap_or("unsupported host")
    );
    match (&status.driver, status.source) {
        (Some(path), Some(source)) => println!("driver: {path} ({source:?})"),
        _ => println!(
            "driver: {}",
            status.problem.as_deref().unwrap_or("unavailable")
        ),
    }
    println!("display: {:?}", status.display);
    if let Some(path) = &status.xauthority_repaired {
        println!("xauthority: inherited path is stale; the driver uses {path}");
    }
    if status.native_wayland {
        println!("wayland: native backend on (CLUD_CUA_NATIVE_WAYLAND=0 turns it off)");
    }
    println!(
        "permission mode: {} by default, {} under --safe; Claude never prompts for mcp__cua__*",
        status.default_permission_mode.as_str(),
        status.safe_permission_mode.as_str()
    );
    for note in &status.notes {
        println!("note: {note}");
    }
}

//! Session-startup soldr activation.
//!
//! Called from `main.rs` after self-contained utility commands and dry-run
//! exits, but before daemon/backend subprocesses that might resolve `cargo`
//! / `rustc` from PATH. The flow:
//!
//! 1. [`crate::repo_clud_config::discover_effective_clud_config`] merges
//!    user-level `~/.clud/settings.json` under repo-level
//!    `<repo-root>/.clud/settings.json` (repo wins per-field).
//! 2. If `rust.install` is `true`, install a missing soldr, upgrade one older
//!    than the `soldr shims` floor, or periodically refresh it to the latest
//!    release. A soldr version setting (`rust.version`,
//!    `optimize.rust.soldr_version`) is no longer honored: it is ignored with
//!    a yellow warning, and the installed soldr is used and kept at latest.
//! 3. If `rust.use_soldr` is `true`, spawn `soldr shims --json` and
//!    capture the JSON.
//! 4. Prepend the JSON's `path_entry` to `PATH` in-process. Every
//!    subsequent subprocess inherits the modified PATH and routes its
//!    `cargo` / `rustc` calls through soldr.
//!
//! Failure-mode contract (zackees/clud#343): **every** way the soldr
//! probe can fail — `soldr` not on PATH, exit ≠ 0, hung, malformed
//! JSON, missing `path_entry`, dir doesn't exist — must result in
//! exactly one warning line on stderr and a clean fall-through to
//! "behave as if `.clud/settings.json` were absent". Never panic,
//! never abort the session, never prompt.
//!
//! On-demand soldr install (zackees/clud#343 + user follow-up): when
//! `rust.install` is `true` (default) and soldr is missing, this module
//! attempts to install it via `uv tool install soldr` (preferred) or
//! `pip install --user soldr` (fallback). The install is **best-effort** — a failure
//! engages the same warn-and-continue contract above.

use crate::repo_clud_config::{discover_effective_clud_config, RepoCludConfig};
use crate::subprocess;
use crate::win_creation_flags::invisible_helper_creationflags;
use running_process::{NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const SOLDR_SHIMS_TIMEOUT: Duration = Duration::from_secs(15);
const SOLDR_VERSION_TIMEOUT: Duration = Duration::from_secs(2);
const SOLDR_INSTALL_TIMEOUT: Duration = Duration::from_secs(60);
const SOLDR_LATEST_FRESHNESS: Duration = Duration::from_secs(24 * 60 * 60);
const MIN_SOLDR_SHIMS_VERSION: VersionTriple = VersionTriple(0, 7, 55);

/// Env-var that opts into chatty activation logging. Default is silent
/// so clud's stderr stays empty across normal sessions — `clud` is a
/// CLI orchestrator whose stderr is asserted clean by several
/// integration tests (e.g. `test_probe_demo_gfx_sixel_emits_sixel_bytes`).
/// Users debugging soldr routing set this to `1`.
const VERBOSE_ENV_VAR: &str = "CLUD_VERBOSE_SOLDR_SHIMS";

fn verbose() -> bool {
    let Some(value) = std::env::var_os(VERBOSE_ENV_VAR) else {
        return false;
    };
    let s = value.to_string_lossy();
    !s.is_empty() && s != "0" && !s.eq_ignore_ascii_case("false")
}

/// Print a stderr line only when `CLUD_VERBOSE_SOLDR_SHIMS` is truthy.
macro_rules! verbose_eprintln {
    ($($arg:tt)*) => {{
        if verbose() {
            eprintln!($($arg)*);
        }
    }};
}

/// Expected JSON shape from `soldr shims --json`. We tolerate unknown
/// fields (forward-compat) and only require `schema_version` and
/// `path_entry`.
#[derive(Debug, Deserialize)]
struct SoldrShimsJson {
    schema_version: u32,
    path_entry: Option<String>,
    #[serde(default)]
    soldr_version: Option<String>,
}

/// Top-level entry point. Called from `main.rs` after `trampoline::unlock_exe()`
/// and before any subprocess that might want a soldr-routed cargo.
///
/// Returns `()` unconditionally — every failure path warns + continues.
pub fn activate_soldr_shims_if_requested() {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return,
    };

    let Some(cfg) = discover_effective_clud_config(&cwd) else {
        return;
    };

    if !cfg.rust.use_soldr {
        // Honored opt-out; no warning needed — the user explicitly turned
        // soldr routing off.
        return;
    }

    activate_with_config(&cfg);
}

/// Internal entry point exposed for testing. Same flow as
/// [`activate_soldr_shims_if_requested`] but takes the resolved config
/// directly so tests can stub discovery.
fn activate_with_config(cfg: &RepoCludConfig) {
    // soldr cannot be pinned: a stale pin in one repo's settings kept old
    // soldr protocols alive beside current brokers. Say so, then run the
    // installed soldr and keep it at latest as if no version were set.
    if let Some(pin) = pinned_version(cfg.rust.version.as_deref()) {
        eprintln!("{}", pin_ignored_warning(pin));
    }
    // First probe: is soldr installed, and is it at least the shims floor?
    // A launch never downgrades a newer soldr the user installed.
    let installed = probe_installed_soldr();
    let minimum = Some(MIN_SOLDR_SHIMS_VERSION);
    match soldr_action(installed, minimum) {
        SoldrAction::Install if !cfg.rust.install => {
            verbose_eprintln!(
                "clud: soldr not found on PATH and install is disabled; .clud/settings.json directive ignored"
            );
            return;
        }
        SoldrAction::Install => {
            if let Err(reason) = install_soldr_on_demand(minimum, false) {
                verbose_eprintln!(
                    "clud: failed to install soldr automatically: {reason}; .clud/settings.json directive ignored"
                );
                return;
            }
        }
        SoldrAction::Upgrade if cfg.rust.install => {
            if let Err(reason) = install_soldr_on_demand(minimum, true) {
                verbose_eprintln!(
                    "clud: failed to upgrade soldr to the minimum: {reason}; continuing with the installed soldr"
                );
            }
        }
        SoldrAction::Keep => refresh_rolling_latest(cfg),
        SoldrAction::Upgrade => {}
    }

    // Second probe: ask soldr for the shim dir.
    match run_soldr_shims_json() {
        Ok(shim_info) => {
            prepend_path_entry(&shim_info.path_entry);
            verbose_eprintln!(
                "clud: .clud/settings.json (or ~/.clud/settings.json) detected; routing cargo / rustc / rustfmt / clippy-driver / rustdoc through soldr{version} (shim dir: {dir})",
                version = shim_info
                    .soldr_version
                    .map(|v| format!(" v{v}"))
                    .unwrap_or_default(),
                dir = shim_info.path_entry.display()
            );
        }
        Err(reason) => {
            verbose_eprintln!("clud: {reason}; .clud/settings.json directive ignored");
        }
    }
}

#[derive(Debug)]
struct ShimInfo {
    path_entry: PathBuf,
    soldr_version: Option<String>,
}

/// Spawn `soldr shims --json` and parse the response.
///
/// Returns a `String` reason on failure (already prefixed for the
/// caller's `eprintln!` — caller appends "`.clud/settings.json
/// directive ignored`").
fn run_soldr_shims_json() -> Result<ShimInfo, String> {
    ensure_soldr_supports_shims()?;

    let argv = vec![
        "soldr".to_string(),
        "shims".to_string(),
        "--json".to_string(),
    ];
    let (exit_code, combined) =
        match run_capturing_with_env(argv, SOLDR_SHIMS_TIMEOUT, read_only_soldr_env()) {
            Ok(t) => t,
            Err(SubprocError::Spawn(err)) => {
                return Err(format!("failed to spawn `soldr shims --json`: {err}"));
            }
            Err(SubprocError::Timeout) => {
                return Err(format!(
                    "soldr shims --json timed out after {}s",
                    SOLDR_SHIMS_TIMEOUT.as_secs()
                ));
            }
            Err(SubprocError::Wait(err)) => {
                return Err(format!("waiting on `soldr shims --json` failed: {err}"));
            }
        };

    if exit_code != 0 {
        let lower = combined.to_lowercase();
        if lower.contains("unrecognized subcommand") || lower.contains("unknown subcommand") {
            return Err("this soldr is too old (no 'shims' verb); upgrade to v0.7.55+".to_string());
        }
        let snippet: String = combined.chars().take(200).collect();
        return Err(format!(
            "soldr shims --json exited with code {exit_code}; output: {snippet}"
        ));
    }

    // soldr emits informational `soldr: ...` lines to stderr that get merged
    // into the combined stream by `StderrMode::Stdout`. The JSON payload is a
    // single pretty-printed object that begins with `{` and ends with `}`.
    // Slice from the first `{` to the LAST `}` to skip the prefix lines.
    let json_text = extract_json_object(&combined)
        .ok_or_else(|| "soldr shims --json returned no JSON object in its output".to_string())?;
    let parsed: SoldrShimsJson = serde_json::from_str(json_text)
        .map_err(|e| format!("soldr shims --json returned invalid JSON; parse error: {e}"))?;

    if parsed.schema_version != 1 {
        return Err(format!(
            "soldr shims --json returned unexpected schema version {} (expected 1)",
            parsed.schema_version
        ));
    }

    let path_entry_raw = parsed
        .path_entry
        .ok_or_else(|| "soldr shims --json response missing path_entry".to_string())?;
    let path_entry = PathBuf::from(path_entry_raw);
    if !path_entry.is_dir() {
        return Err(format!(
            "soldr shim dir {} does not exist",
            path_entry.display()
        ));
    }

    Ok(ShimInfo {
        path_entry,
        soldr_version: parsed.soldr_version,
    })
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct VersionTriple(u64, u64, u64);

fn ensure_soldr_supports_shims() -> Result<(), String> {
    let version = installed_soldr_version()?;
    if version < MIN_SOLDR_SHIMS_VERSION {
        return Err(format!(
            "this soldr is too old (v{}.{}.{}; needs v{}.{}.{}+ for 'shims')",
            version.0,
            version.1,
            version.2,
            MIN_SOLDR_SHIMS_VERSION.0,
            MIN_SOLDR_SHIMS_VERSION.1,
            MIN_SOLDR_SHIMS_VERSION.2
        ));
    }
    Ok(())
}

fn installed_soldr_version() -> Result<VersionTriple, String> {
    let argv = vec!["soldr".to_string(), "--version".to_string()];
    let (exit_code, combined) = match run_capturing(argv, SOLDR_VERSION_TIMEOUT) {
        Ok(t) => t,
        Err(SubprocError::Spawn(err)) => {
            return Err(format!("failed to spawn `soldr --version`: {err}"));
        }
        Err(SubprocError::Timeout) => {
            return Err(format!(
                "soldr --version timed out after {}s",
                SOLDR_VERSION_TIMEOUT.as_secs()
            ));
        }
        Err(SubprocError::Wait(err)) => {
            return Err(format!("waiting on `soldr --version` failed: {err}"));
        }
    };

    if exit_code != 0 {
        let snippet: String = combined.chars().take(200).collect();
        return Err(format!(
            "soldr --version exited with code {exit_code}; output: {snippet}"
        ));
    }

    let Some(version) = parse_first_version_triple(&combined) else {
        let snippet: String = combined.chars().take(200).collect();
        return Err(format!(
            "soldr --version returned no parseable version; output: {snippet}"
        ));
    };

    Ok(version)
}

fn parse_first_version_triple(text: &str) -> Option<VersionTriple> {
    for token in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '.') {
        let trimmed = token.trim_start_matches('v');
        let mut parts = trimmed.split('.');
        let Some(major) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        let Some(minor) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        let Some(patch) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        return Some(VersionTriple(major, minor, patch));
    }
    None
}

/// Prepend `path_entry` to `PATH` (idempotent — skip if already at
/// position 0). Modifies the *current process* env so spawned children
/// inherit the change.
fn prepend_path_entry(path_entry: &Path) {
    let separator = if crate::platform::host::WINDOWS {
        ";"
    } else {
        ":"
    };
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let existing_str = existing.to_string_lossy();
    let path_entry_str = path_entry.to_string_lossy();

    // Idempotency: if PATH already starts with this dir, no-op.
    let already_leading = existing_str
        .split(if crate::platform::host::WINDOWS {
            ';'
        } else {
            ':'
        })
        .next()
        .map(|first| {
            if crate::platform::host::WINDOWS {
                first.eq_ignore_ascii_case(&path_entry_str)
            } else {
                first == path_entry_str
            }
        })
        .unwrap_or(false);
    if already_leading {
        return;
    }

    let new_path = if existing.is_empty() {
        path_entry_str.into_owned()
    } else {
        format!("{}{}{}", path_entry_str, separator, existing_str)
    };
    // SAFETY: env::set_var is safe at process startup before any other
    // thread is spawned. clud's main thread reaches this before
    // spawning any worker / runner thread.
    unsafe {
        std::env::set_var("PATH", new_path);
    }
}

/// What soldr looks like on this machine before clud touches it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstalledSoldr {
    /// `soldr` is not on PATH.
    Missing,
    /// `soldr` is on PATH but `soldr --version` gave no usable version.
    Unreadable,
    At(VersionTriple),
}

/// What a launch does to the installed soldr.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SoldrAction {
    Install,
    Upgrade,
    Keep,
}

fn probe_installed_soldr() -> InstalledSoldr {
    if which::which("soldr").is_err() {
        return InstalledSoldr::Missing;
    }
    installed_soldr_version()
        .map(InstalledSoldr::At)
        .unwrap_or(InstalledSoldr::Unreadable)
}

/// The version decision. A configured version is a **minimum**: install
/// when missing, upgrade when older, and otherwise leave the user's soldr
/// alone -- a newer one is never downgraded, and one whose version cannot
/// be read is never clobbered.
fn soldr_action(installed: InstalledSoldr, minimum: Option<VersionTriple>) -> SoldrAction {
    match (installed, minimum) {
        (InstalledSoldr::Missing, _) => SoldrAction::Install,
        (InstalledSoldr::At(have), Some(min)) if have < min => SoldrAction::Upgrade,
        _ => SoldrAction::Keep,
    }
}

/// A soldr version setting other than empty or `"latest"`: a pin, which
/// clud no longer honors.
pub fn pinned_version(version: Option<&str>) -> Option<&str> {
    version
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("latest"))
}

/// The yellow warning shown when a settings file still pins soldr.
pub fn pin_ignored_warning(version: &str) -> String {
    format!(
        "\x1b[33m[clud] ignoring soldr version pin \"{version}\": soldr can no longer be \
         pinned. clud uses the installed soldr and upgrades it to the latest release. \
         Remove `rust.version` / `optimize.rust.soldr_version` from .clud/settings.json, \
         .clud/settings.local.json or ~/.clud/settings.json.\x1b[0m"
    )
}

/// Once a day, move soldr to the newest release. `uv tool upgrade` only
/// moves forward, so this never downgrades either.
fn refresh_rolling_latest(cfg: &RepoCludConfig) {
    if !cfg.rust.install || !claim_latest_refresh(SystemTime::now()) {
        return;
    }
    if let Err(reason) = upgrade_soldr_to_latest() {
        verbose_eprintln!(
            "clud: failed to refresh rolling-latest soldr: {reason}; continuing with the installed soldr"
        );
    }
}

/// Install soldr via `uv tool install` (preferred) or `pip install --user`
/// (fallback), as `soldr>=<minimum>`: the resolver picks the newest release
/// that satisfies the floor, and the uv receipt never records an exact pin
/// that a later `uv tool upgrade soldr` could not move past.
///
/// Returns `Ok(())` only if `soldr` is on PATH afterwards and at least
/// `minimum`.
fn install_soldr_on_demand(minimum: Option<VersionTriple>, force: bool) -> Result<(), String> {
    let pkg = soldr_package_spec(minimum);
    let uv_args = if force {
        vec!["tool", "install", "--force", pkg.as_str()]
    } else {
        vec!["tool", "install", pkg.as_str()]
    };
    let pip_args = ["install", "--user", "--upgrade", pkg.as_str()];

    let via = try_install(&[("uv", uv_args.as_slice())])
        .or_else(|_| try_install(&[("pip", pip_args.as_slice())]))?;
    if which::which("soldr").is_err() {
        return Err(format!(
            "`{via}` reported success but `soldr` is still not on PATH (you may need to add your install dir to PATH manually)"
        ));
    }
    if let Some(min) = minimum {
        let actual = installed_soldr_version()?;
        if actual < min {
            return Err(format!(
                "`{via}` completed but soldr is v{}.{}.{}, below the minimum v{}.{}.{}",
                actual.0, actual.1, actual.2, min.0, min.1, min.2
            ));
        }
    }
    verbose_eprintln!("clud: installed soldr via `{via}`");
    Ok(())
}

fn soldr_package_spec(minimum: Option<VersionTriple>) -> String {
    match minimum {
        None => "soldr".to_string(),
        Some(VersionTriple(major, minor, patch)) => format!("soldr>={major}.{minor}.{patch}"),
    }
}

fn upgrade_soldr_to_latest() -> Result<(), String> {
    try_install(&[("uv", &["tool", "upgrade", "soldr"])])
        .or_else(|_| try_install(&[("pip", &["install", "--user", "--upgrade", "soldr"])]))
        .and_then(|_| {
            which::which("soldr")
                .map(|_| ())
                .map_err(|_| "installer succeeded but soldr is not on PATH".to_string())
        })
}

fn latest_refresh_stamp() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CLUD_SOLDR_LATEST_STAMP") {
        return Some(PathBuf::from(path));
    }
    crate::home::user_home().map(|home| home.join(".clud/cache/soldr/latest-check"))
}

fn claim_latest_refresh(now: SystemTime) -> bool {
    let Some(stamp) = latest_refresh_stamp() else {
        return false;
    };
    if std::fs::metadata(&stamp)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age < SOLDR_LATEST_FRESHNESS)
    {
        return false;
    }
    let Some(parent) = stamp.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let lock = stamp.with_extension("lock");
    if std::fs::metadata(&lock)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= SOLDR_LATEST_FRESHNESS)
    {
        let _ = std::fs::remove_file(&lock);
    }
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .is_err()
    {
        return false;
    }
    let claimed = std::fs::write(&stamp, b"rolling-latest\n").is_ok();
    let _ = std::fs::remove_file(lock);
    claimed
}

/// Try a series of `(installer, args)` candidates. Returns the first
/// one that succeeded, or the last failure reason.
fn try_install(candidates: &[(&str, &[&str])]) -> Result<String, String> {
    let mut last_reason = String::from("no installer attempted");
    for (installer, args) in candidates {
        if which::which(installer).is_err() {
            last_reason = format!("`{installer}` not on PATH");
            continue;
        }
        let summary = format!("{} {}", installer, args.join(" "));
        let mut argv = vec![installer.to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        match run_capturing(argv, SOLDR_INSTALL_TIMEOUT) {
            Ok((0, _output)) => return Ok(summary),
            Ok((code, output)) => {
                last_reason = format!(
                    "`{summary}` exited with code {code}: {}",
                    output.chars().take(200).collect::<String>()
                );
            }
            Err(SubprocError::Timeout) => {
                last_reason = format!(
                    "`{summary}` timed out after {}s",
                    SOLDR_INSTALL_TIMEOUT.as_secs()
                );
            }
            Err(SubprocError::Spawn(err)) => {
                last_reason = format!("failed to spawn `{summary}`: {err}");
            }
            Err(SubprocError::Wait(err)) => {
                last_reason = format!("waiting on `{summary}` failed: {err}");
            }
        }
    }
    Err(last_reason)
}

// ---------------------------------------------------------------------
// Subprocess wrapper — uses `running_process::NativeProcess` per the
// clud-wide ban on `std::process::Command` (lint enforced by CI).
// Mirrors the pattern in `daemon::gc_service::extern_repo::run_gh_capture`.
// ---------------------------------------------------------------------

enum SubprocError {
    Spawn(String),
    Wait(String),
    Timeout,
}

/// Spawn `argv[0] argv[1..]`, capture combined stdout+stderr (per the
/// `StderrMode::Stdout` convention used elsewhere in this crate), and
/// return `(exit_code, combined_output)`. Kills the child on timeout.
/// soldr's front door spawns a per-`HOME` broker for every command it
/// serves. `soldr shims --json` never needs one -- it prints a directory --
/// and every clud integration test runs under a fresh temporary `HOME`, so
/// without this each test left a broker process behind (soldr#3193; 350 of
/// them on one host). `SOLDR_BROKER_AUTOSPAWN=0` is soldr's opt-out;
/// older soldr versions ignore it.
pub(crate) const SOLDR_BROKER_AUTOSPAWN_ENV: &str = "SOLDR_BROKER_AUTOSPAWN";

/// The inherited environment plus the read-only opt-out, for soldr calls
/// that never need a daemon.
fn read_only_soldr_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| key != SOLDR_BROKER_AUTOSPAWN_ENV)
        .collect();
    env.push((SOLDR_BROKER_AUTOSPAWN_ENV.to_string(), "0".to_string()));
    env
}

fn run_capturing(argv: Vec<String>, deadline: Duration) -> Result<(i32, String), SubprocError> {
    run_capturing_impl(argv, deadline, None)
}

fn run_capturing_with_env(
    argv: Vec<String>,
    deadline: Duration,
    env: Vec<(String, String)>,
) -> Result<(i32, String), SubprocError> {
    run_capturing_impl(argv, deadline, Some(env))
}

fn run_capturing_impl(
    argv: Vec<String>,
    deadline: Duration,
    env: Option<Vec<(String, String)>>,
) -> Result<(i32, String), SubprocError> {
    let config = ProcessConfig {
        command: subprocess::command_spec_for_subprocess(argv),
        cwd: None,
        env,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: invisible_helper_creationflags(),
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new(config);
    process
        .start()
        .map_err(|e| SubprocError::Spawn(e.to_string()))?;

    let start = std::time::Instant::now();
    let mut buf = Vec::<u8>::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => {
                buf.extend_from_slice(&event.line);
                buf.push(b'\n');
            }
            ReadStatus::Timeout => {
                if process.returncode().is_some() {
                    break;
                }
                if start.elapsed() >= deadline {
                    let _ = process.kill();
                    let _ = process.wait(Some(Duration::from_secs(5)));
                    return Err(SubprocError::Timeout);
                }
            }
            ReadStatus::Eof => break,
        }
    }
    // Exit is not end of output: the last lines can reach the capture queue
    // after the exit code on a loaded machine. `wait` runs running-process's
    // bounded capture drain, so the queue is complete once it returns (#2159).
    let exit_code = process
        .wait(Some(Duration::from_secs(5)))
        .map_err(|e| SubprocError::Wait(e.to_string()))?;
    for event in process.drain_combined() {
        buf.extend_from_slice(&event.line);
        buf.push(b'\n');
    }
    Ok((exit_code, String::from_utf8_lossy(&buf).to_string()))
}

/// Extract the first valid JSON object from `combined` by slicing from
/// the first `{` to the last `}`. Returns `None` if there is no
/// matching pair. Robust enough for soldr's pretty-printed JSON output
/// mixed with `soldr: ...` informational lines on the same stream.
fn extract_json_object(combined: &str) -> Option<&str> {
    let start = combined.find('{')?;
    let end = combined.rfind('}')?;
    if end >= start {
        Some(&combined[start..=end])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    /// soldr#3193: the shims probe must never leave a broker behind.
    #[test]
    fn read_only_soldr_env_opts_out_of_broker_autospawn_and_keeps_the_rest() {
        // Reads PATH twice, so it must hold the same guard as the tests that
        // mutate PATH; otherwise a parallel case can change it between reads.
        let _g = isolate_path_env();
        let env = read_only_soldr_env();
        let opt_outs: Vec<_> = env
            .iter()
            .filter(|(key, _)| key == SOLDR_BROKER_AUTOSPAWN_ENV)
            .collect();
        assert_eq!(opt_outs.len(), 1);
        assert_eq!(opt_outs[0].1, "0");
        if let Ok(path) = std::env::var("PATH") {
            assert!(env
                .iter()
                .any(|(key, value)| key == "PATH" && *value == path));
        }
    }

    use super::*;
    use crate::repo_clud_config::RustConfig;

    fn cfg_with_rust(r: RustConfig) -> RepoCludConfig {
        RepoCludConfig {
            rust: r,
            bash: crate::repo_clud_config::BashConfig::default(),
            gh: crate::repo_clud_config::GhConfig::default(),
            hook_roots: crate::repo_clud_config::HookRootsConfig::default(),
            bad_commands: Vec::new(),
            bad_pipelines: Vec::new(),
        }
    }

    fn isolate_path_env() -> PathGuard {
        PathGuard::capture()
    }

    /// RAII guard that snapshots PATH on construction and restores it
    /// on drop. Tests that mutate PATH MUST hold one; otherwise
    /// parallel cases stomp each other.
    struct PathGuard {
        prior: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn path_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    impl PathGuard {
        fn capture() -> Self {
            let lock = path_mutex().lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os("PATH");
            Self { prior, _lock: lock }
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prior.take() {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // prepend_path_entry — idempotency + ordering.
    // -----------------------------------------------------------------

    #[test]
    fn prepend_idempotent_when_already_leading() {
        let _g = isolate_path_env();
        let shim = std::env::temp_dir().join("clud-shim-idempotent");
        std::fs::create_dir_all(&shim).unwrap();
        let sep = if crate::platform::host::WINDOWS {
            ";"
        } else {
            ":"
        };
        let other = std::env::temp_dir().join("other");
        std::fs::create_dir_all(&other).unwrap();

        let starting = format!("{}{sep}{}", shim.display(), other.display());
        unsafe {
            std::env::set_var("PATH", &starting);
        }
        prepend_path_entry(&shim);
        let after = std::env::var("PATH").unwrap();
        assert_eq!(after, starting, "no double-prepend when already leading");
    }

    #[test]
    fn prepend_inserts_at_position_zero() {
        let _g = isolate_path_env();
        let shim = std::env::temp_dir().join("clud-shim-prepend");
        std::fs::create_dir_all(&shim).unwrap();
        let other = std::env::temp_dir().join("other-prepend");
        std::fs::create_dir_all(&other).unwrap();

        unsafe {
            std::env::set_var("PATH", other.display().to_string());
        }
        prepend_path_entry(&shim);
        let after = std::env::var("PATH").unwrap();
        let sep = if crate::platform::host::WINDOWS {
            ';'
        } else {
            ':'
        };
        let first = after.split(sep).next().unwrap();
        assert_eq!(
            first,
            shim.display().to_string(),
            "shim dir must be at PATH[0]: {after}"
        );
    }

    #[test]
    fn prepend_handles_empty_starting_path() {
        let _g = isolate_path_env();
        unsafe {
            std::env::remove_var("PATH");
        }
        let shim = std::env::temp_dir().join("clud-shim-empty");
        std::fs::create_dir_all(&shim).unwrap();

        prepend_path_entry(&shim);
        let after = std::env::var("PATH").unwrap();
        assert_eq!(after, shim.display().to_string());
    }

    // -----------------------------------------------------------------
    // activate_with_config — failure-mode contract.
    //
    // We can't trivially stub `soldr` on PATH in a unit test without
    // platform-specific shenanigans, so the integration-level "spawn
    // a stub soldr" tests live in `tests/`. Here we just verify the
    // shape of activate_with_config when use_soldr=false (must early-
    // return without touching PATH).
    // -----------------------------------------------------------------

    #[test]
    fn activate_with_use_soldr_false_is_a_no_op_on_path() {
        let _g = isolate_path_env();
        let baseline_dir = std::env::temp_dir().join("clud-shim-disabled-baseline");
        std::fs::create_dir_all(&baseline_dir).unwrap();
        unsafe {
            std::env::set_var("PATH", baseline_dir.display().to_string());
        }
        let baseline = std::env::var_os("PATH");
        let cfg = cfg_with_rust(RustConfig {
            use_soldr: false,
            install: true,
            version: None,
            porcelain: false,
        });
        activate_with_config(&cfg);
        assert_eq!(
            std::env::var_os("PATH"),
            baseline,
            "use_soldr=false must not mutate PATH"
        );
    }

    // -----------------------------------------------------------------
    // Pinned-version pkg spec.
    // -----------------------------------------------------------------

    #[test]
    fn rolling_refresh_claim_is_freshness_bounded() {
        let _g = isolate_path_env();
        let temp = tempfile::tempdir().expect("temporary stamp directory");
        let stamp = temp.path().join("latest-check");
        unsafe {
            std::env::set_var("CLUD_SOLDR_LATEST_STAMP", &stamp);
        }
        let now = SystemTime::now();
        assert!(claim_latest_refresh(now));
        assert!(!claim_latest_refresh(now + Duration::from_secs(60)));
        assert!(claim_latest_refresh(
            now + SOLDR_LATEST_FRESHNESS + Duration::from_secs(1)
        ));
        unsafe {
            std::env::remove_var("CLUD_SOLDR_LATEST_STAMP");
        }
    }

    #[test]
    fn version_parser_finds_soldr_semver_token() {
        assert_eq!(
            parse_first_version_triple("soldr 0.8.0"),
            Some(VersionTriple(0, 8, 0))
        );
        assert_eq!(
            parse_first_version_triple("soldr v0.7.55\n"),
            Some(VersionTriple(0, 7, 55))
        );
        assert_eq!(
            parse_first_version_triple("setup-soldr: using soldr 0.7.45"),
            Some(VersionTriple(0, 7, 45))
        );
    }

    // -----------------------------------------------------------------
    // Version decision: a pin is a minimum, never an exact target.
    // A launch must never downgrade a soldr the user installed (a repo
    // pinning 0.7.11 used to force-reinstall 0.7.11 over 0.9.x).
    // -----------------------------------------------------------------

    #[test]
    fn missing_soldr_is_installed() {
        assert_eq!(
            soldr_action(InstalledSoldr::Missing, None),
            SoldrAction::Install
        );
        assert_eq!(
            soldr_action(InstalledSoldr::Missing, Some(VersionTriple(0, 9, 27))),
            SoldrAction::Install
        );
    }

    #[test]
    fn soldr_older_than_minimum_is_upgraded() {
        assert_eq!(
            soldr_action(
                InstalledSoldr::At(VersionTriple(0, 7, 11)),
                Some(VersionTriple(0, 9, 27))
            ),
            SoldrAction::Upgrade
        );
    }

    #[test]
    fn soldr_at_or_above_minimum_is_left_alone() {
        let min = Some(VersionTriple(0, 7, 11));
        assert_eq!(
            soldr_action(InstalledSoldr::At(VersionTriple(0, 7, 11)), min),
            SoldrAction::Keep
        );
        assert_eq!(
            soldr_action(InstalledSoldr::At(VersionTriple(0, 9, 27)), min),
            SoldrAction::Keep,
            "a newer installed soldr must never be downgraded to the pin"
        );
        assert_eq!(
            soldr_action(InstalledSoldr::At(VersionTriple(0, 9, 27)), None),
            SoldrAction::Keep
        );
    }

    #[test]
    fn unreadable_soldr_version_is_left_alone() {
        assert_eq!(
            soldr_action(InstalledSoldr::Unreadable, Some(VersionTriple(9, 9, 9))),
            SoldrAction::Keep
        );
    }

    #[test]
    fn a_numeric_soldr_version_is_a_pin_and_latest_is_not() {
        assert_eq!(pinned_version(Some("0.9.27")), Some("0.9.27"));
        assert_eq!(pinned_version(Some(" 0.7.11 ")), Some("0.7.11"));
        assert_eq!(pinned_version(Some("latest")), None);
        assert_eq!(pinned_version(Some("LATEST")), None);
        assert_eq!(pinned_version(Some("")), None);
        assert_eq!(pinned_version(None), None);
    }

    #[test]
    fn the_pin_warning_is_yellow_and_says_the_pin_is_ignored() {
        let w = pin_ignored_warning("0.9.27");
        assert!(w.starts_with("\x1b[33m") && w.ends_with("\x1b[0m"), "{w:?}");
        assert!(w.contains("\"0.9.27\""), "{w}");
        assert!(w.contains("can no longer be pinned"), "{w}");
        assert!(w.contains("latest release"), "{w}");
    }

    #[test]
    fn install_spec_is_a_floor_not_an_exact_pin() {
        assert_eq!(
            soldr_package_spec(Some(VersionTriple(0, 9, 27))),
            "soldr>=0.9.27"
        );
        assert_eq!(soldr_package_spec(None), "soldr");
    }

    #[test]
    fn shims_version_floor_rejects_old_soldr() {
        assert!(VersionTriple(0, 7, 45) < MIN_SOLDR_SHIMS_VERSION);
        assert!(VersionTriple(0, 7, 55) >= MIN_SOLDR_SHIMS_VERSION);
        assert!(VersionTriple(0, 8, 0) >= MIN_SOLDR_SHIMS_VERSION);
    }
}

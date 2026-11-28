//! Crash-report writer for clud.
//!
//! Two install entry points:
//!
//! - [`install`] sets up a Rust panic hook that writes a JSON record on every
//!   `panic!()` before chaining to the previously-installed hook so stderr
//!   behavior is preserved.
//! - [`install_native`] additionally attaches a native crash handler (via the
//!   `crash-handler` crate) for SIGSEGV / SIGBUS / SIGILL / SIGFPE / SIGABRT
//!   on Unix and structured exceptions (EXCEPTION_ACCESS_VIOLATION, etc.) on
//!   Windows. The crate explicitly does not attach a SIGINT / CTRL_C_EVENT
//!   handler, so the existing `ctrlc`-based Ctrl-C path (#372 /
//!   `ctrl_c_track`) remains authoritative for user-initiated cancellation.
//!
//! Both paths share one writer that produces records at
//! `~/.clud/state/crashes/<unix_ms>-<role>-<pid>.json`, with rotation at 50
//! reports.
//!
//! The first `install(role)` call surfaces a one-line stderr notice if there's
//! a crash report newer than the last one this process tree saw, so the next
//! launch tells the user something happened without spamming on every
//! subsequent launch.
//!
//! Role can be updated by a later `install(role)` call (e.g. main.rs installs
//! as `"foreground"`, then the daemon process re-installs as `"daemon"`
//! before doing daemon work); the underlying hook is installed only once, so
//! a crash inside the daemon writes one report tagged `"daemon"`, not two.

use std::backtrace::Backtrace;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub(crate) const MAX_REPORTS: usize = 50;
const LAST_SEEN_FILE: &str = "last_seen";

/// The target triple this binary was built for, stamped by `build.rs`.
///
/// Issue #1016: with debug info split out of the wheel, symbolication needs
/// the matching sidecar, and the `.dwp` / `.pdb` / `.dSYM` is per-triple. The
/// binary is the only thing that knows which build it is -- the wheel a user
/// installed does not otherwise record it, and `env!("TARGET")` does not
/// compile in a bin because cargo sets `TARGET` only for build scripts.
///
/// `"unknown"` if the build script could not read it, so a missing value is
/// visible in a report rather than failing the build.
pub const BUILD_TARGET: &str = env!("CLUD_TARGET");

static CURRENT_ROLE: OnceLock<RwLock<String>> = OnceLock::new();
static HOOK_INSTALLED: OnceLock<()> = OnceLock::new();
static NATIVE_INSTALLED: OnceLock<()> = OnceLock::new();

/// JSON shape written to `~/.clud/state/crashes/<unix_ms>-<role>-<pid>.json`.
///
/// All fields except `version`, `role`, `pid`, `timestamp_unix_ms`,
/// `backtrace`, and `args` are optional because panic-driven and native-crash
/// reports populate different subsets of the schema.
#[derive(Serialize)]
struct CrashReport {
    version: String,
    /// Which build this is. Version alone does not identify a sidecar: the
    /// same version is released for every triple.
    target: &'static str,
    /// Which *build* this is (#1016 item 3). Version and triple identify a
    /// release; a rebuilt binary at the same version would pair with the
    /// wrong sidecar and mis-symbolicate. `None` where the format carries no
    /// such id, which is every non-ELF target today.
    #[serde(skip_serializing_if = "Option::is_none")]
    build_id: Option<&'static str>,
    role: String,
    /// `"panic"` (Rust panic hook) or `"native"` (signal / structured
    /// exception handler).
    kind: &'static str,
    pid: u32,
    cwd: Option<String>,
    args: Vec<String>,
    timestamp_unix_ms: u128,
    // Panic-only fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    panic_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    panic_message: Option<String>,
    // Native-crash-only fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_or_exception: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exception_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    faulting_address: Option<String>,
    backtrace: String,
}

/// Install (or update) the clud crash reporter for this process.
///
/// Called from `main.rs` (`role = "foreground"`), the daemon process entry
/// (`role = "daemon"`), and the worker process entry (`role = "worker"`).
/// Idempotent: the panic hook itself is installed exactly once per process;
/// subsequent calls only update the role the hook will tag reports with.
pub fn install(role: &str) {
    // Resolve the build id while we are still in ordinary startup. The native
    // handler runs in signal context, where the file read this needs is not
    // async-signal-safe; the panic hook is fine either way, but both paths
    // share the report builder so both read the cached value.
    crate::build_id::prime();

    let lock = CURRENT_ROLE.get_or_init(|| RwLock::new(role.to_string()));
    if let Ok(mut w) = lock.write() {
        *w = role.to_string();
    }

    HOOK_INSTALLED.get_or_init(|| {
        if let Ok(dir) = crashes_dir() {
            // Convert records a crashed previous run left in the spool
            // before looking for the newest report to surface.
            if let Ok(spool) = crash_spool_dir() {
                collect_spooled_crashes(&spool, &dir);
            }
            if let Some((_, path)) = surface_previous_report(&dir) {
                eprintln!("clud: previous crash report at {}", path.display());
                // Opportunistic symbol-status hint: if the report's
                // backtrace has no `at FILE:LINE` frames, point the user
                // at `clud symbols verify` (#374 PR 3). Best-effort; if
                // we can't read or parse the report we stay quiet.
                if let Ok(raw) = fs::read_to_string(&path) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                        if let Some(bt) = value.get("backtrace").and_then(|v| v.as_str()) {
                            if crate::symbols::is_unsymbolicated(bt) {
                                eprintln!(
                                    "clud: backtrace appears unsymbolicated; run `clud symbols verify` for diagnostic details"
                                );
                            }
                        }
                    }
                }
            }
        }
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let role = current_role();
            let _ = write_panic_report(&role, info);
            prev_hook(info);
        }));
    });
}

fn current_role() -> String {
    CURRENT_ROLE
        .get()
        .and_then(|lock| lock.read().ok().map(|g| g.clone()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Returns `~/.clud/state/crashes/`, creating the directory if missing.
///
/// Exposed `pub(crate)` so the `symbols` subcommand reuses the same
/// resolution logic (`CLUD_DAEMON_STATE_DIR` env override → home dir).
pub(crate) fn crashes_dir() -> std::io::Result<PathBuf> {
    let state_dir = crate::daemon::default_state_dir()?;
    let dir = state_dir.join("crashes");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn write_panic_report(
    role: &str,
    info: &std::panic::PanicHookInfo<'_>,
) -> std::io::Result<PathBuf> {
    let dir = crashes_dir()?;
    let pid = std::process::id();
    let ts = now_unix_ms();
    let path = dir.join(format!("{ts}-{role}-{pid}.json"));

    let panic_location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    let panic_message = panic_payload_to_string(info);
    let backtrace = Backtrace::force_capture().to_string();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let args = sanitize_args(std::env::args().collect());

    let report = CrashReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        target: BUILD_TARGET,
        build_id: crate::build_id::own_build_id(),
        role: role.to_string(),
        kind: "panic",
        pid,
        cwd,
        args,
        timestamp_unix_ms: ts,
        panic_location,
        panic_message: Some(panic_message),
        signal_or_exception: None,
        signal_number: None,
        exception_code: None,
        faulting_address: None,
        backtrace,
    };

    let json = serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string());
    fs::write(&path, json)?;
    let _ = prune_old_reports(&dir, MAX_REPORTS);
    Ok(path)
}

/// Per-platform labels decoded from a spool record's fault code/address.
#[derive(Default)]
pub(crate) struct NativeCrashMeta {
    pub signal_or_exception: Option<String>,
    pub signal_number: Option<i32>,
    pub exception_code: Option<i64>,
    pub faulting_address: Option<String>,
}

/// Spool directory kernal-api's native crash capture writes `.rpcrash`
/// records into: `<state>/crash-spool/`. Owner-private; kernal-api creates
/// and verifies it at install time.
pub(crate) fn crash_spool_dir() -> std::io::Result<PathBuf> {
    Ok(crate::daemon::default_state_dir()?.join("crash-spool"))
}

/// The live crash registration. Replaced (new first, then old dropped) when
/// the role changes, so the record a later crash writes carries the new role.
static NATIVE_GUARD: Mutex<Option<(String, kernal_api::crash::CrashGuard)>> = Mutex::new(None);

fn crash_metadata(role: &str) -> kernal_api::crash::CrashMetadata {
    kernal_api::crash::CrashMetadata {
        app_class: "cli".to_string(),
        app_name: "clud".to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        instance_name: role.to_string(),
        creation_time_ms: u64::try_from(now_unix_ms()).unwrap_or(u64::MAX),
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Arm native crash capture via `kernal_api::crash`.
///
/// Calls [`install`] first, so the role, reports dir and panic hook are
/// wired and any record a previous crashed run left in the spool has already
/// been turned into a JSON report. A repeat call with a new role re-registers
/// the metadata so a later crash is tagged with it; a repeat call with the
/// same role is a no-op.
///
/// **Does not attach a SIGINT / CTRL_C_EVENT handler.** kernal-api hooks
/// SIGSEGV / SIGBUS / SIGILL / SIGFPE / SIGABRT on Unix and structured
/// exceptions on Windows, so the `ctrlc` handler installed by
/// `startup::install_ctrl_c_flag` (#372 / `ctrl_c_track`) stays authoritative
/// for Ctrl-C / Ctrl-Break.
///
/// No clud code runs in the crash context: kernal-api writes one fixed-size
/// binary record, and [`collect_spooled_crashes`] converts it into the usual
/// JSON report on the next clud start (docs/DESIGN_DECISIONS.md, DD-239).
pub fn install_native(role: &str) {
    install(role);
    let mut slot = match NATIVE_GUARD.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    let first = NATIVE_INSTALLED.set(()).is_ok();
    match slot.as_ref() {
        Some((current, _)) if current == role => return,
        // A previous attach failed; do not retry on every retag.
        None if !first => return,
        _ => {}
    }
    let Ok(spool) = crash_spool_dir() else {
        return;
    };
    // kernal-api 0.1.41 takes the spool location only from this variable,
    // and reads it only while `install` creates the sink. Set it for the
    // call and restore it at once, so children clud spawns (soldr, other
    // kernal-api apps) never inherit clud's spool and file their crashes
    // as clud's.
    let env = kernal_api::crash::spool::SPOOL_DIR_ENV;
    let previous = std::env::var_os(env);
    std::env::set_var(env, &spool);
    let installed =
        kernal_api::crash::install(kernal_api::crash::CrashPolicy::On, crash_metadata(role));
    match previous {
        Some(value) => std::env::set_var(env, value),
        None => std::env::remove_var(env),
    }
    match installed {
        Ok(guard) => {
            // Drop the old registration only after the new one exists, so
            // there is never a window without an armed handler.
            let old = slot.replace((role.to_string(), guard));
            drop(old);
        }
        Err(err) => {
            if slot.is_none() {
                eprintln!("clud: failed to attach native crash handler: {err}");
            }
        }
    }
}

/// Turn every finished `.rpcrash` record in `spool` into a JSON report in
/// `crashes`, then delete the record. Returns the reports written.
///
/// A record is finished when it is a whole `RECORD_SIZE` bytes: kernal-api
/// pre-opens an empty sink per process and only the fatal path fills it, so
/// a short file belongs to a live (or cleanly exited, later reaped) process
/// and is left alone. The record is claimed by renaming it to `.draining`
/// first, so two clud processes starting at once never convert it twice.
/// Records that fail to parse are renamed `.invalid` instead of retried.
pub fn collect_spooled_crashes(spool: &Path, crashes: &Path) -> Vec<PathBuf> {
    use kernal_api::crash::spool::{parse, RECORD_SIZE};
    let mut written = Vec::new();
    let Ok(entries) = fs::read_dir(spool) else {
        return written;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rpcrash") {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() != RECORD_SIZE as u64 {
            continue;
        }
        let claimed = path.with_extension("rpcrash.draining");
        if fs::rename(&path, &claimed).is_err() {
            // Another process claimed it, or (Windows) the dying owner still
            // holds it open; the next start picks it up.
            continue;
        }
        match fs::read(&claimed).and_then(|bytes| parse(&bytes)) {
            Ok(raw) => match write_report(crashes, &report_from_record(&raw)) {
                Ok(out) => {
                    let _ = fs::remove_file(&claimed);
                    written.push(out);
                }
                // Keep the evidence; put it back for the next start.
                Err(_) => {
                    let _ = fs::rename(&claimed, &path);
                }
            },
            Err(_) => {
                let _ = fs::rename(&claimed, path.with_extension("rpcrash.invalid"));
            }
        }
    }
    if !written.is_empty() {
        let _ = prune_old_reports(crashes, MAX_REPORTS);
    }
    written
}

/// Map a parsed spool record onto clud's JSON report schema.
fn report_from_record(raw: &kernal_api::crash::spool::RawCrashReport) -> CrashReport {
    let meta = crate::platform::crash_report::decode_fault(raw.fault_code, raw.fault_address);
    let role = if raw.metadata.instance_name.is_empty() {
        "unknown".to_string()
    } else {
        raw.metadata.instance_name.clone()
    };
    let version = if raw.metadata.app_version.is_empty() {
        env!("CARGO_PKG_VERSION").to_string()
    } else {
        raw.metadata.app_version.clone()
    };
    CrashReport {
        version,
        target: BUILD_TARGET,
        build_id: crate::build_id::own_build_id(),
        role,
        kind: "native",
        pid: raw.pid,
        cwd: (!raw.metadata.cwd.is_empty()).then(|| raw.metadata.cwd.clone()),
        // The record carries no argv: nothing variable-length is copied in
        // the crash context.
        args: Vec::new(),
        timestamp_unix_ms: u128::from(raw.crash_unix_ms),
        panic_location: None,
        panic_message: None,
        signal_or_exception: meta.signal_or_exception,
        signal_number: meta.signal_number,
        exception_code: meta.exception_code,
        faulting_address: meta.faulting_address,
        backtrace: render_record_backtrace(raw),
    }
}

/// Render the record's pre-captured, unsymbolized frames as
/// `module+0xoffset`, faulting thread first. The offsets are ASLR-stable, so
/// they resolve offline against the matching sidecar.
fn render_record_backtrace(raw: &kernal_api::crash::spool::RawCrashReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut threads: Vec<_> = raw.threads.iter().collect();
    threads.sort_by_key(|t| t.os_tid != raw.tid);
    for thread in threads {
        let marker = if thread.os_tid == raw.tid {
            " (crashed)"
        } else {
            ""
        };
        let _ = writeln!(out, "thread {}{marker}:", thread.os_tid);
        for (i, frame) in thread.frames.iter().enumerate() {
            let module = frame
                .module_index
                .and_then(|idx| raw.modules.get(idx as usize))
                .map(|m| m.identity.as_str());
            let _ = match module {
                Some(module) => writeln!(out, "  {i:>2}: {module}+0x{:x}", frame.relative_address),
                None => writeln!(out, "  {i:>2}: 0x{:x}", frame.relative_address),
            };
        }
    }
    if out.is_empty() {
        out.push_str("<no pre-crash thread sample in record>\n");
    }
    if raw.truncated {
        out.push_str("<record truncated>\n");
    }
    out
}

fn write_report(dir: &Path, report: &CrashReport) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!(
        "{}-{}-{}.json",
        report.timestamp_unix_ms, report.role, report.pid
    ));
    let json = serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string());
    fs::write(&path, json)?;
    Ok(path)
}

pub fn hex_addr(addr: usize) -> Option<String> {
    if addr == 0 {
        None
    } else {
        Some(format!("0x{addr:x}"))
    }
}

pub fn signal_name(signo: i32) -> &'static str {
    // Linux signal numbers from <bits/signum.h>. Stable.
    match signo {
        4 => "SIGILL",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        11 => "SIGSEGV",
        _ => "UNKNOWN",
    }
}

pub fn mac_exception_kind_name(kind: u32) -> &'static str {
    // `mach/exception_types.h` — `EXC_*` constants.
    match kind {
        1 => "EXC_BAD_ACCESS",
        2 => "EXC_BAD_INSTRUCTION",
        3 => "EXC_ARITHMETIC",
        4 => "EXC_EMULATION",
        5 => "EXC_SOFTWARE",
        6 => "EXC_BREAKPOINT",
        7 => "EXC_SYSCALL",
        8 => "EXC_MACH_SYSCALL",
        9 => "EXC_RPC_ALERT",
        10 => "EXC_CRASH",
        11 => "EXC_RESOURCE",
        12 => "EXC_GUARD",
        13 => "EXC_CORPSE_NOTIFY",
        _ => "UNKNOWN",
    }
}

pub const EXCEPTION_ACCESS_VIOLATION: i64 = 0xC000_0005_u32 as i32 as i64;
pub const EXCEPTION_IN_PAGE_ERROR: i64 = 0xC000_0006_u32 as i32 as i64;
pub const EXCEPTION_ILLEGAL_INSTRUCTION: i64 = 0xC000_001D_u32 as i32 as i64;
pub const EXCEPTION_INT_DIVIDE_BY_ZERO: i64 = 0xC000_0094_u32 as i32 as i64;
pub const EXCEPTION_ARRAY_BOUNDS_EXCEEDED: i64 = 0xC000_008C_u32 as i32 as i64;
pub const EXCEPTION_BREAKPOINT: i64 = 0x8000_0003_u32 as i32 as i64;
pub const EXCEPTION_PRIV_INSTRUCTION: i64 = 0xC000_0096_u32 as i32 as i64;
pub const EXCEPTION_STACK_OVERFLOW: i64 = 0xC000_00FD_u32 as i32 as i64;

pub fn windows_exception_name(code: i64) -> &'static str {
    // Reference values from `winnt.h` / NTSTATUS. Match via constants
    // because pattern positions only accept constant expressions.
    if code == EXCEPTION_ACCESS_VIOLATION {
        "EXCEPTION_ACCESS_VIOLATION"
    } else if code == EXCEPTION_IN_PAGE_ERROR {
        "EXCEPTION_IN_PAGE_ERROR"
    } else if code == EXCEPTION_ILLEGAL_INSTRUCTION {
        "EXCEPTION_ILLEGAL_INSTRUCTION"
    } else if code == EXCEPTION_INT_DIVIDE_BY_ZERO {
        "EXCEPTION_INT_DIVIDE_BY_ZERO"
    } else if code == EXCEPTION_ARRAY_BOUNDS_EXCEEDED {
        "EXCEPTION_ARRAY_BOUNDS_EXCEEDED"
    } else if code == EXCEPTION_BREAKPOINT {
        "EXCEPTION_BREAKPOINT"
    } else if code == EXCEPTION_PRIV_INSTRUCTION {
        "EXCEPTION_PRIV_INSTRUCTION"
    } else if code == EXCEPTION_STACK_OVERFLOW {
        "EXCEPTION_STACK_OVERFLOW"
    } else {
        "UNKNOWN_EXCEPTION"
    }
}

fn panic_payload_to_string(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

pub(crate) fn sanitize_args(raw: Vec<String>) -> Vec<String> {
    raw.into_iter()
        .map(|s| {
            if looks_secret(&s) {
                "<redacted>".to_string()
            } else {
                crate::secret_redaction::redact_text(&s)
            }
        })
        .collect()
}

fn looks_secret(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    for needle in [
        "token=",
        "secret=",
        "password=",
        "passwd=",
        "api-key=",
        "apikey=",
        "auth=",
        "authorization=",
    ] {
        if lower.contains(needle) {
            return true;
        }
    }
    // Long runs of base64/url-safe chars look like a bearer token.
    if s.len() >= 40
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '+' || c == '='
        })
    {
        return true;
    }
    false
}

pub(crate) fn prune_old_reports(dir: &Path, keep: usize) -> std::io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".json") && e.path().is_file()
        })
        .collect();

    if entries.len() <= keep {
        return Ok(());
    }

    // Sort by mtime ascending so we drop the oldest first.
    entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    let to_remove = entries.len() - keep;
    for entry in entries.into_iter().take(to_remove) {
        let _ = fs::remove_file(entry.path());
    }
    Ok(())
}

/// Scan `crashes_dir` for the newest report newer than the recorded
/// `last_seen` watermark. On a hit, advance the watermark and return
/// `Some((unix_ms, path))` so the caller can surface a one-line notice on
/// stderr. Returns `None` when there's nothing new.
pub(crate) fn surface_previous_report(crashes_dir: &Path) -> Option<(u128, PathBuf)> {
    let last_seen_path = crashes_dir.join(LAST_SEEN_FILE);
    let last_seen: u128 = fs::read_to_string(&last_seen_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    let mut newest: Option<(u128, PathBuf)> = None;
    let entries = fs::read_dir(crashes_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        // Filename layout: <unix_ms>-<role>-<pid>.json
        let Some(ms_str) = name.split('-').next() else {
            continue;
        };
        let Ok(ms) = ms_str.parse::<u128>() else {
            continue;
        };
        if ms <= last_seen {
            continue;
        }
        match newest {
            Some((cur, _)) if cur >= ms => {}
            _ => newest = Some((ms, entry.path())),
        }
    }

    if let Some((ms, _)) = &newest {
        if let Ok(mut f) = fs::File::create(&last_seen_path) {
            let _ = writeln!(f, "{ms}");
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    #[test]
    fn sanitize_redacts_known_secret_keys_and_long_tokens() {
        let input = vec![
            "clud".to_string(),
            "--repo=foo/bar".to_string(),
            "GH_TOKEN=ghp_abcDEFghiJKLmnoPQRstuVWXyz0123456789".to_string(),
            "secret=hunter2".to_string(),
            "ghp_abcDEFghiJKLmnoPQRstuVWXyz0123456789abcd".to_string(),
            "--flag".to_string(),
            "short".to_string(),
            "sk-0123456789abcdef0123456789abcdef".to_string(),
        ];
        let out = sanitize_args(input);
        assert_eq!(out[0], "clud");
        assert_eq!(out[1], "--repo=foo/bar");
        assert_eq!(out[2], "<redacted>", "token= prefix should be redacted");
        assert_eq!(out[3], "<redacted>", "secret= prefix should be redacted");
        assert_eq!(out[4], "<redacted>", "long base64 run should be redacted");
        assert_eq!(out[5], "--flag");
        assert_eq!(out[6], "short", "short non-secret strings pass through");
        assert_eq!(
            out[7], "****cdef",
            "35-character provider keys must be masked"
        );
    }

    #[test]
    fn prune_keeps_only_n_reports() -> std::io::Result<()> {
        let dir = TemporaryDirectory::new()?;
        // Write 53 dummy reports. We don't care which 50 survive, only the
        // count — mtime-based ordering for "which to drop" is exercised
        // implicitly by `prune_old_reports`'s sort key.
        for i in 0..53 {
            fs::write(
                dir.path().join(format!(
                    "{}-test-{}.json",
                    1_700_000_000_000_u128 + i as u128,
                    i
                )),
                "{}",
            )?;
        }
        prune_old_reports(dir.path(), 50)?;
        let remaining: Vec<_> = fs::read_dir(dir.path())?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert_eq!(remaining.len(), 50, "should keep exactly 50 reports");
        Ok(())
    }

    #[test]
    fn prune_no_op_when_under_cap() -> std::io::Result<()> {
        let dir = TemporaryDirectory::new()?;
        for i in 0..3 {
            fs::write(dir.path().join(format!("{i}-test-{i}.json")), "{}")?;
        }
        prune_old_reports(dir.path(), 50)?;
        let remaining = fs::read_dir(dir.path())?.count();
        assert_eq!(remaining, 3);
        Ok(())
    }

    #[test]
    fn prune_ignores_non_json_files() -> std::io::Result<()> {
        let dir = TemporaryDirectory::new()?;
        fs::write(dir.path().join("last_seen"), "0")?;
        for i in 0..52 {
            fs::write(
                dir.path().join(format!(
                    "{}-test-{}.json",
                    1_700_000_000_000_u128 + i as u128,
                    i
                )),
                "{}",
            )?;
        }
        prune_old_reports(dir.path(), 50)?;
        // 50 .json + the last_seen sidecar = 51 entries.
        assert!(
            dir.path().join("last_seen").exists(),
            "last_seen survives prune"
        );
        let jsons = fs::read_dir(dir.path())?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .count();
        assert_eq!(jsons, 50);
        Ok(())
    }

    #[test]
    fn surface_returns_newest_and_advances_last_seen() -> std::io::Result<()> {
        let dir = TemporaryDirectory::new()?;
        fs::write(dir.path().join("100-test-1.json"), "{}")?;
        fs::write(dir.path().join("200-test-2.json"), "{}")?;
        fs::write(dir.path().join("150-test-3.json"), "{}")?;

        let hit = surface_previous_report(dir.path()).expect("expected a newest report");
        assert_eq!(hit.0, 200);
        assert!(hit.1.ends_with("200-test-2.json"));
        // last_seen file should now hold 200, so a re-scan returns None.
        let last_seen = fs::read_to_string(dir.path().join("last_seen"))?;
        assert_eq!(last_seen.trim(), "200");
        assert!(surface_previous_report(dir.path()).is_none());
        Ok(())
    }

    #[test]
    fn surface_returns_none_on_empty_dir() -> std::io::Result<()> {
        let dir = TemporaryDirectory::new()?;
        assert!(surface_previous_report(dir.path()).is_none());
        assert!(!dir.path().join("last_seen").exists());
        Ok(())
    }

    #[test]
    fn hex_addr_handles_null_and_nonzero() {
        assert_eq!(hex_addr(0), None);
        assert_eq!(hex_addr(0x1).as_deref(), Some("0x1"));
        assert_eq!(hex_addr(0xdead_beef).as_deref(), Some("0xdeadbeef"));
    }

    #[test]
    fn linux_signal_names_cover_documented_signals() {
        assert_eq!(signal_name(4), "SIGILL");
        assert_eq!(signal_name(6), "SIGABRT");
        assert_eq!(signal_name(7), "SIGBUS");
        assert_eq!(signal_name(8), "SIGFPE");
        assert_eq!(signal_name(11), "SIGSEGV");
        assert_eq!(signal_name(0), "UNKNOWN");
        assert_eq!(signal_name(99), "UNKNOWN");
    }

    #[test]
    fn macos_exception_kind_names_cover_common_exc_constants() {
        assert_eq!(mac_exception_kind_name(1), "EXC_BAD_ACCESS");
        assert_eq!(mac_exception_kind_name(2), "EXC_BAD_INSTRUCTION");
        assert_eq!(mac_exception_kind_name(3), "EXC_ARITHMETIC");
        assert_eq!(mac_exception_kind_name(6), "EXC_BREAKPOINT");
        assert_eq!(mac_exception_kind_name(10), "EXC_CRASH");
        assert_eq!(mac_exception_kind_name(0), "UNKNOWN");
        assert_eq!(mac_exception_kind_name(999), "UNKNOWN");
    }

    #[test]
    fn windows_exception_names_cover_common_codes() {
        // The bit pattern reinterpretation matters because NTSTATUS codes
        // start at 0xC000_0000 (i.e. negative when interpreted as i32).
        assert_eq!(
            windows_exception_name(0xC000_0005_u32 as i32 as i64),
            "EXCEPTION_ACCESS_VIOLATION"
        );
        assert_eq!(
            windows_exception_name(0xC000_001D_u32 as i32 as i64),
            "EXCEPTION_ILLEGAL_INSTRUCTION"
        );
        assert_eq!(
            windows_exception_name(0xC000_0094_u32 as i32 as i64),
            "EXCEPTION_INT_DIVIDE_BY_ZERO"
        );
        assert_eq!(
            windows_exception_name(0x8000_0003_u32 as i32 as i64),
            "EXCEPTION_BREAKPOINT"
        );
        assert_eq!(
            windows_exception_name(0xC000_00FD_u32 as i32 as i64),
            "EXCEPTION_STACK_OVERFLOW"
        );
        assert_eq!(windows_exception_name(0), "UNKNOWN_EXCEPTION");
        assert_eq!(windows_exception_name(0x1234), "UNKNOWN_EXCEPTION");
    }

    // NB: there is intentionally no in-process unit test for
    // `install_native()` itself. Calling it would attach a real signal /
    // SEH handler that persists for the lifetime of the test binary and
    // would intercept any subsequent test panic on Unix (since SIGABRT is
    // hooked). kernal-api's own tests prove the attach and the record
    // write; the tests below prove the record -> report conversion clud
    // owns. For manual reproduction:
    //
    //   - Linux/macOS: build clud, run a long-lived clud in the background,
    //     `kill -SEGV <pid>`, then start clud again and confirm a JSON file
    //     with `"kind":"native"` and `"signal_or_exception":"SIGSEGV"`
    //     appears in `~/.clud/state/crashes/` and `crash-spool/` is empty.
    //   - Windows: trigger an access violation from a debugger, restart
    //     clud, and confirm `"EXCEPTION_ACCESS_VIOLATION"` appears.

    fn sample_record(fault_code: i64) -> kernal_api::crash::spool::RawCrashReport {
        use kernal_api::crash::spool::{
            CrashFrame, CrashMetadata, CrashModule, CrashThread, RawCrashReport,
        };
        RawCrashReport {
            pid: 4242,
            tid: 7,
            fault_code,
            fault_address: 0xdead_beef,
            crash_unix_ms: 1_700_000_000_123,
            metadata: CrashMetadata {
                app_class: "cli".to_string(),
                app_name: "clud".to_string(),
                app_version: "9.9.9".to_string(),
                instance_name: "daemon".to_string(),
                creation_time_ms: 1,
                cwd: "/work/repo".to_string(),
            },
            modules: vec![CrashModule {
                identity: "/opt/clud/bin/clud".to_string(),
            }],
            threads: vec![
                CrashThread {
                    os_tid: 3,
                    frames: vec![CrashFrame {
                        module_index: None,
                        relative_address: 0x1000,
                    }],
                },
                CrashThread {
                    os_tid: 7,
                    frames: vec![CrashFrame {
                        module_index: Some(0),
                        relative_address: 0x1234,
                    }],
                },
            ],
            raw_context: Vec::new(),
            truncated: false,
        }
    }

    fn native_segv_code() -> i64 {
        crate::platform::crash_report::SEGV_FAULT.0
    }

    fn native_segv_label() -> &'static str {
        crate::platform::crash_report::SEGV_FAULT.1
    }

    #[test]
    fn a_spool_record_converts_into_the_native_report_schema() {
        let report = report_from_record(&sample_record(native_segv_code()));
        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["kind"], "native");
        assert_eq!(json["role"], "daemon");
        assert_eq!(json["pid"], 4242);
        assert_eq!(json["version"], "9.9.9");
        assert_eq!(json["target"], BUILD_TARGET);
        assert_eq!(json["cwd"], "/work/repo");
        assert_eq!(json["timestamp_unix_ms"], 1_700_000_000_123_u64);
        assert_eq!(json["signal_or_exception"], native_segv_label());
        assert_eq!(json["faulting_address"], "0xdeadbeef");
        assert!(json.get("panic_message").is_none());
        let bt = json["backtrace"].as_str().expect("backtrace string");
        assert!(
            bt.starts_with("thread 7 (crashed):\n"),
            "faulting thread first: {bt}"
        );
        assert!(bt.contains("/opt/clud/bin/clud+0x1234"), "{bt}");
        assert!(bt.contains("thread 3:\n"), "{bt}");
        assert!(bt.contains("0x1000"), "{bt}");
    }

    #[test]
    fn collect_converts_finished_records_and_deletes_them() -> std::io::Result<()> {
        let tmp = TempDir::new()?;
        let spool = tmp.path().join("crash-spool");
        let crashes = tmp.path().join("crashes");
        fs::create_dir_all(&spool)?;
        let record = spool.join("pending-4242-1-0000000000000001.rpcrash");
        fs::write(
            &record,
            kernal_api::crash::spool::encode(&sample_record(native_segv_code())),
        )?;
        // A live process's sink: short, must be left untouched.
        let live = spool.join("pending-1-1-0000000000000002.rpcrash");
        fs::write(&live, [0u8; 16])?;

        let written = collect_spooled_crashes(&spool, &crashes);
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(
            written[0].file_name().and_then(|n| n.to_str()),
            Some("1700000000123-daemon-4242.json")
        );
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&written[0])?).expect("report is valid JSON");
        assert_eq!(json["kind"], "native");
        assert_eq!(json["signal_or_exception"], native_segv_label());
        assert!(!record.exists(), "processed record must be deleted");
        assert!(live.exists(), "a short sink is not a crash record");

        // Idempotent: a second start finds nothing new.
        assert!(collect_spooled_crashes(&spool, &crashes).is_empty());
        // The surfacing path sees the converted report.
        assert!(surface_previous_report(&crashes).is_some());
        Ok(())
    }

    #[test]
    fn collect_quarantines_a_corrupt_record() -> std::io::Result<()> {
        let tmp = TempDir::new()?;
        let spool = tmp.path().join("crash-spool");
        let crashes = tmp.path().join("crashes");
        fs::create_dir_all(&spool)?;
        let record = spool.join("pending-9-1-0000000000000003.rpcrash");
        fs::write(&record, vec![0xffu8; kernal_api::crash::spool::RECORD_SIZE])?;

        assert!(collect_spooled_crashes(&spool, &crashes).is_empty());
        assert!(!record.exists());
        assert!(spool
            .join("pending-9-1-0000000000000003.rpcrash.invalid")
            .exists());
        Ok(())
    }

    /// Issue #1016: the triple has to actually be baked, not defaulted.
    ///
    /// `build.rs` falls back to "unknown" so a build-script hiccup cannot fail
    /// the build -- which means the failure mode is a binary that silently
    /// cannot identify itself, and a sidecar fetch that cannot pick an asset.
    /// Cargo always sets `TARGET` for build scripts, so "unknown" here means
    /// the stamping broke.
    #[test]
    fn the_build_target_is_stamped() {
        assert_ne!(
            BUILD_TARGET, "unknown",
            "build.rs did not stamp CLUD_TARGET; sidecar lookup cannot pick an asset"
        );
        assert!(
            !BUILD_TARGET.is_empty(),
            "an empty triple would build a nonsense asset name"
        );
        // A triple is at least arch-vendor-os; the loosest check that still
        // rejects a placeholder, without pinning this test to one platform.
        assert!(
            BUILD_TARGET.matches('-').count() >= 2,
            "{BUILD_TARGET} does not look like a target triple"
        );
    }

    /// It must match what this test binary was actually compiled for, so a
    /// stale or host-vs-target mix-up in `build.rs` is caught. Cross builds are
    /// exactly where a wrong triple would pick the wrong sidecar and
    /// mis-symbolicate, which is worse than not symbolicating at all.
    #[test]
    fn the_build_target_matches_this_compilation() {
        assert!(
            BUILD_TARGET.contains(std::env::consts::ARCH),
            "{BUILD_TARGET} does not name this build's arch {}",
            std::env::consts::ARCH
        );
        let os_marker = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        assert!(
            BUILD_TARGET.contains(os_marker),
            "{BUILD_TARGET} does not name this build's OS {os_marker}"
        );
    }

    /// A crash report carries the triple, because that is where the sidecar
    /// lookup reads it from. Both report kinds, since a native crash is the
    /// one most likely to need symbolicating.
    #[test]
    fn both_report_kinds_record_the_target() {
        for kind in ["panic", "native"] {
            let report = CrashReport {
                version: env!("CARGO_PKG_VERSION").to_string(),
                target: BUILD_TARGET,
                build_id: crate::build_id::own_build_id(),
                role: "test".to_string(),
                kind,
                pid: 1,
                cwd: None,
                args: Vec::new(),
                timestamp_unix_ms: 0,
                panic_location: None,
                panic_message: None,
                signal_or_exception: None,
                signal_number: None,
                exception_code: None,
                faulting_address: None,
                backtrace: String::new(),
            };
            let json = serde_json::to_value(&report).expect("serialize");
            assert_eq!(
                json["target"], BUILD_TARGET,
                "{kind} report must carry the triple"
            );
        }
    }

    /// #1016 item 3: the report must actually carry the build id.
    ///
    /// The field is `skip_serializing_if = "Option::is_none"`, so a reader
    /// that stopped resolving would silently drop it from every report rather
    /// than fail — the sidecar check would then have nothing to compare and
    /// would quietly accept any DWARF.
    #[test]
    fn a_report_carries_the_build_id_on_a_platform_that_has_one() {
        // Only Linux (ELF) builds carry a build id.
        if crate::platform::host::LINUX {
            let report = CrashReport {
                version: env!("CARGO_PKG_VERSION").to_string(),
                target: BUILD_TARGET,
                build_id: crate::build_id::own_build_id(),
                role: "test".to_string(),
                kind: "panic",
                pid: 1,
                cwd: None,
                args: Vec::new(),
                timestamp_unix_ms: 0,
                panic_location: None,
                panic_message: None,
                signal_or_exception: None,
                signal_number: None,
                exception_code: None,
                faulting_address: None,
                backtrace: String::new(),
            };
            let json = serde_json::to_value(&report).expect("serialize");
            let id = json["build_id"]
                .as_str()
                .expect("ELF builds carry a build-id; build.rs asks for one");
            assert_eq!(id.len(), 40, "sha1 build-id: {id}");
        }
    }
}

//! Issue #510 — daemon-side reclamation of stale Rust `target/` dirs.
//!
//! Opt-in: does nothing unless `CLUD_GC_TARGET_ROOTS` names one or more dev
//! roots (OS path-list separated — `;` on Windows, `:` on Unix). When set,
//! the periodic tick throttles to [`MIN_INTERVAL`] via a sentinel at
//! `~/.clud/state/target-sweep.last`, then removes `target/` dirs under those
//! roots whose mtime is older than the threshold.
//!
//! Config:
//! - `CLUD_GC_TARGET_ROOTS` — dev roots to sweep (required to enable).
//! - `CLUD_GC_TARGET_STALE_DAYS` — age gate in days (default
//!   [`crate::gc::target_sweep::DEFAULT_STALE_DAYS`], 3).
//!
//! Default-off because reclaiming `target/` forces a rebuild — more
//! disruptive than the disposable session-temp sweep. All errors non-fatal.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::gc::target_sweep;

pub const ROOTS_ENV: &str = "CLUD_GC_TARGET_ROOTS";
pub const STALE_DAYS_ENV: &str = "CLUD_GC_TARGET_STALE_DAYS";

/// Only worth walking dev roots once a day.
pub const MIN_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

const SENTINEL_FILE: &str = "target-sweep.last";

/// Production entry point — called from the daemon's periodic tick. No-op
/// unless [`ROOTS_ENV`] is configured.
pub fn maybe_sweep_targets() {
    let roots = configured_roots();
    if roots.is_empty() {
        return;
    }
    let Some(sentinel) = sentinel_path() else {
        return;
    };
    let threshold = configured_threshold();
    if let Err(e) = maybe_sweep_at(&sentinel, &roots, threshold, SystemTime::now()) {
        eprintln!("[clud] target sweep error: {e}");
    }
}

/// Testable variant. Skips when the sentinel is fresher than
/// `now - MIN_INTERVAL`; otherwise sweeps `roots` and rewrites the sentinel.
pub fn maybe_sweep_at(
    sentinel_path: &std::path::Path,
    roots: &[PathBuf],
    threshold: Duration,
    now: SystemTime,
) -> std::io::Result<Option<target_sweep::SweepReport>> {
    if let Some(last) = read_sentinel(sentinel_path) {
        match now.duration_since(last) {
            Ok(age) if age < MIN_INTERVAL => return Ok(None),
            // A sentinel dated in the future (clock was ahead) is invalid:
            // sweep now and rewrite it, or the sweep stays off until real
            // time catches up (#1910).
            _ => {}
        }
    }
    let report = target_sweep::sweep_roots_at(roots, now, threshold, false);
    write_sentinel(sentinel_path, now)?;
    log_report(&report);
    Ok(Some(report))
}

/// Force an immediate sweep of the configured roots, ignoring the sentinel
/// throttle. Used by the GC tick's background thread under disk pressure.
/// No-op when [`ROOTS_ENV`] is unset.
pub fn sweep_now() {
    let roots = configured_roots();
    if roots.is_empty() {
        return;
    }
    let now = SystemTime::now();
    let report = target_sweep::sweep_roots_at(&roots, now, configured_threshold(), false);
    if let Some(sentinel) = sentinel_path() {
        let _ = write_sentinel(&sentinel, now);
    }
    log_report(&report);
}

fn log_report(report: &target_sweep::SweepReport) {
    if report.targets_removed > 0 || report.skipped > 0 {
        eprintln!(
            "[clud] target sweep: removed {} target dir{} (~{} MiB), {} skipped",
            report.targets_removed,
            if report.targets_removed == 1 { "" } else { "s" },
            report.bytes_freed / (1024 * 1024),
            report.skipped,
        );
    }
}

/// Parse [`ROOTS_ENV`] into a de-duplicated list of existing directories.
pub fn configured_roots() -> Vec<PathBuf> {
    let Some(raw) = std::env::var_os(ROOTS_ENV) else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for path in std::env::split_paths(&raw) {
        if path.as_os_str().is_empty() || roots.contains(&path) {
            continue;
        }
        roots.push(path);
    }
    roots
}

fn configured_threshold() -> Duration {
    let days = std::env::var(STALE_DAYS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(target_sweep::DEFAULT_STALE_DAYS);
    Duration::from_secs(days * 24 * 60 * 60)
}

fn sentinel_path() -> Option<PathBuf> {
    Some(
        crate::home::user_home()?
            .join(".clud/state")
            .join(SENTINEL_FILE),
    )
}

fn read_sentinel(path: &std::path::Path) -> Option<SystemTime> {
    let raw = fs::read_to_string(path).ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

fn write_sentinel(path: &std::path::Path, now: SystemTime) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| std::io::Error::other("system clock before UNIX epoch"))?
        .as_secs();
    fs::write(path, secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;
    use std::fs::File;
    use std::io::Write;
    use std::time::Duration as StdDuration;

    fn make_crate_with_target(root: &std::path::Path, name: &str) -> PathBuf {
        let crate_dir = root.join(name);
        fs::create_dir_all(&crate_dir).unwrap();
        File::create(crate_dir.join("Cargo.toml")).unwrap();
        let target = crate_dir.join("target");
        fs::create_dir_all(&target).unwrap();
        File::create(target.join("a.o"))
            .unwrap()
            .write_all(b"data")
            .unwrap();
        target
    }

    #[test]
    fn min_interval_is_24h() {
        assert_eq!(MIN_INTERVAL, Duration::from_secs(24 * 60 * 60));
    }

    #[test]
    fn first_run_writes_sentinel_and_sweeps_stale() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let dev = tmp.path().join("dev");
        let target = make_crate_with_target(&dev, "c");
        let future_now = SystemTime::now() + StdDuration::from_secs(15 * 24 * 60 * 60);
        let report = maybe_sweep_at(
            &sentinel,
            &[dev],
            Duration::from_secs(3 * 24 * 60 * 60),
            future_now,
        )
        .unwrap()
        .expect("first run sweeps");
        assert_eq!(report.targets_removed, 1);
        assert!(!target.exists());
        assert!(sentinel.exists());
    }

    #[test]
    fn future_sentinel_is_repaired_instead_of_skipping() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let now = SystemTime::now();
        write_sentinel(&sentinel, now + Duration::from_secs(30 * 24 * 60 * 60)).unwrap();
        assert!(maybe_sweep_at(&sentinel, &[], Duration::from_secs(1), now)
            .unwrap()
            .is_some());
        assert!(read_sentinel(&sentinel).unwrap() <= now);
    }

    #[test]
    fn second_run_within_interval_skips() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let now = SystemTime::now();
        maybe_sweep_at(&sentinel, &[], Duration::from_secs(1), now).unwrap();
        let soon = now + Duration::from_secs(60 * 60);
        assert!(maybe_sweep_at(&sentinel, &[], Duration::from_secs(1), soon)
            .unwrap()
            .is_none());
    }
}

//! Issue #423 — daemon-side daily sweep of stale uv-cache envs.
//!
//! The clud daemon's existing periodic tick (`gc_service.rs`) calls
//! [`maybe_sweep_uv_cache`] every cadence. That helper reads a sentinel
//! timestamp at `~/.clud/state/uv-cache-sweep.last`; if 24h has elapsed
//! since the last successful sweep, it invokes
//! [`crate::gc::uv_cache::sweep_stale`] and updates the sentinel.
//!
//! The same pass then invalidates live wheel pointers whose archive was
//! gutted ([`uv_cache_repair`], #1711) before the size cap runs.
//!
//! All errors are non-fatal — a sweep miss never crashes the daemon. The
//! worst case is one extra `cargo` resolve when uv re-materializes a
//! recently-evicted env on the next `clud tool run`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::gc::{uv_cache, uv_cache_repair};

/// How often the sweep is allowed to run. 24h matches the issue spec.
pub const MIN_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Sentinel filename under the clud state dir.
const SENTINEL_FILE: &str = "uv-cache-sweep.last";

/// Production entry point — called from the daemon's periodic tick.
/// Cheap on the steady state (one file-stat + age comparison).
pub fn maybe_sweep_uv_cache() {
    let Some(sentinel) = sentinel_path() else {
        // No home dir → no state dir → nothing to do.
        return;
    };
    if let Err(e) = maybe_sweep_at(&sentinel, SystemTime::now()) {
        eprintln!("[clud] uv-cache sweep error: {e}");
    }
}

/// Testable variant of [`maybe_sweep_uv_cache`].
///
/// - Reads the sentinel at `sentinel_path` if it exists; if its content
///   is a unix timestamp newer than `now - MIN_INTERVAL`, returns Ok(None)
///   (sweep skipped — too soon).
/// - Otherwise calls [`uv_cache::sweep_stale`] with `now`, writes the
///   new sentinel, and returns Ok(Some(report)).
pub fn maybe_sweep_at(
    sentinel_path: &Path,
    now: SystemTime,
) -> std::io::Result<Option<uv_cache::SweepReport>> {
    maybe_sweep_at_root(sentinel_path, &crate::tools::clud_uv_cache_dir(), now)
}

fn maybe_sweep_at_root(
    sentinel_path: &Path,
    cache_root: &Path,
    now: SystemTime,
) -> std::io::Result<Option<uv_cache::SweepReport>> {
    if let Some(last) = read_sentinel(sentinel_path) {
        // duration_since Err = clock skew (sentinel in the future) →
        // treat as "we just ran" and skip; we'll recover on the next tick
        // once wall-clock catches up.
        if let Ok(age) = now.duration_since(last) {
            if age < MIN_INTERVAL {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
    let report = uv_cache::sweep_stale_at(cache_root, now, false)?;
    write_sentinel(sentinel_path, now)?;
    // #1711 (DD-148): invalidate live pointers to gutted archives, under
    // uv's own locks, so installs refetch instead of failing on every pin.
    // Under `cfg(test)` only temp roots: tests that drive the daemon tick
    // reach this with the real `~/.clud/cache/uv`.
    if cfg!(not(test)) || cache_root != crate::tools::clud_uv_cache_dir() {
        let repair = uv_cache_repair::repair_corrupt_wheels_at(cache_root, false);
        if repair.corrupt_found > 0 {
            eprintln!(
                "[clud] uv-cache repair: {} corrupt wheel entries, {} invalidated, {} busy-skipped",
                repair.corrupt_found, repair.repaired, repair.skipped,
            );
        }
    }
    // #1691 (DD-145): size cap. Only the production root; tests that pass a
    // temp root exercise `enforce_cap_at` directly with a fake uv. Compiled
    // out of unit tests so no test can ever reach the real cache or real uv.
    #[cfg(not(test))]
    if cache_root == crate::tools::clud_uv_cache_dir() {
        let max = crate::home::user_home()
            .map(|h| crate::clud_settings::peek_cache_max_bytes_at(&h))
            .unwrap_or(0);
        match uv_cache::enforce_cap(max) {
            uv_cache::CapOutcome::Cleaned { before_bytes } => eprintln!(
                "[clud] uv-cache over cache.max_bytes ({before_bytes} bytes): ran `uv cache clean`"
            ),
            uv_cache::CapOutcome::Failed(e) => eprintln!("[clud] uv-cache cap: {e}"),
            uv_cache::CapOutcome::Spared(_) => {}
        }
    }
    if report.stale_envs_removed > 0 || report.locked_envs_skipped > 0 {
        eprintln!(
            "[clud] uv-cache sweep: removed {} stale env{}, {} locked-skipped",
            report.stale_envs_removed,
            if report.stale_envs_removed == 1 {
                ""
            } else {
                "s"
            },
            report.locked_envs_skipped,
        );
    }
    Ok(Some(report))
}

fn sentinel_path() -> Option<PathBuf> {
    let home = crate::home::user_home()?;
    Some(home.join(".clud/state").join(SENTINEL_FILE))
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

    #[test]
    fn first_run_writes_sentinel_and_returns_report() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let now = SystemTime::now();
        let result = maybe_sweep_at_root(&sentinel, &tmp.path().join("cache"), now).unwrap();
        assert!(result.is_some(), "first run must execute the sweep");
        assert!(sentinel.exists(), "sentinel file must be written");
    }

    #[test]
    fn second_run_within_interval_is_skipped() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let now = SystemTime::now();
        maybe_sweep_at_root(&sentinel, &tmp.path().join("cache"), now).unwrap();
        // Second call 1h later — well under MIN_INTERVAL (24h).
        let one_hour_later = now + Duration::from_secs(3600);
        let result =
            maybe_sweep_at_root(&sentinel, &tmp.path().join("cache"), one_hour_later).unwrap();
        assert!(result.is_none(), "sweep must skip when sentinel is fresh");
    }

    #[test]
    fn run_after_interval_executes_again() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let now = SystemTime::now();
        maybe_sweep_at_root(&sentinel, &tmp.path().join("cache"), now).unwrap();
        // 25h later — past MIN_INTERVAL.
        let next_day = now + Duration::from_secs(25 * 60 * 60);
        let result = maybe_sweep_at_root(&sentinel, &tmp.path().join("cache"), next_day).unwrap();
        assert!(result.is_some(), "sweep must re-execute after MIN_INTERVAL");
    }

    #[test]
    fn future_sentinel_is_treated_as_fresh_not_panic() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        let now = SystemTime::now();
        let future = now + Duration::from_secs(60 * 60);
        // Write a sentinel in the future (clock-skew scenario).
        write_sentinel(&sentinel, future).unwrap();
        let result = maybe_sweep_at_root(&sentinel, &tmp.path().join("cache"), now).unwrap();
        assert!(
            result.is_none(),
            "future-timestamped sentinel must skip the sweep, not panic on Err from duration_since",
        );
    }

    #[test]
    fn malformed_sentinel_triggers_fresh_sweep() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);
        fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        fs::write(&sentinel, "not-a-number").unwrap();
        let result =
            maybe_sweep_at_root(&sentinel, &tmp.path().join("cache"), SystemTime::now()).unwrap();
        assert!(
            result.is_some(),
            "unparseable sentinel must fall through to sweep, not skip",
        );
    }

    #[test]
    fn min_interval_is_24_hours() {
        assert_eq!(MIN_INTERVAL, Duration::from_secs(24 * 60 * 60));
    }

    /// #1711: a `find -type f -atime +7 -delete` over the cache removed an
    /// archive's `.dist-info` but kept its pointer, so every install of that
    /// pin failed with "The wheel is invalid: Missing .dist-info directory"
    /// until the daily sweep invalidates the pointer and uv refetches.
    #[test]
    fn sweep_invalidates_a_pointer_whose_archive_lost_its_dist_info() {
        if !crate::platform::host::UNIX {
            return;
        }
        use crate::gc::uv_cache_fixture::{link_exists, wheel};
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("cache").join("uv");
        let bad = wheel(&root, "pydantic", "2.13.4", "UZfKIsCOzJxPSTiX");
        let good = wheel(&root, "idna", "3.18", "12FEHyBt4cSdD5YF");
        fs::remove_dir_all(&bad.dist_info).unwrap();
        let sentinel = tmp.path().join("state").join(SENTINEL_FILE);

        maybe_sweep_at_root(&sentinel, &root, SystemTime::now()).unwrap();

        assert!(
            !bad.pointer.exists() && !link_exists(&bad),
            "the pointer to the gutted archive must be invalidated so uv refetches"
        );
        assert!(
            bad.archive.join("pydantic").join("__init__.py").exists(),
            "archive contents are uv's to prune; clud never deletes them"
        );
        assert!(good.pointer.exists() && link_exists(&good) && good.dist_info.exists());
    }
}

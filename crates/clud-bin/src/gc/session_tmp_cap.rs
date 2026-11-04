//! Size cap for the session temp dir `~/.clud/tmp` (#2397).
//!
//! The 72 h age sweep ([`super::session_tmp`]) was the only reclaimer, so a day
//! of heavy sessions parked tens of gigabytes until it aged out (64 GB on the
//! box that reported this). `tmp.max_bytes` (default 10 GiB, `0` disables)
//! adds a size trigger: when the tree is over the cap, the oldest *eviction
//! units* go first until it fits. This supersedes the warn-only stance of
//! DD-141 for the over-cap case.
//!
//! # Units
//!
//! The harness lays scratch out as `claude-<uid>/<project>/<session>/`, so the
//! unit of eviction mirrors the age sweep's depth rule
//! ([`super::session_tmp::MAX_NESTED_DEPTH`]): a directory at the deepest
//! level is one unit (a session's scratch), anything shallower is recursed
//! into, and a file at any level is its own unit. A top-level directory is
//! therefore never removed wholesale while it still holds a live session.
//!
//! # Liveness
//!
//! mtime alone is not liveness: a session idle through a long CI wait or
//! overnight writes nothing yet expects its scratch. A unit is therefore
//! evicted only when *all* of these hold: its newest mtime anywhere beneath it
//! is at least [`MIN_IDLE`] old (an unreadable or oversized walk counts as
//! live), no running process has a cwd or scratch-env path inside it, and no
//! session id the daemon tracks matches a component of its path
//! ([`super::session_tmp_live`]; an unconsultable source counts everything as
//! live). Idleness is re-measured immediately before removal. The cost of
//! caution is only that the cap is exceeded until the unit is truly dead. A
//! live unit is skipped and the pass continues with the next oldest.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::delete_audit;
use super::session_tmp::MAX_NESTED_DEPTH;
use super::session_tmp_live::LiveSignals;

/// Default `tmp.max_bytes`: 10 GiB.
pub const DEFAULT_TMP_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// A unit with any write this recent anywhere beneath it is a live session.
pub const MIN_IDLE: Duration = Duration::from_secs(6 * 60 * 60);

/// Entries one measurement walk may visit before the unit is assumed live.
const WALK_BUDGET: usize = 2_000_000;

const SITE: &str = "gc.session-tmp-cap";

/// Outcome of one [`enforce_cap_at`] pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapReport {
    pub dry_run: bool,
    pub cap: u64,
    /// Measured size before eviction (apparent bytes).
    pub total_bytes: u64,
    /// Units removed (or, in a dry run, that would be).
    pub evicted: usize,
    pub reclaimed_bytes: u64,
    /// Units kept because they were, or might be, live.
    pub kept_live: usize,
    /// Idle units retained because their liveness could not be established.
    pub kept_unknown: usize,
    pub liveness_unavailable: Option<&'static str>,
    /// Units whose removal failed.
    pub failed: usize,
    /// Every path that could not be removed, for one summary line (#2397).
    pub undeletable: super::undeletable::Undeletable,
}

impl CapReport {
    /// The one line `clud gc prune` prints.
    pub fn summary_line(&self) -> String {
        if self.cap == 0 {
            return "tmp: size cap disabled (tmp.max_bytes = 0).".to_string();
        }
        if self.total_bytes <= self.cap {
            return format!(
                "tmp: {} within the {} cap.",
                super::cli::format_bytes(self.total_bytes),
                super::cli::format_bytes(self.cap)
            );
        }
        let verb = if self.dry_run {
            "--dry-run would evict"
        } else {
            "evicted"
        };
        let mut summary = format!(
            "tmp: {} over the {} cap; {verb} {} idle unit(s) ({}), kept {} live, {} unknown, {} failed.",
            super::cli::format_bytes(self.total_bytes),
            super::cli::format_bytes(self.cap),
            self.evicted,
            super::cli::format_bytes(self.reclaimed_bytes),
            self.kept_live,
            self.kept_unknown,
            self.failed
        );
        if let Some(reason) = self.liveness_unavailable {
            summary.push_str(&format!(" Eviction blocked: {reason}."));
        }
        summary
    }
}

#[derive(Debug)]
struct Unit {
    path: PathBuf,
    is_dir: bool,
    bytes: u64,
    /// Newest mtime in the unit; `None` when it could not be established.
    newest: Option<SystemTime>,
}

/// Apparent size and newest mtime of `path`'s subtree, without following
/// symlinks. `None` newest means "could not establish", i.e. treat as live.
fn measure(path: &Path, budget: &mut usize) -> (u64, Option<SystemTime>) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return (0, None);
    };
    let mut newest = meta.modified().ok();
    let mut bytes = if meta.is_dir() { 0 } else { meta.len() };
    if !meta.is_dir() {
        return (bytes, newest);
    }
    let Ok(entries) = fs::read_dir(path) else {
        return (bytes, None);
    };
    for entry in entries {
        if *budget == 0 {
            return (bytes, None);
        }
        *budget -= 1;
        let Ok(entry) = entry else {
            return (bytes, None);
        };
        let (child_bytes, child_newest) = measure(&entry.path(), budget);
        bytes = bytes.saturating_add(child_bytes);
        newest = match (newest, child_newest) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
    }
    (bytes, newest)
}

fn collect_units(dir: &Path, depth: usize, units: &mut Vec<Unit>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() && depth + 1 < MAX_NESTED_DEPTH {
            collect_units(&path, depth + 1, units);
            continue;
        }
        let mut budget = WALK_BUDGET;
        let (bytes, newest) = measure(&path, &mut budget);
        units.push(Unit {
            path,
            is_dir: meta.is_dir(),
            bytes,
            newest,
        });
    }
}

/// Is the unit idle by [`MIN_IDLE`]? Any doubt answers `false` (live).
fn is_idle(newest: Option<SystemTime>, now: SystemTime) -> bool {
    newest
        .and_then(|mtime| now.duration_since(mtime).ok())
        .is_some_and(|age| age >= MIN_IDLE)
}

/// Remove one unit, recording each undeletable path; `true` when it is gone.
fn remove_unit(
    root: &Path,
    unit: &Unit,
    undeletable: &mut super::undeletable::Undeletable,
) -> bool {
    let before = undeletable.count();
    if unit.is_dir {
        crate::rm_tool::remove_dir_tree_collecting(&unit.path, &mut |p, e| {
            undeletable.record(p, e);
        });
    } else if let Err(error) = crate::rm_tool::remove_link_or_file(&unit.path) {
        if error.kind() != std::io::ErrorKind::PermissionDenied
            || crate::platform::rm_tool::remove_owned_leaf_writable(root, &unit.path).is_err()
        {
            undeletable.record(&unit.path, &error);
        }
    }
    undeletable.count() == before
}

/// Evict the oldest idle units under `root` until it is within `cap` bytes.
/// `cap == 0` disables. Missing `root` is an empty, in-cap tree.
pub fn enforce_cap_at(
    root: &Path,
    cap: u64,
    now: SystemTime,
    dry_run: bool,
    live: impl FnOnce() -> LiveSignals,
) -> CapReport {
    let mut report = CapReport {
        dry_run,
        cap,
        ..Default::default()
    };
    if cap == 0 || !root.is_dir() {
        return report;
    }
    let mut units = Vec::new();
    collect_units(root, 0, &mut units);
    report.total_bytes = units.iter().fold(0u64, |t, u| t.saturating_add(u.bytes));
    if report.total_bytes <= cap {
        return report;
    }
    // Consulted only once the tree is known to be over the cap.
    let live = live();
    report.liveness_unavailable = live.unavailable_reason();
    // Oldest first; an unmeasurable unit sorts last (it is live by default).
    units.sort_by_key(|u| (u.newest.is_none(), u.newest));
    let mut remaining = report.total_bytes;
    for unit in &units {
        if remaining <= cap {
            break;
        }
        if !is_idle(unit.newest, now) {
            report.kept_live += 1;
            continue;
        }
        if report.liveness_unavailable.is_some() {
            report.kept_unknown += 1;
            continue;
        }
        if live.is_live(root, &unit.path) {
            report.kept_live += 1;
            continue;
        }
        // Re-measure right before acting: a session may have woken up.
        let mut budget = WALK_BUDGET;
        let (_, newest_now) = measure(&unit.path, &mut budget);
        if !is_idle(newest_now, now) {
            report.kept_live += 1;
            continue;
        }
        if !dry_run {
            delete_audit::record(SITE, &unit.path, "session-tmp over tmp.max_bytes");
            if !remove_unit(root, unit, &mut report.undeletable) {
                report.failed += 1;
                continue;
            }
        }
        report.evicted += 1;
        report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(unit.bytes);
        remaining = remaining.saturating_sub(unit.bytes);
    }
    report
}

/// Production entry: `~/.clud/tmp` against `tmp.max_bytes`.
pub fn enforce_cap(dry_run: bool) -> CapReport {
    let Some(root) = super::session_tmp::session_tmp_dir() else {
        return CapReport::default();
    };
    let cap = crate::clud_settings::load_tmp_max_bytes().unwrap_or(DEFAULT_TMP_MAX_BYTES);
    enforce_cap_at(&root, cap, SystemTime::now(), dry_run, LiveSignals::collect)
}

#[cfg(test)]
#[path = "session_tmp_cap_tests.rs"]
mod tests;

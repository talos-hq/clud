//! Issue #1672 — size cap on `~/.clud/trash` (`trash.max_bytes`).
//!
//! `safe-rm` keeps what it deletes for [`crate::rm_tool::TRASH_KEEP`] (72 h)
//! so it can be restored by hand. Retention was time-only, so a burst of
//! agents deleting `target/` trees parked 87 GB on a disk that was 94 % full.
//! Past the cap, the oldest entries go first. Everything in the trash is
//! something a user or agent already asked to delete, and nothing reads an
//! entry back except a human restoring by hand, so the only cost of eviction
//! is a shorter restore window. An entry younger than [`MIN_AGE`] is never
//! evicted, so a just-deleted path can still be recovered.
//!
//! The decision ([`plan_eviction`]) is a pure function over injected facts.
//! [`enforce_at`] gathers the facts, refuses anything that is not a real
//! directory directly inside the canonical trash root, records each removal
//! in the delete audit, and leaves the registry row for the GC tick's trash
//! reaper, which drops rows whose path is gone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Entries younger than this are never evicted for size.
pub const MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// One trash entry as seen by the planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashFact {
    pub path: PathBuf,
    /// When the entry was trashed (manifest mtime, else directory mtime).
    pub trashed_at: SystemTime,
    pub bytes: u64,
}

/// Oldest-first entries to evict so the total fits in `max_bytes`.
/// `max_bytes == 0` disables the cap. Entries younger than [`MIN_AGE`] (or
/// dated in the future) are never chosen, even if the cap stays exceeded.
pub fn plan_eviction(facts: &[TrashFact], max_bytes: u64, now: SystemTime) -> Vec<PathBuf> {
    if max_bytes == 0 {
        return Vec::new();
    }
    let mut total: u64 = facts.iter().map(|fact| fact.bytes).sum();
    if total <= max_bytes {
        return Vec::new();
    }
    let mut ordered: Vec<&TrashFact> = facts.iter().collect();
    ordered.sort_by(|a, b| a.trashed_at.cmp(&b.trashed_at).then(a.path.cmp(&b.path)));
    let mut evict = Vec::new();
    for fact in ordered {
        if total <= max_bytes {
            break;
        }
        let old_enough = matches!(now.duration_since(fact.trashed_at), Ok(age) if age >= MIN_AGE);
        if !old_enough {
            // Sorted oldest-first: everything after this is younger too.
            break;
        }
        total = total.saturating_sub(fact.bytes);
        evict.push(fact.path.clone());
    }
    evict
}

/// Outcome of one enforcement pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CapReport {
    pub total_bytes: u64,
    pub evicted: usize,
    pub evicted_bytes: u64,
    pub failed: Vec<(PathBuf, String)>,
}

/// Trash entries never change after they are written, so one walk per entry
/// per daemon lifetime is enough.
static SIZE_CACHE: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);

/// Production entry point — called from the maintenance sweep thread.
pub fn maybe_enforce() {
    // Unit tests never touch the real `~/.clud`.
    if cfg!(test) {
        return;
    }
    let Ok(trash_root) = crate::daemon::default_trash_dir() else {
        return;
    };
    let max_bytes = crate::clud_settings::load_trash_max_bytes()
        .unwrap_or(crate::rm_tool::TRASH_MAX_BYTES_DEFAULT);
    let report = enforce_at(&trash_root, max_bytes, SystemTime::now());
    if report.evicted > 0 {
        eprintln!(
            "[clud] trash: over trash.max_bytes ({} > {}); evicted {} oldest entr{} ({:.1} GB)",
            report.total_bytes,
            max_bytes,
            report.evicted,
            if report.evicted == 1 { "y" } else { "ies" },
            report.evicted_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        );
    }
    for (path, error) in &report.failed {
        eprintln!("[clud] trash: could not remove {}: {error}", path.display());
    }
}

/// Gather facts under `trash_root`, plan, and remove.
pub fn enforce_at(trash_root: &Path, max_bytes: u64, now: SystemTime) -> CapReport {
    let mut report = CapReport::default();
    if max_bytes == 0 {
        return report;
    }
    let Ok(canonical_root) = std::fs::canonicalize(trash_root) else {
        return report;
    };
    let Ok(entries) = std::fs::read_dir(&canonical_root) else {
        return report;
    };
    let mut facts = Vec::new();
    let mut cache = SIZE_CACHE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    let mut seen = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(trashed_at) = entry_fact_time(&path) else {
            continue;
        };
        let bytes = *cache
            .entry(path.clone())
            .or_insert_with(|| tree_bytes(&path));
        seen.push(path.clone());
        facts.push(TrashFact {
            path,
            trashed_at,
            bytes,
        });
    }
    cache.retain(|path, _| seen.contains(path));
    report.total_bytes = facts.iter().map(|fact| fact.bytes).sum();
    for path in plan_eviction(&facts, max_bytes, now) {
        if !strictly_inside(&canonical_root, &path) {
            continue;
        }
        let bytes = facts
            .iter()
            .find(|fact| fact.path == path)
            .map_or(0, |fact| fact.bytes);
        // Audit before acting (#893).
        crate::gc::delete_audit::record("gc.trash-cap", &path, "trash.max_bytes");
        crate::rm_tool::make_writable(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                cache.remove(&path);
                report.evicted += 1;
                report.evicted_bytes = report.evicted_bytes.saturating_add(bytes);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                cache.remove(&path);
            }
            Err(err) => report.failed.push((path, err.to_string())),
        }
    }
    report
}

/// Trashed-at time for a real (non-symlink) directory entry, or `None` to
/// leave it alone.
fn entry_fact_time(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return None;
    }
    std::fs::symlink_metadata(path.join(crate::rm_tool::TRASH_MANIFEST))
        .and_then(|manifest| manifest.modified())
        .or_else(|_| meta.modified())
        .ok()
}

/// A direct child of `root` that is still a real directory, not a symlink.
fn strictly_inside(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && std::fs::symlink_metadata(path)
            .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
}

/// Apparent size of a tree; symlinks are counted, never followed.
fn tree_bytes(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| !meta.is_dir())
        .map(|meta| meta.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(name: &str, age_secs: u64, bytes: u64, now: SystemTime) -> TrashFact {
        TrashFact {
            path: PathBuf::from(name),
            trashed_at: now - Duration::from_secs(age_secs),
            bytes,
        }
    }

    #[test]
    fn under_the_cap_nothing_is_evicted() {
        let now = SystemTime::now();
        let facts = [fact("a", 9_000, 10, now), fact("b", 8_000, 10, now)];
        assert!(plan_eviction(&facts, 20, now).is_empty());
    }

    #[test]
    fn zero_disables_the_cap() {
        let now = SystemTime::now();
        let facts = [fact("a", 9_000, 1_000, now)];
        assert!(plan_eviction(&facts, 0, now).is_empty());
    }

    #[test]
    fn oldest_go_first_until_the_total_fits() {
        let now = SystemTime::now();
        let facts = [
            fact("newer", 7_200, 30, now),
            fact("oldest", 90_000, 30, now),
            fact("middle", 50_000, 30, now),
        ];
        assert_eq!(
            plan_eviction(&facts, 40, now),
            vec![PathBuf::from("oldest"), PathBuf::from("middle")]
        );
    }

    #[test]
    fn entries_younger_than_min_age_are_spared_even_over_the_cap() {
        let now = SystemTime::now();
        let facts = [
            fact("old", 7_200, 10, now),
            fact("just-deleted", 60, 1_000, now),
        ];
        assert_eq!(plan_eviction(&facts, 100, now), vec![PathBuf::from("old")]);
        let future = TrashFact {
            path: PathBuf::from("future"),
            trashed_at: now + Duration::from_secs(3_600),
            bytes: 1_000,
        };
        assert!(plan_eviction(&[future], 1, now).is_empty());
    }

    fn entry(root: &Path, name: &str, bytes: usize, age: Duration) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target").join("blob"), vec![0u8; bytes]).unwrap();
        let manifest = dir.join(crate::rm_tool::TRASH_MANIFEST);
        std::fs::write(&manifest, b"{}").unwrap();
        let when = SystemTime::now() - age;
        filetime::set_file_mtime(&manifest, filetime::FileTime::from_system_time(when)).unwrap();
        dir
    }

    #[test]
    fn enforce_evicts_oldest_real_entries_and_never_follows_symlinks() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let trash = temp.path().join("trash");
        std::fs::create_dir_all(&trash).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("precious"), vec![0u8; 4_096]).unwrap();
        let old = entry(&trash, "old", 3_000, Duration::from_secs(48 * 3_600));
        let young = entry(&trash, "young", 3_000, Duration::from_secs(2 * 3_600));
        let fresh = entry(&trash, "fresh", 3_000, Duration::from_secs(60));
        if crate::platform::host::UNIX {
            crate::platform::gc_test_fs::symlink(&outside, trash.join("escape")).unwrap();
        }

        let report = enforce_at(&trash, 7_000, SystemTime::now());

        assert!(!old.exists(), "{report:?}");
        assert!(young.exists(), "evicted more than needed: {report:?}");
        assert!(fresh.exists());
        assert!(outside.join("precious").exists());
        assert_eq!(report.evicted, 1);
        assert!(report.failed.is_empty(), "{report:?}");
    }

    #[test]
    fn enforce_removes_read_only_trees() {
        if !crate::platform::host::UNIX {
            return;
        }
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let trash = temp.path().join("trash");
        std::fs::create_dir_all(&trash).unwrap();
        let old = entry(&trash, "sealed", 3_000, Duration::from_secs(48 * 3_600));
        crate::platform::gc_test_fs::set_mode(old.join("target"), 0o555).unwrap();

        let report = enforce_at(&trash, 1, SystemTime::now());

        assert!(!old.exists(), "{report:?}");
    }
}

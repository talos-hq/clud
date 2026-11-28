//! Issue #465: persisted handover registry for the orphan sweep.
//!
//! Records the originator PIDs of intentionally-detached sessions so the
//! periodic dead-originator sweep [`crate::orphan_reaper`] **spares** their
//! descendants. The case that needs it is a **daemon restart**: a detached
//! session's descendants are tagged with the daemon's PID as originator (see
//! the empirical originator model in #465), so once the daemon that launched
//! them exits, a *successor* daemon's sweep would see a dead originator and
//! reap the still-live detached session. Persisting the launching daemon's PID
//! here lets the successor spare it.
//!
//! **Purely subtractive on the reap path.** The sweep consults this only to
//! remove candidates from the kill set, never to add to it, so a stale or
//! over-broad entry can at worst leave an orphan un-reaped (the pre-#465 status
//! quo) — it can never cause a wrong kill. Growth is bounded by [`prune`],
//! which drops entries no live originator tag references any more.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const REGISTRY_FILE: &str = "handover-registry.json";

/// Serializes the read-modify-write in [`register`] and [`prune`], so a
/// concurrent prune cannot drop a PID that was just registered (#1912).
static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn registry_path(state_dir: &Path) -> PathBuf {
    state_dir.join(REGISTRY_FILE)
}

/// Load the protected originator-PID set. Missing or corrupt file → empty set
/// (fail-open: the sweep then simply spares nothing extra).
pub(super) fn load(state_dir: &Path) -> BTreeSet<u32> {
    match std::fs::read_to_string(registry_path(state_dir)) {
        Ok(text) => parse(&text),
        Err(_) => BTreeSet::new(),
    }
}

fn parse(text: &str) -> BTreeSet<u32> {
    serde_json::from_str::<Vec<u32>>(text)
        .map(|pids| pids.into_iter().collect())
        .unwrap_or_default()
}

fn serialize(pids: &BTreeSet<u32>) -> String {
    serde_json::to_string(&pids.iter().copied().collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".to_string())
}

/// Atomically persist the set (write-temp-then-rename so a crash mid-write can
/// never leave a half-written registry).
pub(super) fn save(state_dir: &Path, pids: &BTreeSet<u32>) -> std::io::Result<()> {
    let path = registry_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serialize(pids))?;
    // `rename` replaces an existing target atomically on every platform
    // (MoveFileExW with REPLACE_EXISTING on Windows). Deleting the target
    // first opened a window where `load` saw no registry at all (#1912).
    std::fs::rename(&tmp, &path)
}

/// Record `origin_pid` as protected (idempotent). Called when a detached
/// session is created, with the launching daemon's own PID.
pub(super) fn register(state_dir: &Path, origin_pid: u32) {
    let _guard = REGISTRY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut set = load(state_dir);
    if set.insert(origin_pid) {
        let _ = save(state_dir, &set);
    }
}

/// Drop protected entries no longer referenced by any live originator tag, so a
/// long-lived host doesn't accumulate a dead PID per detach forever.
/// `live_origins` is the set of originator PIDs currently present on
/// CLUD-tagged processes (the sweep already enumerates these).
pub(super) fn prune(state_dir: &Path, live_origins: &BTreeSet<u32>) {
    let _guard = REGISTRY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let set = load(state_dir);
    let kept: BTreeSet<u32> = set.intersection(live_origins).copied().collect();
    if kept.len() != set.len() {
        let _ = save(state_dir, &kept);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    #[test]
    fn round_trips_through_disk_and_survives_reload() {
        let tmp = TemporaryDirectory::new().unwrap();
        register(tmp.path(), 100);
        register(tmp.path(), 200);
        register(tmp.path(), 100); // idempotent
                                   // A fresh load (models a successor daemon reading the persisted file).
        assert_eq!(load(tmp.path()), BTreeSet::from([100, 200]));
    }

    #[test]
    fn concurrent_saves_never_expose_a_missing_registry() {
        let tmp = TemporaryDirectory::new().unwrap();
        let dir = tmp.path().to_path_buf();
        register(&dir, 100);
        let writer_dir = dir.clone();
        let writer = std::thread::spawn(move || {
            for pid in 1000..1500 {
                register(&writer_dir, pid);
                prune(&writer_dir, &BTreeSet::from([100]));
            }
        });
        while !writer.is_finished() {
            assert!(
                load(&dir).contains(&100),
                "a save exposed an empty registry"
            );
        }
        writer.join().unwrap();
        assert_eq!(load(&dir), BTreeSet::from([100]));
    }

    #[test]
    fn missing_and_corrupt_files_load_as_empty() {
        let tmp = TemporaryDirectory::new().unwrap();
        assert!(load(tmp.path()).is_empty(), "missing file → empty");
        std::fs::write(registry_path(tmp.path()), "{ not json").unwrap();
        assert!(
            load(tmp.path()).is_empty(),
            "corrupt file → empty (fail-open)"
        );
    }

    #[test]
    fn parse_accepts_a_pid_array() {
        assert_eq!(parse("[3,1,2]"), BTreeSet::from([1, 2, 3]));
        assert!(parse("42").is_empty(), "non-array → empty");
    }

    #[test]
    fn prune_drops_entries_with_no_live_originator() {
        let tmp = TemporaryDirectory::new().unwrap();
        register(tmp.path(), 100);
        register(tmp.path(), 200);
        register(tmp.path(), 300);
        // Only 200 still has live descendants tagged with it.
        prune(tmp.path(), &BTreeSet::from([200, 999]));
        assert_eq!(load(tmp.path()), BTreeSet::from([200]));
    }

    #[test]
    fn prune_is_a_noop_when_all_entries_are_live() {
        let tmp = TemporaryDirectory::new().unwrap();
        register(tmp.path(), 100);
        register(tmp.path(), 200);
        prune(tmp.path(), &BTreeSet::from([100, 200, 300]));
        assert_eq!(load(tmp.path()), BTreeSet::from([100, 200]));
    }
}

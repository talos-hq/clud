//! Issue #2487: stray `*.log` files at the top of `~/.clud/worktrees`.
//!
//! Agents redirect lint and test output (`lint.log`, `red1358.log`, ...) into
//! the worktrees root, where nothing ever removes it. Each GC tick moves every
//! top-level `*.log` file that has been idle for [`MIN_IDLE_SECS`] into
//! `~/.clud/logs/worktrees`, then caps that directory: files older than
//! [`MAX_AGE_SECS`] go, and the oldest go while it holds more than
//! [`MAX_BYTES`]. Directories (the worktrees themselves) and other file kinds
//! are never touched, and nothing below the top level is examined.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A log written to within this window may still be open; leave it.
pub const MIN_IDLE_SECS: u64 = 60 * 60;

/// Relocated logs older than this are deleted.
pub const MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// Size cap for the logs directory; the oldest files go first.
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// What one [`tidy`] call did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TidyReport {
    pub moved: usize,
    pub deleted: usize,
    pub deleted_bytes: u64,
}

/// `<home>/.clud/logs/worktrees`, given the worktrees root.
pub fn logs_dir_for(wt_root: &Path) -> PathBuf {
    wt_root
        .parent()
        .unwrap_or(wt_root)
        .join("logs")
        .join("worktrees")
}

fn age_secs(meta: &fs::Metadata, now: SystemTime) -> Option<u64> {
    let mtime = meta.modified().ok()?;
    Some(
        now.duration_since(mtime)
            .unwrap_or(Duration::ZERO)
            .as_secs(),
    )
}

/// A free name in `dir` for `name` (`name`, then `name.1`, `name.2`, ...).
fn free_name(dir: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
    let first = dir.join(name);
    if !first.exists() {
        return Some(first);
    }
    (1..1_000u32)
        .map(|n| {
            let mut candidate = name.to_os_string();
            candidate.push(format!(".{n}"));
            dir.join(candidate)
        })
        .find(|p| !p.exists())
}

/// Move idle top-level `*.log` files out of `wt_root`, then cap `logs_dir`.
pub fn tidy(wt_root: &Path, logs_dir: &Path, now: SystemTime) -> TidyReport {
    let mut report = TidyReport {
        moved: relocate(wt_root, logs_dir, now),
        ..TidyReport::default()
    };
    cap(logs_dir, now, MAX_BYTES, &mut report);
    report
}

fn relocate(wt_root: &Path, logs_dir: &Path, now: SystemTime) -> usize {
    let Ok(entries) = fs::read_dir(wt_root) else {
        return 0;
    };
    let mut moved = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "log") {
            continue;
        }
        // symlink_metadata: a symlink or directory named `*.log` stays put.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || age_secs(&meta, now).is_none_or(|age| age < MIN_IDLE_SECS) {
            continue;
        }
        if fs::create_dir_all(logs_dir).is_err() {
            return moved;
        }
        let Some(dest) = free_name(logs_dir, &entry.file_name()) else {
            continue;
        };
        // Same filesystem (both under ~/.clud), so rename is atomic; a
        // cross-device failure leaves the file where it was.
        if fs::rename(&path, &dest).is_ok() {
            moved += 1;
        }
    }
    moved
}

fn cap(logs_dir: &Path, now: SystemTime, max_bytes: u64, report: &mut TidyReport) {
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return;
    };
    let mut files: Vec<(u64, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = fs::symlink_metadata(e.path()).ok()?;
            meta.is_file()
                .then(|| (age_secs(&meta, now).unwrap_or(0), meta.len(), e.path()))
        })
        .collect();
    // Oldest first.
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    for (age, len, path) in files {
        if age <= MAX_AGE_SECS && total <= max_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            report.deleted += 1;
            report.deleted_bytes += len;
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_aged(path: &Path, bytes: usize, age: u64, now: SystemTime) {
        fs::write(path, vec![b'x'; bytes]).unwrap();
        let at = now - Duration::from_secs(age);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(at)).unwrap();
    }

    #[test]
    fn idle_root_logs_move_and_everything_else_stays() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".clud").join("worktrees");
        let worktree = root.join("repo-wt-a");
        fs::create_dir_all(&worktree).unwrap();
        let now = SystemTime::now();
        write_aged(&root.join("lint.log"), 10, 2 * 3_600, now);
        write_aged(&root.join("red1358.log"), 10, 2 * 3_600, now);
        write_aged(&root.join("live.log"), 10, 60, now);
        write_aged(&root.join("notes.txt"), 10, 2 * 3_600, now);
        write_aged(&worktree.join("inner.log"), 10, 2 * 3_600, now);
        fs::create_dir(root.join("dir.log")).unwrap();
        let logs = logs_dir_for(&root);
        assert_eq!(logs, home.path().join(".clud/logs/worktrees"));

        let report = tidy(&root, &logs, now);
        assert_eq!(report.moved, 2);
        assert!(logs.join("lint.log").exists());
        assert!(logs.join("red1358.log").exists());
        assert!(!root.join("lint.log").exists());
        assert!(
            root.join("live.log").exists(),
            "a log still being written stays"
        );
        assert!(root.join("notes.txt").exists());
        assert!(
            worktree.join("inner.log").exists(),
            "worktree contents untouched"
        );
        assert!(root.join("dir.log").is_dir());
    }

    #[test]
    fn a_name_collision_keeps_both_files() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("worktrees");
        let logs = home.path().join("logs");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&logs).unwrap();
        let now = SystemTime::now();
        write_aged(&logs.join("lint.log"), 1, 3_600, now);
        write_aged(&root.join("lint.log"), 2, 2 * 3_600, now);
        assert_eq!(tidy(&root, &logs, now).moved, 1);
        assert!(logs.join("lint.log").exists());
        assert!(logs.join("lint.log.1").exists());
    }

    #[test]
    fn the_logs_dir_is_capped_by_age_and_size_oldest_first() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("worktrees");
        let logs = home.path().join("logs");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&logs).unwrap();
        let now = SystemTime::now();
        write_aged(&logs.join("expired.log"), 1, MAX_AGE_SECS + 60, now);
        write_aged(&logs.join("fresh.log"), 1, 60, now);
        let report = tidy(&root, &logs, now);
        assert_eq!(report.deleted, 1);
        assert!(!logs.join("expired.log").exists());
        assert!(logs.join("fresh.log").exists());

        // Over the byte cap: the oldest goes, the newest stays.
        let mut cap_report = TidyReport::default();
        write_aged(&logs.join("a-old.log"), 60, 3 * 3_600, now);
        write_aged(&logs.join("b-new.log"), 60, 2 * 3_600, now);
        cap(&logs, now, 100, &mut cap_report);
        assert_eq!(cap_report.deleted, 1);
        assert!(!logs.join("a-old.log").exists());
        assert!(logs.join("b-new.log").exists());
    }
}

//! Low-space mode: when Claude Code runs from clud's temp-home install
//! fallback (see `claude_temp_home_install`), the home volume is the small
//! one that made the install fail. Log directories are moved into the
//! RAM-backed temp home so they cannot fill it. A background thread sleeps
//! on file-system events for those logs and, only when a write pushes them
//! past [`LOG_CAP_BYTES`], deletes the oldest files so they cannot eat the
//! RAM either. No writes means no work.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Total log size kept in temp space; a purge trims to half of it.
pub const LOG_CAP_BYTES: u64 = 64 * 1024 * 1024;
/// After a write event, the size is measured at most this often.
pub const MIN_CHECK_GAP: Duration = Duration::from_secs(30);

/// Log directories under the real home, each with its name inside the
/// temp logs root: clud's state logs and verbose logs, Claude Code's debug logs.
pub fn log_dirs(real_home: &Path) -> Vec<(PathBuf, &'static str)> {
    let state = real_home.join(".clud").join("state");
    vec![
        (state.join("logs"), "clud-logs"),
        (state.join("verbose"), "clud-verbose"),
        (real_home.join(".claude").join("debug"), "claude-debug"),
    ]
}

/// Point `from` at `to` with a symlink. Only replaces a missing path, an
/// empty directory, or a dangling link (a previous boot's temp home); a
/// directory with content is left alone. Returns whether it changed `from`.
pub fn redirect(from: &Path, to: &Path) -> Result<bool, String> {
    std::fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    match from.symlink_metadata() {
        Err(_) => {}
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(from).is_ok_and(|target| target == to) {
                return Ok(false);
            }
            if from.exists() {
                return Err(format!("{} already links elsewhere", from.display()));
            }
            std::fs::remove_file(from)
                .map_err(|e| format!("cannot replace {}: {e}", from.display()))?;
        }
        Ok(meta) if meta.is_dir() => {
            let empty = std::fs::read_dir(from)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if !empty {
                return Err(format!("{} already holds logs", from.display()));
            }
            std::fs::remove_dir(from)
                .map_err(|e| format!("cannot replace {}: {e}", from.display()))?;
        }
        Ok(_) => return Err(format!("{} is not a directory", from.display())),
    }
    if let Some(parent) = from.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    crate::platform::alias_link::symlink(to, from)
        .map_err(|e| format!("cannot link {}: {e}", from.display()))?;
    Ok(true)
}

fn collect_files(dir: &Path, out: &mut Vec<(SystemTime, u64, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_files(&entry.path(), out);
        } else if meta.is_file() {
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((modified, meta.len(), entry.path()));
        }
    }
}

/// When the files under `root` exceed `cap` bytes, delete the oldest until
/// they fit in half of it. Returns the bytes removed.
pub fn purge(root: &Path, cap: u64) -> u64 {
    let mut files = Vec::new();
    collect_files(root, &mut files);
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total <= cap {
        return 0;
    }
    files.sort();
    let target = cap / 2;
    let mut removed = 0;
    for (_, len, path) in files {
        if total <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            removed += len;
        }
    }
    removed
}

/// Yellow notice that low-space mode is on.
pub fn low_space_warning(temp_home: &Path, logs_root: &Path) -> String {
    format!(
        "\x1b[33m[clud] WARNING: low-space mode: Claude Code runs from temporary storage at {}; \
         logs go to {} and the oldest are purged above {} MiB\x1b[0m",
        temp_home.display(),
        logs_root.display(),
        LOG_CAP_BYTES / (1024 * 1024)
    )
}

/// Turn on low-space mode when `claude_binary` lives in a temp install
/// home: redirect the log directories and start the background purge.
pub fn activate_if_temp_install(claude_binary: &Path) {
    let Some(temp_home) = crate::claude_temp_home_install::temp_home_of(claude_binary) else {
        return;
    };
    let Some(real_home) = crate::home::user_home() else {
        return;
    };
    let logs_root = temp_home.join("logs");
    eprintln!("{}", low_space_warning(&temp_home, &logs_root));
    for (from, name) in log_dirs(&real_home) {
        if let Err(err) = redirect(&from, &logs_root.join(name)) {
            eprintln!(
                "\x1b[33m[clud] WARNING: low-space mode left {name} logs in place: {err}\x1b[0m"
            );
        }
    }
    purge(&logs_root, LOG_CAP_BYTES);
    let _ = std::thread::Builder::new()
        .name("clud-temp-log-purge".into())
        .spawn(move || purge_on_writes(&logs_root));
}

/// Block on write events under `logs_root`; purge only when they push the
/// logs over the cap. Ends quietly if the watcher cannot start.
fn purge_on_writes(logs_root: &Path) {
    use notify::Watcher;
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut watcher) = notify::recommended_watcher(tx) else {
        return;
    };
    if watcher
        .watch(logs_root, notify::RecursiveMode::Recursive)
        .is_err()
    {
        return;
    }
    let mut last_check: Option<std::time::Instant> = None;
    for event in rx {
        let Ok(event) = event else { continue };
        if !matches!(
            event.kind,
            notify::EventKind::Create(_) | notify::EventKind::Modify(_)
        ) {
            continue;
        }
        if last_check.is_some_and(|at| at.elapsed() < MIN_CHECK_GAP) {
            continue;
        }
        last_check = Some(std::time::Instant::now());
        purge(logs_root, LOG_CAP_BYTES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, len: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; len]).unwrap();
    }

    #[test]
    fn purge_keeps_logs_under_the_cap() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("a/one.log"), 40);
        assert_eq!(purge(root.path(), 100), 0);
        assert!(root.path().join("a/one.log").exists());
    }

    #[test]
    fn purge_deletes_the_oldest_files_down_to_half_the_cap() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("a/old.log");
        let mid = root.path().join("b/mid.log");
        let new = root.path().join("a/new.log");
        for path in [&old, &mid, &new] {
            write(path, 40);
            std::thread::sleep(Duration::from_millis(20));
        }
        // 120 bytes > cap 100: drop oldest until <= 50.
        assert_eq!(purge(root.path(), 100), 80);
        assert!(!old.exists() && !mid.exists());
        assert!(new.exists());
    }

    #[test]
    fn redirect_links_a_missing_dir_and_is_idempotent() {
        if crate::platform::host::WINDOWS {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let from = home.path().join(".claude/debug");
        let to = temp.path().join("claude-debug");
        assert!(redirect(&from, &to).unwrap());
        assert_eq!(std::fs::read_link(&from).unwrap(), to);
        assert!(!redirect(&from, &to).unwrap());
        // Writes through the link land in temp space.
        std::fs::write(from.join("x.txt"), b"log").unwrap();
        assert!(to.join("x.txt").is_file());
    }

    #[test]
    fn redirect_replaces_an_empty_dir_and_a_dangling_link() {
        if crate::platform::host::WINDOWS {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let empty = home.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(redirect(&empty, &temp.path().join("e")).unwrap());
        let dangling = home.path().join("dangling");
        crate::platform::alias_link::symlink(Path::new("/nonexistent/old-boot"), &dangling)
            .unwrap();
        assert!(redirect(&dangling, &temp.path().join("d")).unwrap());
        assert_eq!(
            std::fs::read_link(&dangling).unwrap(),
            temp.path().join("d")
        );
    }

    #[test]
    fn redirect_leaves_existing_logs_in_place() {
        let home = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let from = home.path().join("logs");
        write(&from.join("keep.log"), 3);
        assert!(redirect(&from, &temp.path().join("l")).is_err());
        assert!(from.join("keep.log").is_file());
    }

    #[test]
    fn low_space_warning_is_yellow() {
        let w = low_space_warning(Path::new("/tmp/h"), Path::new("/tmp/h/logs"));
        assert!(w.starts_with("\x1b[33m") && w.ends_with("\x1b[0m"));
        assert!(w.contains("/tmp/h/logs") && w.contains("64 MiB"));
    }
}

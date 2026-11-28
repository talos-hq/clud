//! uv-cache integrity repair (#1711, DD-148).
//!
//! uv keeps each cached wheel as a pointer plus an unpacked archive:
//!
//! ```text
//! wheels-v6/<index>/<pkg>/<key>       -> ../../../archive-v0/<id>   (symlink)
//! wheels-v6/<index>/<pkg>/<key>.http  the pointer uv's wheel index reads
//!                                     (`<key>.rev` for a local wheel)
//! wheels-v6/<index>/<pkg>/<key>.lock  uv's per-entry advisory lock
//! archive-v0/<id>/                    the unpacked wheel
//! ```
//!
//! uv trusts any pointer whose archive directory exists. A file-level
//! deletion inside the cache can therefore leave a live pointer to an archive
//! without its `.dist-info` ("The wheel is invalid: Missing .dist-info
//! directory" on every install of that pin), without `RECORD`, or missing
//! package files (the install succeeds and imports fail). uv's own fix,
//! `uv cache clean <pkg>`, needs the cache's exclusive lock, and every live
//! `uv run` holds the shared one, so on a busy host it times out.
//!
//! [`repair_corrupt_wheels_at`] finds those pointers and invalidates them
//! under uv's own writer protocol: a shared lock on `<root>/.lock`, then the
//! entry's exclusive `<key>.lock`, which is the lock uv holds while it
//! replaces that pointer. Only the pointer files are removed. uv then sees a
//! cache miss and re-downloads into a fresh archive. The broken archive is
//! left as a dangling entry for `uv cache prune`, and clud never deletes
//! archive contents (they may be hard-linked into live venvs).
//!
//! The pass is Unix-only in effect. On Windows uv writes `<key>` as a plain
//! link file (`archive-v0/<id>`) rather than a symlink, and keys the entry
//! lock by the wheel stem, so the scan finds nothing and nothing is unlinked.

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// The audit `rule` prefix for an invalidated pointer.
pub const REPAIR_RULE: &str = "uv-cache wheel archive corrupt";

/// Pointer-file extensions uv writes next to a wheel's `<key>` symlink.
const POINTER_EXTS: [&str; 2] = ["http", "rev"];

/// Outcome of [`repair_corrupt_wheels_at`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepairReport {
    /// Live pointers whose archive is defective.
    pub corrupt_found: usize,
    /// Pointers invalidated (in `dry_run`, pointers that would be).
    pub repaired: usize,
    /// Corrupt pointers left alone this pass: uv held the cache or the
    /// entry lock, the pointer moved, or the platform cannot take uv's lock.
    pub skipped: usize,
    pub dry_run: bool,
}

/// One live wheel pointer that resolves into `archive-v0/`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WheelPointer {
    /// The `<key>` symlink.
    link: PathBuf,
    /// The `<key>.http` / `<key>.rev` files that exist.
    pointer_files: Vec<PathBuf>,
    /// Canonical `archive-v0/<id>` the link resolves to.
    archive: PathBuf,
}

/// Production entry: repair clud's own uv cache (`~/.clud/cache/uv`).
pub fn repair_corrupt_wheels(dry_run: bool) -> RepairReport {
    repair_corrupt_wheels_at(&crate::tools::clud_uv_cache_dir(), dry_run)
}

/// Find every live wheel pointer under `root` whose archive is defective and
/// invalidate it. Never fails: an unreadable tree is skipped, not reported.
pub fn repair_corrupt_wheels_at(root: &Path, dry_run: bool) -> RepairReport {
    let mut report = RepairReport {
        dry_run,
        ..Default::default()
    };
    let corrupt: Vec<(WheelPointer, String)> = wheel_pointers(root)
        .into_iter()
        .filter_map(|pointer| archive_defect(&pointer.archive).map(|why| (pointer, why)))
        .collect();
    report.corrupt_found = corrupt.len();
    if corrupt.is_empty() {
        return report;
    }
    if dry_run {
        report.repaired = corrupt.len();
        return report;
    }
    // Act as one more uv process: hold the cache-wide lock shared, so a
    // running `uv cache clean`/`prune` (exclusive) is never raced.
    let Some(_cache_lock) = lock_cache_shared(root) else {
        report.skipped = corrupt.len();
        return report;
    };
    for (pointer, why) in corrupt {
        if invalidate(root, &pointer, &why) {
            report.repaired += 1;
        } else {
            report.skipped += 1;
        }
    }
    report
}

/// Every `<key>` symlink under a `wheels-v*` bucket that has a `.http` or
/// `.rev` sibling and resolves to an existing directory in `archive-v0/`.
/// A pointer to a missing archive is skipped: uv ignores it and refetches.
fn wheel_pointers(root: &Path) -> Vec<WheelPointer> {
    let Ok(archive_dir) = fs::canonicalize(root.join("archive-v0")) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut stack: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("wheels-v"))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    let mut found = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_symlink() {
                let pointer_files: Vec<PathBuf> = POINTER_EXTS
                    .iter()
                    .map(|ext| sibling(&path, ext))
                    .filter(|p| p.is_file())
                    .collect();
                if pointer_files.is_empty() {
                    continue;
                }
                if let Some(archive) = resolve_archive(&path, &archive_dir) {
                    found.push(WheelPointer {
                        link: path,
                        pointer_files,
                        archive,
                    });
                }
            }
        }
    }
    found
}

/// `<dir>/<name>.<ext>` for a `<dir>/<name>` pointer link.
fn sibling(link: &Path, ext: &str) -> PathBuf {
    let mut name = link.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(ext);
    link.with_file_name(name)
}

/// The canonical archive a pointer link resolves to, only when it is an
/// existing directory directly inside `archive_dir`.
fn resolve_archive(link: &Path, archive_dir: &Path) -> Option<PathBuf> {
    let target = fs::read_link(link).ok()?;
    let target = if target.is_absolute() {
        target
    } else {
        link.parent()?.join(target)
    };
    let archive = fs::canonicalize(target).ok()?;
    (archive.is_dir() && archive.parent() == Some(archive_dir)).then_some(archive)
}

/// Why an unpacked wheel cannot be installed, or `None` when it is intact
/// or cannot be judged (unreadable, or a shape uv rejects on its own).
fn archive_defect(archive: &Path) -> Option<String> {
    let entries = fs::read_dir(archive).ok()?;
    let dist_infos: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".dist-info"))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    let dist_info = match dist_infos.as_slice() {
        [] => return Some("missing .dist-info directory".to_string()),
        [one] => one,
        // Several `.dist-info` dirs is uv's own error, not cache damage.
        _ => return None,
    };
    let name = dist_info.file_name()?.to_string_lossy().into_owned();
    let record = match fs::read(dist_info.join("RECORD")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Some(format!("missing {name}/RECORD"));
        }
        Err(_) => return None,
    };
    for line in String::from_utf8_lossy(&record).lines() {
        let Some(path) = record_path(line) else {
            continue;
        };
        let rel = Path::new(&path);
        let plain = rel.components().all(|c| matches!(c, Component::Normal(_)));
        if !plain || rel.as_os_str().is_empty() {
            continue;
        }
        let gone = matches!(
            fs::symlink_metadata(archive.join(rel)),
            Err(e) if e.kind() == ErrorKind::NotFound
        );
        if gone {
            return Some(format!("missing {path}"));
        }
    }
    None
}

/// The path column of one `RECORD` CSV row (`path,hash,size`). Hash and size
/// never contain a comma, so an unquoted path is everything before the last
/// two; a quoted path follows CSV quoting.
fn record_path(line: &str) -> Option<String> {
    let line = line.trim_end_matches('\r');
    if let Some(rest) = line.strip_prefix('"') {
        let mut path = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            if c == '"' {
                match chars.next() {
                    Some('"') => path.push('"'),
                    _ => return Some(path),
                }
            } else {
                path.push(c);
            }
        }
        return None;
    }
    let mut fields = line.rsplitn(3, ',');
    let (_size, _hash) = (fields.next()?, fields.next()?);
    fields.next().map(str::to_string)
}

/// A held lock file; the lock is released when the file closes on drop.
struct HeldLock {
    _file: fs::File,
}

/// Open (creating if needed) a lock file the way uv does.
fn open_lock(path: &Path) -> Option<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()
}

/// Non-blocking shared lock on `<root>/.lock`. `None` while `uv cache
/// clean`/`prune` holds it exclusively.
fn lock_cache_shared(root: &Path) -> Option<HeldLock> {
    use crate::file_lock::FileExt;
    let file = open_lock(&root.join(".lock"))?;
    matches!(FileExt::try_lock_shared(&file), Ok(true)).then_some(HeldLock { _file: file })
}

/// Invalidate one pointer under its uv entry lock. `false` = skipped.
fn invalidate(root: &Path, pointer: &WheelPointer, why: &str) -> bool {
    use crate::file_lock::FileExt;
    if !crate::platform::host::UNIX {
        return false;
    }
    let Some(lock) = open_lock(&sibling(&pointer.link, "lock")) else {
        return false;
    };
    // uv holds this lock across lookup, download and publication of the
    // entry; a held lock means uv is replacing the pointer right now.
    if !matches!(FileExt::try_lock_exclusive(&lock), Ok(true)) {
        return false;
    }
    let _held = HeldLock { _file: lock };
    let Ok(archive_dir) = fs::canonicalize(root.join("archive-v0")) else {
        return false;
    };
    // Re-check under the lock: uv may have republished between scan and lock.
    if resolve_archive(&pointer.link, &archive_dir).as_ref() != Some(&pointer.archive) {
        return false;
    }
    let rule = format!("{REPAIR_RULE}: {why}");
    let mut ok = true;
    for path in pointer.pointer_files.iter().chain([&pointer.link]) {
        // Audit each path before acting (#893).
        super::delete_audit::record("gc.uv-cache-repair", path, &rule);
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(_) => ok = false,
        }
    }
    ok
}

#[cfg(test)]
#[path = "uv_cache_repair_tests.rs"]
mod tests;

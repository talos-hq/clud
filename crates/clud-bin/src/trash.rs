//! `clud trash` quarantine command.
//!
//! Moves paths into a per-user trash root and records a `kind = "trash"`
//! GC row so the daemon can retry deletion until locked DLL/EXE/PYD files
//! become removable.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::args::Args;
use crate::gc::InsertInput;

static SUFFIX_COUNTER: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone)]
pub struct TrashRecord {
    pub origin_path: String,
    pub quarantine_dir: PathBuf,
    pub quarantined_path: PathBuf,
}

#[derive(Debug)]
pub enum TrashError {
    InvalidPath(PathBuf),
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    CrossVolume {
        source: PathBuf,
        trash_root: PathBuf,
        source_volume: String,
        trash_volume: String,
    },
}

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath(path) => write!(f, "{} has no basename to trash", path.display()),
            Self::Io {
                action,
                path,
                source,
            } => {
                write!(f, "{action} {}: {source}", path.display())
            }
            Self::CrossVolume {
                source,
                trash_root,
                source_volume,
                trash_volume,
            } => write!(
                f,
                "{} is on volume {source_volume}, trash root {} is on {trash_volume}. Pass --cross-volume to copy + best-effort remove instead.",
                source.display(),
                trash_root.display()
            ),
        }
    }
}

impl std::error::Error for TrashError {}

/// Dispatch a `clud trash` invocation. Returns the process exit code.
pub fn run(args: &Args, paths: &[PathBuf], cross_volume: bool) -> i32 {
    if args.no_daemon {
        eprintln!("error: trash operations require the clud daemon; remove --no-daemon");
        return 2;
    }
    let trash_root = match crate::daemon::default_trash_dir() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("error: cannot resolve clud trash dir: {err}");
            return 1;
        }
    };
    let state_dir = match crate::daemon::default_state_dir() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("error: cannot resolve clud state dir: {err}");
            return 1;
        }
    };

    if args.dry_run {
        return dry_run(paths, &trash_root);
    }

    let mut failed = false;
    for path in paths {
        match trash_one_path(path, &trash_root, cross_volume) {
            Ok(record) => {
                // #2096: the session may later erase what it trashed.
                if let Some(session) = crate::rm_tool::session_id_from_env() {
                    crate::rm_tool::own_trash::write_owner(&record.quarantine_dir, &session);
                }
                let input = insert_input_for_record(&record, now_unix());
                match crate::daemon::gc_client_insert(&state_dir, &input) {
                    Ok(()) => {
                        println!(
                            "trashed {} -> {}",
                            record.origin_path,
                            record.quarantined_path.display()
                        );
                    }
                    Err(err) => {
                        // Registration is best-effort. `clud trash` is a
                        // utility mode, so it never starts a daemon, and with
                        // none running the row cannot be written. The file is
                        // still quarantined, which is what the command
                        // promises -- reporting failure here would tell the
                        // caller the trashing itself did not happen.
                        println!(
                            "trashed {} -> {}",
                            record.origin_path,
                            record.quarantined_path.display()
                        );
                        eprintln!(
                            "[clud] warning: GC registration skipped for {}: {err}",
                            record.quarantined_path.display()
                        );
                    }
                }
            }
            Err(err) => {
                failed = true;
                eprintln!("error: {err}");
            }
        }
    }

    if failed {
        1
    } else {
        0
    }
}

/// `clud trash --dry-run`: report what would be quarantined and move nothing.
/// A missing path is an error, exactly as the real run reports it.
fn dry_run(paths: &[PathBuf], trash_root: &Path) -> i32 {
    let mut failed = false;
    for path in paths {
        match std::fs::symlink_metadata(path) {
            Ok(_) => println!("would trash {} -> {}", path.display(), trash_root.display()),
            Err(err) => {
                failed = true;
                eprintln!("error: stat {}: {err}", path.display());
            }
        }
    }
    i32::from(failed)
}

pub fn insert_input_for_record(record: &TrashRecord, now_unix: i64) -> InsertInput {
    InsertInput {
        kind: "trash".to_string(),
        path: record.quarantine_dir.to_string_lossy().to_string(),
        repo_root: None,
        branch: None,
        agent_id: Some(record.origin_path.clone()),
        now_unix,
    }
}

pub fn trash_one_path(
    path: &Path,
    trash_root: &Path,
    cross_volume: bool,
) -> Result<TrashRecord, TrashError> {
    let source = absolute_path(path).map_err(|source| TrashError::Io {
        action: "resolve",
        path: path.to_path_buf(),
        source,
    })?;
    let source_meta = std::fs::symlink_metadata(&source).map_err(|source_err| TrashError::Io {
        action: "stat",
        path: source.clone(),
        source: source_err,
    })?;
    let basename = source
        .file_name()
        .map(OsStr::to_os_string)
        .ok_or_else(|| TrashError::InvalidPath(source.clone()))?;

    std::fs::create_dir_all(trash_root).map_err(|source| TrashError::Io {
        action: "create_dir_all",
        path: trash_root.to_path_buf(),
        source,
    })?;
    let trash_root_abs = absolute_path(trash_root).map_err(|source| TrashError::Io {
        action: "resolve",
        path: trash_root.to_path_buf(),
        source,
    })?;

    if !cross_volume && volumes_differ(&source, &trash_root_abs) {
        let source_volume = volume_label(&source);
        let trash_volume = volume_label(&trash_root_abs);
        return Err(TrashError::CrossVolume {
            source,
            trash_root: trash_root_abs,
            source_volume,
            trash_volume,
        });
    }

    let quarantine_dir = create_unique_quarantine_dir(&trash_root_abs)?;
    let quarantined_path = quarantine_dir.join(basename);
    let move_result = if cross_volume {
        copy_path(&source, &quarantined_path, &source_meta)
            .map(|_| remove_source_best_effort(&source, &source_meta))
    } else {
        std::fs::rename(&source, &quarantined_path).map_err(|source_err| {
            if is_cross_volume_error(&source_err) {
                TrashError::CrossVolume {
                    source: source.clone(),
                    trash_root: trash_root_abs.clone(),
                    source_volume: volume_label(&source),
                    trash_volume: volume_label(&trash_root_abs),
                }
            } else {
                TrashError::Io {
                    action: "rename",
                    path: source.clone(),
                    source: source_err,
                }
            }
        })
    };

    if let Err(err) = move_result {
        let _ = std::fs::remove_dir_all(&quarantine_dir);
        return Err(err);
    }

    Ok(TrashRecord {
        origin_path: source.to_string_lossy().to_string(),
        quarantine_dir,
        quarantined_path,
    })
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn create_unique_quarantine_dir(trash_root: &Path) -> Result<PathBuf, TrashError> {
    for _ in 0..32 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let name = format!(
            "{}-{}",
            timestamp_utcish(now.as_secs() as i64),
            randomish_hex(now)
        );
        let dir = trash_root.join(name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(TrashError::Io {
                    action: "create_dir",
                    path: dir,
                    source,
                });
            }
        }
    }
    Err(TrashError::Io {
        action: "create unique trash dir",
        path: trash_root.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique trash directory",
        ),
    })
}

pub(crate) fn randomish_hex(now: std::time::Duration) -> String {
    let counter = SUFFIX_COUNTER.fetch_add(1, Ordering::Relaxed) as u64;
    let mut x =
        now.as_nanos() as u64 ^ ((std::process::id() as u64) << 24) ^ counter.rotate_left(17);
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    format!("{:06x}", x & 0x00ff_ffff)
}

pub(crate) fn timestamp_utcish(unix_secs: i64) -> String {
    // Format only. This string names quarantine directories that already
    // exist on disk, so it must stay byte-for-byte identical; the calendar
    // arithmetic lives in `crate::civil_time` (#1206).
    let (year, month, day, hour, minute, second) =
        crate::civil_time::civil_from_unix_secs(unix_secs);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

fn copy_path(source: &Path, dest: &Path, metadata: &std::fs::Metadata) -> Result<(), TrashError> {
    if metadata.is_dir() {
        copy_dir_all(source, dest)
    } else {
        std::fs::copy(source, dest)
            .map(|_| ())
            .map_err(|source_err| TrashError::Io {
                action: "copy",
                path: source.to_path_buf(),
                source: source_err,
            })
    }
}

fn copy_dir_all(source: &Path, dest: &Path) -> Result<(), TrashError> {
    std::fs::create_dir_all(dest).map_err(|source_err| TrashError::Io {
        action: "create_dir_all",
        path: dest.to_path_buf(),
        source: source_err,
    })?;
    for entry in std::fs::read_dir(source).map_err(|source_err| TrashError::Io {
        action: "read_dir",
        path: source.to_path_buf(),
        source: source_err,
    })? {
        let entry = entry.map_err(|source_err| TrashError::Io {
            action: "read_dir entry",
            path: source.to_path_buf(),
            source: source_err,
        })?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let metadata = entry.metadata().map_err(|source_err| TrashError::Io {
            action: "stat",
            path: from.clone(),
            source: source_err,
        })?;
        copy_path(&from, &to, &metadata)?;
    }
    Ok(())
}

fn remove_source_best_effort(source: &Path, metadata: &std::fs::Metadata) {
    // Audit before acting (#893): this deletes the user's original after the
    // cross-volume copy into quarantine succeeded — the one trash-path
    // deletion of live user data rather than clud-owned copies.
    crate::gc::delete_audit::record("trash.cross-volume-source", source, "trash move completed");
    let _ = if metadata.is_dir() {
        std::fs::remove_dir_all(source)
    } else {
        std::fs::remove_file(source)
    };
}

pub(crate) fn is_cross_volume_error(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::CrossesDevices || matches!(err.raw_os_error(), Some(17 | 18))
}

fn volumes_differ(source: &Path, trash_root: &Path) -> bool {
    match (volume_key(source), volume_key(trash_root)) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

fn volume_key(path: &Path) -> Option<String> {
    use std::path::{Component, Prefix};

    if !crate::platform::host::WINDOWS {
        return None;
    }
    let Component::Prefix(prefix) = path.components().next()? else {
        return None;
    };
    match prefix.kind() {
        Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => {
            Some(format!("{}:", (disk as char).to_ascii_uppercase()))
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some(format!(
            "\\\\{}\\{}",
            server.to_string_lossy().to_ascii_lowercase(),
            share.to_string_lossy().to_ascii_lowercase()
        )),
        _ => Some(prefix.as_os_str().to_string_lossy().to_string()),
    }
}

fn volume_label(path: &Path) -> String {
    volume_key(path).unwrap_or_else(|| {
        path.components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamp_formats_epoch_as_utcish() {
        assert_eq!(timestamp_utcish(0), "19700101T000000Z");
        assert_eq!(timestamp_utcish(86_400), "19700102T000000Z");
    }

    #[test]
    fn random_suffix_is_six_hex_chars() {
        let value = randomish_hex(Duration::from_nanos(123));
        assert_eq!(value.len(), 6);
        assert!(value.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// `clud trash --dry-run` once moved the path for real: the global flag was
    /// parsed and ignored. It must leave every path where it is.
    #[test]
    fn dry_run_moves_nothing() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let source = dir.path().join("artifact.log");
        let trash_root = dir.path().join("trash");
        std::fs::write(&source, b"keep me").unwrap();
        assert_eq!(dry_run(std::slice::from_ref(&source), &trash_root), 0);
        assert!(source.exists(), "dry run must not move the path");
        assert!(
            !trash_root.exists(),
            "dry run must not create the trash dir"
        );
    }

    #[test]
    fn dry_run_reports_a_missing_path_as_an_error() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let missing = dir.path().join("absent");
        assert_eq!(dry_run(&[missing], &dir.path().join("trash")), 1);
    }

    #[test]
    fn same_volume_trash_renames_file_into_quarantine_dir() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let source = dir.path().join("artifact.dll");
        let trash_root = dir.path().join("trash");
        std::fs::write(&source, b"locked-ish").unwrap();

        let record = trash_one_path(&source, &trash_root, false).unwrap();

        assert!(!source.exists());
        assert_eq!(record.quarantine_dir.parent(), Some(trash_root.as_path()));
        assert_eq!(
            record.quarantined_path.file_name().and_then(OsStr::to_str),
            Some("artifact.dll")
        );
        assert_eq!(
            std::fs::read(&record.quarantined_path).unwrap(),
            b"locked-ish"
        );
        assert_eq!(record.origin_path, source.to_string_lossy());
    }

    #[test]
    fn cross_volume_mode_copies_then_best_effort_removes_source() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let source = dir.path().join("artifact.exe");
        let trash_root = dir.path().join("trash");
        std::fs::write(&source, b"copy me").unwrap();

        let record = trash_one_path(&source, &trash_root, true).unwrap();

        assert!(!source.exists(), "best-effort remove should succeed here");
        assert_eq!(std::fs::read(&record.quarantined_path).unwrap(), b"copy me");
    }

    #[test]
    fn insert_input_tracks_quarantine_dir_and_origin_agent_id() {
        let record = TrashRecord {
            origin_path: "C:/repo/target/debug/foo.dll".to_string(),
            quarantine_dir: PathBuf::from("C:/Users/me/.clud/trash/20260101T000000Z-abcdef"),
            quarantined_path: PathBuf::from(
                "C:/Users/me/.clud/trash/20260101T000000Z-abcdef/foo.dll",
            ),
        };

        let input = insert_input_for_record(&record, 123);
        assert_eq!(input.kind, "trash");
        assert_eq!(
            input.path,
            "C:/Users/me/.clud/trash/20260101T000000Z-abcdef"
        );
        assert_eq!(
            input.agent_id.as_deref(),
            Some("C:/repo/target/debug/foo.dll")
        );
        assert_eq!(input.now_unix, 123);
    }
}

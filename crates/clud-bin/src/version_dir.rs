//! Per-build alias directories (DD-203), adapted from soldr's
//! `versioned_shims_dir` and `self_relocate` runtime tree.
//!
//! Every build of `clud` owns `~/.clud/state/versions/v<version>-<hash>/`:
//!
//! ```text
//! image/clud    the pinned executable: a hardlink or copy, never a symlink
//! shims/        interpreter aliases
//! rm-shim/      session aliases
//! helper-bin/   scanner aliases
//! last-used     Unix seconds of the last launch (the GC ledger)
//! .lease        shared-locked by every process using this directory
//! ```
//!
//! The hash covers the executable's bytes, so two dev builds of one version
//! never share a directory, while two installs of the same bytes do. Nothing
//! is ever relinked in place, so versions cannot clobber each other and a
//! session sees one build for its whole life.
//!
//! Aliases link to `image/clud`, not to the install path. A symlink to the
//! install path would silently start running the next version after an
//! upgrade; one into this directory cannot.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;
use sha2::{Digest, Sha256};

use crate::alias_link::{self, Method};

/// Under the user's home: one directory per build.
pub const ROOT_SUBDIR: &str = ".clud/state/versions";
pub const INTERPRETER_LEAF: &str = "shims";
pub const SESSION_LEAF: &str = "rm-shim";
pub const HELPER_LEAF: &str = "helper-bin";

const LOCK_FILE: &str = ".lock";
const GC_MARKER: &str = ".last-gc";
const HASH_MEMO_DIR: &str = ".image-hash";
const LEASE_FILE: &str = ".lease";
const LAST_USED: &str = "last-used";
const GC_PREFIX: &str = ".gc-";
/// Hex digits of the image hash kept in a directory name.
const HASH_CHARS: usize = 16;
/// soldr's `GC_INTERVAL_SECONDS`.
pub const GC_INTERVAL_SECONDS: u64 = 24 * 60 * 60;
/// soldr's `STALE_RUNTIME_SECONDS`.
pub const STALE_SECONDS: u64 = 48 * 60 * 60;

/// One build's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDir {
    pub root: PathBuf,
}

impl VersionDir {
    pub fn image(&self) -> PathBuf {
        self.root
            .join("image")
            .join(crate::shim_registry::file_name("clud"))
    }

    pub fn interpreter(&self) -> PathBuf {
        self.root.join(INTERPRETER_LEAF)
    }

    pub fn session(&self) -> PathBuf {
        self.root.join(SESSION_LEAF)
    }

    pub fn helper(&self) -> PathBuf {
        self.root.join(HELPER_LEAF)
    }
}

fn root_of(home: &Path) -> PathBuf {
    home.join(ROOT_SUBDIR)
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The image's hash, memoized by path, size and mtime so a launch reads a
/// large binary only once per build (soldr#2521 memoizes the same way).
fn image_hash(root: &Path, source: &Path) -> io::Result<String> {
    let meta = fs::metadata(source)?;
    let modified = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let stamp = format!("{} {modified}", meta.len());
    let mut key = Sha256::new();
    key.update(source.to_string_lossy().as_bytes());
    let key: String = key
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let memo = root.join(HASH_MEMO_DIR).join(key);
    if let Ok(body) = fs::read_to_string(&memo) {
        if let Some((seen, hash)) = body.rsplit_once(' ') {
            if seen == stamp {
                return Ok(hash.to_string());
            }
        }
    }
    let hash = hash_file(source)?;
    // Best effort: without the memo the next launch hashes again.
    // Only under a root that exists: asking must not create one.
    if let Some(parent) = memo.parent().filter(|_| root.is_dir()) {
        let _ = fs::create_dir_all(parent);
        let staged = memo.with_extension(format!("{}.tmp", std::process::id()));
        if fs::write(&staged, format!("{stamp} {hash}")).is_ok()
            && fs::rename(&staged, &memo).is_err()
        {
            let _ = fs::remove_file(&staged);
        }
    }
    Ok(hash)
}

fn dir_name(version: &str, hash: &str) -> String {
    format!("v{version}-{}", &hash[..HASH_CHARS.min(hash.len())])
}

/// The directory `source` belongs in. Creates nothing beyond the hash memo,
/// so `clud doctor` and a dry run can ask without installing.
pub fn locate(home: &Path, source: &Path) -> io::Result<VersionDir> {
    let root = root_of(home);
    let hash = image_hash(&root, source)?;
    Ok(VersionDir {
        root: root.join(dir_name(env!("CARGO_PKG_VERSION"), &hash)),
    })
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn write_last_used(dir: &Path, now: u64) -> io::Result<()> {
    let staged = dir.join(format!(".{LAST_USED}.{}.tmp", std::process::id()));
    fs::write(&staged, now.to_string())?;
    fs::rename(&staged, dir.join(LAST_USED))
}

fn read_last_used(dir: &Path) -> Option<u64> {
    fs::read_to_string(dir.join(LAST_USED))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// Hold a shared lock on `dir`'s lease for this process's life. The kernel
/// drops it when the process dies, however it dies, so unlike a recorded PID
/// it cannot go stale or be fooled by PID reuse. Rust opens files
/// close-on-exec, so a child does not inherit it.
fn hold_lease(dir: &Path) -> io::Result<()> {
    static HELD: OnceLock<Mutex<HashMap<PathBuf, File>>> = OnceLock::new();
    let mut held = HELD
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if !held.contains_key(dir) {
        let file = open_lock_file(&dir.join(LEASE_FILE))?;
        FileExt::lock_shared(&file)?;
        held.insert(dir.to_path_buf(), file);
    }
    Ok(())
}

/// Create (or reuse) `source`'s directory, pin its image, stamp the ledger,
/// take a lease and run the periodic GC. All of it under the root lock, so a
/// sweep can never remove a directory between its lookup and its lease.
pub fn place(home: &Path, source: &Path) -> io::Result<VersionDir> {
    let root = root_of(home);
    fs::create_dir_all(&root)?;
    let lock = open_lock_file(&root.join(LOCK_FILE))?;
    FileExt::lock_exclusive(&lock)?;
    let dir = locate(home, source)?;
    fs::create_dir_all(dir.image().parent().unwrap_or(&dir.root))?;
    // Never a symlink: the image must survive the install being replaced.
    alias_link::install_alias_with(source, &dir.image(), &[Method::Hardlink, Method::Copy])?;
    let now = now_seconds();
    write_last_used(&dir.root, now)?;
    hold_lease(&dir.root)?;
    let _ = sweep_if_due(&root, &dir.root, now);
    Ok(dir)
}

/// Install `names` in `alias_dir` as aliases of the pinned image. A failure
/// keeps an alias that already exists: on Windows a running one is locked.
pub fn install_aliases(
    dir: &VersionDir,
    alias_dir: &Path,
    names: &[String],
    order: &[Method],
) -> io::Result<()> {
    fs::create_dir_all(alias_dir)?;
    let image = dir.image();
    for name in names {
        let target = alias_dir.join(name);
        if let Err(error) = alias_link::install_alias_with(&image, &target, order) {
            if !target.is_file() {
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Every existing `<build>/<leaf>` under `home`, whichever build owns it. A
/// session's PATH can hold another build's aliases, and those must never be
/// mistaken for the real `gh` or `git`.
pub fn alias_dirs_named(home: &Path, leaf: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root_of(home)) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join(leaf))
        .filter(|dir| dir.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Whether `dir` is a `<build>/<leaf>` directory under `home`.
pub fn is_alias_dir_named(home: &Path, dir: &Path, leaf: &str) -> bool {
    dir.file_name().is_some_and(|name| name == leaf)
        && dir
            .parent()
            .and_then(Path::parent)
            .is_some_and(|parent| parent == root_of(home))
}

/// What one GC pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcSummary {
    pub removed: usize,
    pub leased: usize,
    pub fresh: usize,
}

fn sweep_if_due(root: &Path, current: &Path, now: u64) -> io::Result<Option<GcSummary>> {
    let marker = root.join(GC_MARKER);
    let last = fs::read_to_string(&marker)
        .ok()
        .and_then(|body| body.trim().parse::<u64>().ok());
    if last.is_some_and(|last| now.saturating_sub(last) < GC_INTERVAL_SECONDS) {
        return Ok(None);
    }
    fs::write(&marker, now.to_string())?;
    sweep(root, current, now, STALE_SECONDS).map(Some)
}

/// Whether a process holds `dir`'s lease. An unreadable lease counts as held:
/// every doubt spares.
fn is_leased(dir: &Path) -> bool {
    let Ok(file) = open_lock_file(&dir.join(LEASE_FILE)) else {
        return true;
    };
    !matches!(FileExt::try_lock_exclusive(&file), Ok(true))
}

/// Remove every build other than `current` whose ledger is older than
/// `stale_seconds` and whose lease nobody holds. The caller holds the root
/// lock, and leases are only taken under it.
///
/// A directory is renamed before it is deleted, so a failed delete can never
/// leave a half-empty build that still looks usable. On Windows the rename
/// itself fails while one of its files runs, which spares it. A missing
/// ledger is written, not trusted to an mtime (soldr#1495).
fn sweep(root: &Path, current: &Path, now: u64, stale_seconds: u64) -> io::Result<GcSummary> {
    let mut summary = GcSummary::default();
    for entry in fs::read_dir(root)?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(GC_PREFIX) {
            let _ = fs::remove_dir_all(&path);
            continue;
        }
        if !name.starts_with('v') || !path.is_dir() || path == current {
            continue;
        }
        let Some(used) = read_last_used(&path) else {
            let _ = write_last_used(&path, now);
            summary.fresh += 1;
            continue;
        };
        if now.saturating_sub(used) < stale_seconds {
            summary.fresh += 1;
            continue;
        }
        if is_leased(&path) {
            summary.leased += 1;
            continue;
        }
        let doomed = root.join(format!("{GC_PREFIX}{name}-{}", std::process::id()));
        if fs::rename(&path, &doomed).is_ok() {
            let _ = fs::remove_dir_all(&doomed);
            summary.removed += 1;
        }
    }
    Ok(summary)
}

#[cfg(test)]
#[path = "version_dir_tests.rs"]
mod tests;

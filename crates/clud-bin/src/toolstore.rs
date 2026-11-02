//! Content-addressed store for the `clud` multicall binary (#2558).
//!
//! A hardlink is a second name for one inode, so it carries no version of its
//! own, and a size/mtime test cannot tell two same-size builds apart. The
//! names clud owns under `~/.clud/state` (`helper-bin/*`, `shims/*`,
//! `rm-shim/*`) therefore point into this store instead of at whatever file
//! the launcher happens to be:
//!
//! ```text
//! ~/.clud/toolstore/<build-id>/clud      one immutable binary per build
//! ~/.clud/state/helper-bin/clud-cmd-scan -> ~/.clud/toolstore/<build-id>/clud
//! ```
//!
//! - **Publish** ([`publish`]) copies the launcher into a temp dir and renames
//!   it into place. An existing build dir is never written to or replaced, so
//!   an upgrade cannot pull a build out from under a running session.
//! - **Point** ([`install_owned`]) re-points a name with a staged symlink and
//!   a rename, so a concurrent reader never sees a half-written name. Where
//!   symlinks are unavailable (Windows without Developer Mode) the
//!   [`crate::alias_link`] hardlink/copy ladder is the fallback, and a
//!   `.<name>.clud-build` sidecar records the build the name holds.
//! - **Freshness** ([`resolved_build`]) is exact: the build a name resolves
//!   to, compared with the launcher's [`build_id`].
//! - **GC** ([`prune`]) keeps the launcher's build, every build a name or a
//!   live launch pin references, and the newest [`KEEP_NEWEST`]; it runs from
//!   the daemon's periodic GC tick.
//!
//! The store is additive: no existing `~/.clud` path moved.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::alias_link::{self, Method, Placed};

/// Under the user's home: the store root.
pub const STORE_SUBDIR: &str = ".clud/toolstore";
/// The binary's file name inside a build dir.
pub const BIN_NAME: &str = "clud";
/// Builds kept regardless of references.
pub const KEEP_NEWEST: usize = 2;
/// Order a clud-owned name tries: symlink first, the ladder as fallback.
pub const OWNED_ORDER: [Method; 3] = [Method::Symlink, Method::Hardlink, Method::Copy];
/// Dirs under the home whose entries are clud-owned names.
pub const OWNED_SUBDIRS: [&str; 3] = [
    crate::shim_registry::INTERPRETER_SUBDIR,
    crate::shim_registry::SESSION_SUBDIR,
    ".clud/state/helper-bin",
];
const PINS_DIR: &str = ".pins";
const ID_CACHE_DIR: &str = ".ids";
const SIDECAR_SUFFIX: &str = ".clud-build";

pub fn store_dir(home: &Path) -> PathBuf {
    home.join(STORE_SUBDIR)
}

/// `toolstore/<id>/clud`.
pub fn build_bin(home: &Path, id: &str) -> PathBuf {
    store_dir(home)
        .join(id)
        .join(crate::shim_registry::file_name(BIN_NAME))
}

/// Keep only characters safe in one path component on every OS.
pub fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The build id of `exe`: `<version>+<12 hex>`, from the ELF build-id note
/// when there is one, else a SHA-256 of the bytes. The hash is memoized under
/// the store by (path, length, mtime) so a launch pays it once per build.
pub fn build_id(home: &Path, exe: &Path) -> io::Result<String> {
    let exe = std::fs::canonicalize(exe)?;
    let meta = std::fs::metadata(&exe)?;
    let stamp = format!("{} {}", meta.len(), mtime_nanos(&meta));
    let cache = id_cache_path(home, &exe);
    if let Ok(cached) = std::fs::read_to_string(&cache) {
        if let Some(id) = cached.strip_prefix(&format!("{stamp} ")) {
            return Ok(id.trim().to_string());
        }
    }
    let digest = match crate::build_id::read_elf_build_id(&exe) {
        Some(note) => note,
        None => sha256_hex(&exe)?,
    };
    let short: String = digest.chars().take(12).collect();
    let id = sanitize(&format!("{}+{short}", env!("CARGO_PKG_VERSION")));
    if let Some(dir) = cache.parent() {
        if std::fs::create_dir_all(dir).is_ok() {
            let _ = write_atomic(&cache, format!("{stamp} {id}").as_bytes());
        }
    }
    Ok(id)
}

fn mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

fn id_cache_path(home: &Path, exe: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let key = Sha256::digest(exe.to_string_lossy().as_bytes());
    let key: String = key.iter().take(8).map(|b| format!("{b:02x}")).collect();
    store_dir(home).join(ID_CACHE_DIR).join(key)
}

fn sha256_hex(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn unique(dir: &Path, stem: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    dir.join(format!(
        ".{stem}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    let staging = unique(dir, "write");
    std::fs::write(&staging, body)?;
    std::fs::rename(&staging, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&staging);
    })
}

/// Publish `exe` as build `id` and return the canonical store binary. An
/// existing build dir is reused as is, never rewritten.
pub fn publish(home: &Path, exe: &Path, id: &str) -> io::Result<PathBuf> {
    let bin = build_bin(home, id);
    if !bin.is_file() {
        let store = store_dir(home);
        std::fs::create_dir_all(&store)?;
        let staging = unique(&store, id);
        std::fs::create_dir_all(&staging)?;
        let staged_bin = staging.join(crate::shim_registry::file_name(BIN_NAME));
        let result = alias_link::place(exe, &staged_bin, &[Method::Copy])
            .and_then(|_| std::fs::rename(&staging, store.join(id)));
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
            // Another launcher may have published the same build first.
            if !bin.is_file() {
                result?;
            }
        }
    }
    pin(home, id);
    std::fs::canonicalize(&bin)
}

/// Record that this process launched build `id`, so GC keeps it while the
/// process lives. Best effort.
fn pin(home: &Path, id: &str) {
    let dir = store_dir(home).join(id).join(PINS_DIR);
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(std::process::id().to_string()), b"");
    }
}

fn sidecar_path(target: &Path) -> Option<PathBuf> {
    let name = target.file_name()?.to_string_lossy().into_owned();
    Some(target.with_file_name(format!(".{name}{SIDECAR_SUFFIX}")))
}

/// The store build `target` resolves to: the build dir its symlink names, or
/// the sidecar a fallback hardlink/copy carries. `None` for a name that is
/// not in the store (a pre-#2558 hardlink, a foreign file).
pub fn resolved_build(home: &Path, target: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(target).ok()?;
    if meta.file_type().is_symlink() {
        let link = std::fs::read_link(target).ok()?;
        let store = std::fs::canonicalize(store_dir(home)).ok()?;
        let resolved = std::fs::canonicalize(&link).unwrap_or(link);
        let build_dir = resolved.parent()?;
        return (build_dir.parent()? == store)
            .then(|| {
                build_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .flatten();
    }
    let id = std::fs::read_to_string(sidecar_path(target)?).ok()?;
    let id = id.trim().to_string();
    let bin = std::fs::metadata(build_bin(home, &id)).ok()?;
    (meta.len() == bin.len()).then_some(id)
}

/// Point clud-owned `target` at the store build of `exe`, publishing it first.
pub fn install_owned(home: &Path, exe: &Path, target: &Path) -> io::Result<Placed> {
    install_owned_with(home, exe, target, &OWNED_ORDER)
}

/// [`install_owned`] restricted to `order`, so a test can force a fallback.
pub fn install_owned_with(
    home: &Path,
    exe: &Path,
    target: &Path,
    order: &[Method],
) -> io::Result<Placed> {
    let id = build_id(home, exe)?;
    let bin = publish(home, exe, &id)?;
    if resolved_build(home, target).as_deref() == Some(id.as_str()) {
        return Ok(Placed::Unchanged);
    }
    let method = alias_link::place(&bin, target, order)?;
    if let Some(sidecar) = sidecar_path(target) {
        if method == Method::Symlink {
            let _ = std::fs::remove_file(&sidecar);
        } else {
            write_atomic(&sidecar, id.as_bytes())?;
        }
    }
    Ok(Placed::Made(method))
}

/// Whether `target` already resolves to the store build of `exe`.
pub fn is_current(home: &Path, exe: &Path, target: &Path) -> bool {
    build_id(home, exe).is_ok_and(|id| resolved_build(home, target).as_deref() == Some(id.as_str()))
}

/// One build dir and when it was published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub id: String,
    pub published: SystemTime,
}

/// The builds GC may delete: neither referenced nor among the newest `keep`.
pub fn select_prunable(
    builds: &[Build],
    referenced: &BTreeSet<String>,
    keep: usize,
) -> Vec<String> {
    let mut sorted = builds.to_vec();
    sorted.sort_by(|a, b| b.published.cmp(&a.published).then(b.id.cmp(&a.id)));
    sorted
        .into_iter()
        .skip(keep)
        .filter(|build| !referenced.contains(&build.id))
        .map(|build| build.id)
        .collect()
}

fn list_builds(store: &Path) -> Vec<Build> {
    let Ok(entries) = std::fs::read_dir(store) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| Build {
            id: entry.file_name().to_string_lossy().into_owned(),
            published: entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
        })
        .collect()
}

/// Builds a clud-owned name under `home` resolves to.
fn named_builds(home: &Path) -> BTreeSet<String> {
    let mut named = BTreeSet::new();
    for subdir in OWNED_SUBDIRS {
        let Ok(entries) = std::fs::read_dir(home.join(subdir)) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if let Some(id) = resolved_build(home, &entry.path()) {
                named.insert(id);
            }
        }
    }
    named
}

/// Whether a live process pinned build dir `dir`; dead pins are removed.
fn pinned_live(dir: &Path, alive: &dyn Fn(u32) -> bool) -> bool {
    let Ok(entries) = std::fs::read_dir(dir.join(PINS_DIR)) else {
        return false;
    };
    let mut live = false;
    for entry in entries.filter_map(Result::ok) {
        let pid = entry.file_name().to_string_lossy().parse::<u32>().ok();
        if pid.is_some_and(alive) {
            live = true;
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    live
}

/// Delete every unreferenced build beyond the newest [`KEEP_NEWEST`] and
/// return the deleted ids. `launcher` is the running build; `alive` probes a
/// pinned pid. A build is renamed aside before deletion, so a launcher never
/// sees half a build dir, and a build still in use where the OS refuses the
/// delete (Windows) simply stays for the next tick.
pub fn prune(home: &Path, launcher: Option<&str>, alive: &dyn Fn(u32) -> bool) -> Vec<String> {
    let store = store_dir(home);
    let builds = list_builds(&store);
    let mut referenced = named_builds(home);
    referenced.extend(launcher.map(str::to_string));
    for build in &builds {
        if pinned_live(&store.join(&build.id), alive) {
            referenced.insert(build.id.clone());
        }
    }
    let mut deleted = Vec::new();
    for id in select_prunable(&builds, &referenced, KEEP_NEWEST) {
        let aside = unique(&store, &format!("trash-{id}"));
        if std::fs::rename(store.join(&id), &aside).is_ok() {
            let _ = std::fs::remove_dir_all(&aside);
            deleted.push(id);
        }
    }
    deleted
}

/// The daemon GC tick's entry point: prune the store under the user's home,
/// keeping the running build.
pub fn prune_current() -> Vec<String> {
    let Some(home) = crate::home::user_home() else {
        return Vec::new();
    };
    let launcher = std::env::current_exe()
        .ok()
        .and_then(|exe| build_id(&home, &exe).ok());
    prune(
        &home,
        launcher.as_deref(),
        &crate::platform::session_registry::os_probe_is_alive,
    )
}

#[cfg(test)]
#[path = "toolstore_tests.rs"]
mod tests;

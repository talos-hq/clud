//! Materialize a `clud` alias: hardlink, then symlink, then copy (#1551).
//!
//! `clud` answers to many names (see [`crate::multicall`]), and each name is
//! a file that resolves to the one real `clud`. Making that file must never
//! fail a launch, so [`install_alias`] tries, in a fixed order on every OS:
//!
//! 1. **Hardlink.** No disk cost, no privilege on POSIX or NTFS; fails across
//!    filesystems (`EXDEV`) and on FAT or network shares.
//! 2. **Symlink.** On Windows this needs Developer Mode or admin, so it is
//!    expected to fail there.
//! 3. **Copy.** The guaranteed last resort.
//!
//! A failed link is a fallback, never an error; only a failed copy is one.
//! Each candidate is staged under a unique name and renamed over the target,
//! so a concurrent reader never sees a half-written alias.
//!
//! An alias is *fresh* when it is the same file as the source (hardlink), a
//! symlink that resolves to it, or a copy with the source's size and mtime.
//! An upgrade replaces `clud`, so every alias goes stale at once and the next
//! launch relinks it.
//!
//! Size plus mtime cannot tell two builds of the same size apart (#2558), so
//! the names under `~/.clud/state` no longer use this test: they point into
//! the content-addressed [`crate::toolstore`], which decides freshness by the
//! build a name resolves to. This module stays the link/copy ladder for it
//! and for aliases beside a build output (`clud __link-aliases`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Hardlink,
    Symlink,
    Copy,
}

/// The order every OS tries.
pub const DEFAULT_ORDER: [Method; 3] = [Method::Hardlink, Method::Symlink, Method::Copy];

/// What [`install_alias`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placed {
    /// The alias was already fresh.
    Unchanged,
    Made(Method),
}

/// Install `target` as an alias of `source`, trying each method in
/// [`DEFAULT_ORDER`].
pub fn install_alias(source: &Path, target: &Path) -> io::Result<Placed> {
    install_alias_with(source, target, &DEFAULT_ORDER)
}

/// [`install_alias`] restricted to `order`, so a test can force a fallback.
pub fn install_alias_with(source: &Path, target: &Path, order: &[Method]) -> io::Result<Placed> {
    let source = std::fs::canonicalize(source)?;
    if is_fresh(&source, target) {
        return Ok(Placed::Unchanged);
    }
    place(&source, target, order).map(Placed::Made)
}

/// Make `target` a fresh alias of `source` with the first method in `order`
/// that works, staged and renamed over `target`, with no freshness check.
/// [`crate::toolstore`] decides freshness itself (#2558).
pub fn place(source: &Path, target: &Path, order: &[Method]) -> io::Result<Method> {
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::other("alias path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let mut last_error = io::Error::other("no link method allowed");
    for &method in order {
        let staging = staging_path(dir, target);
        let _ = std::fs::remove_file(&staging);
        match stage(method, source, &staging).and_then(|()| std::fs::rename(&staging, target)) {
            Ok(()) => return Ok(method),
            Err(error) => {
                let _ = std::fs::remove_file(&staging);
                // A failed hardlink or symlink is routine: the next method
                // is the plan, not an error.
                last_error = error;
            }
        }
    }
    Err(last_error)
}

fn staging_path(dir: &Path, target: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn stage(method: Method, source: &Path, staging: &Path) -> io::Result<()> {
    match method {
        Method::Hardlink => std::fs::hard_link(source, staging),
        Method::Symlink => symlink(source, staging),
        Method::Copy => copy(source, staging),
    }
}

fn symlink(source: &Path, staging: &Path) -> io::Result<()> {
    crate::platform::alias_link::symlink(source, staging)
}

fn copy(source: &Path, staging: &Path) -> io::Result<()> {
    std::fs::copy(source, staging)?;
    crate::platform::shim_fs::set_mode(staging, 0o755)?;
    // Freshness compares size and mtime, so the copy carries the source's.
    if let Ok(modified) = std::fs::metadata(source).and_then(|meta| meta.modified()) {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .open(staging)
            .and_then(|file| file.set_modified(modified));
    }
    Ok(())
}

/// Whether `target` already is `source`: the same file, a symlink to it, or
/// a copy carrying its size and mtime. `source` should be canonical.
///
/// Size plus mtime is a *freshness* heuristic for copies this module made
/// (it sets the mtime itself). It is not an identity test: two unrelated
/// files written in the same instant can share both, so trust checks use
/// [`is_alias_of`].
pub fn is_fresh(source: &Path, target: &Path) -> bool {
    if is_alias_of(source, target) {
        return true;
    }
    let Ok(link_meta) = std::fs::symlink_metadata(target) else {
        return false;
    };
    if link_meta.file_type().is_symlink() {
        return false;
    }
    let (Ok(target_meta), Ok(source_meta)) = (std::fs::metadata(target), std::fs::metadata(source))
    else {
        return false;
    };
    target_meta.is_file()
        && target_meta.len() == source_meta.len()
        && matches!(
            (target_meta.modified(), source_meta.modified()),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Whether `candidate` is `source` itself: the same file (a hardlink) or a
/// symlink that resolves to it. A byte copy is not detected here.
pub fn is_alias_of(source: &Path, candidate: &Path) -> bool {
    same_file::is_same_file(source, candidate).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn source_in(dir: &Path, body: &[u8]) -> PathBuf {
        let path = dir.join("clud");
        fs::write(&path, body).unwrap();
        crate::platform::shim_fs::set_mode(&path, 0o755).unwrap();
        path
    }

    #[test]
    fn trust_recognizes_hardlinks_and_rejects_copies_with_matching_metadata() {
        let tmp = TempDir::new().unwrap();
        let source = source_in(tmp.path(), b"trusted");
        let hardlink = tmp.path().join("hardlink");
        install_alias_with(&source, &hardlink, &[Method::Hardlink]).unwrap();
        assert!(is_alias_of(&source, &hardlink));
        assert!(is_alias_of(&source, &source));
        let copied = tmp.path().join("copy");
        install_alias_with(&source, &copied, &[Method::Copy]).unwrap();
        assert!(is_fresh(&source, &copied));
        assert!(!is_alias_of(&source, &copied));
    }

    #[test]
    fn prefers_a_hardlink_and_is_then_unchanged() {
        let tmp = TempDir::new().unwrap();
        let source = source_in(tmp.path(), b"clud v1");
        let target = tmp.path().join("bin").join("clud-cmd-scan");
        assert_eq!(
            install_alias(&source, &target).unwrap(),
            Placed::Made(Method::Hardlink)
        );
        assert_eq!(fs::read(&target).unwrap(), b"clud v1");
        assert_eq!(install_alias(&source, &target).unwrap(), Placed::Unchanged);
    }

    #[test]
    fn falls_back_to_a_symlink_when_hardlinks_are_refused() {
        let tmp = TempDir::new().unwrap();
        let source = source_in(tmp.path(), b"clud v1");
        let target = tmp.path().join("rm");
        let placed = install_alias_with(&source, &target, &[Method::Symlink, Method::Copy]);
        match placed.unwrap() {
            // Windows without Developer Mode cannot symlink; the copy is the plan.
            Placed::Made(Method::Copy) if crate::platform::host::WINDOWS => {}
            Placed::Made(Method::Symlink) => {
                assert!(fs::symlink_metadata(&target)
                    .unwrap()
                    .file_type()
                    .is_symlink());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(fs::read(&target).unwrap(), b"clud v1");
        assert!(is_fresh(&fs::canonicalize(&source).unwrap(), &target));
    }

    #[test]
    fn copy_is_the_last_resort_and_carries_source_mtime() {
        let tmp = TempDir::new().unwrap();
        let source = source_in(tmp.path(), b"clud v1");
        let target = tmp.path().join("gh");
        assert_eq!(
            install_alias_with(&source, &target, &[Method::Copy]).unwrap(),
            Placed::Made(Method::Copy)
        );
        assert_eq!(fs::read(&target).unwrap(), b"clud v1");
        assert_eq!(
            install_alias_with(&source, &target, &[Method::Copy]).unwrap(),
            Placed::Unchanged,
            "a copy with the source's size and mtime is fresh"
        );
        if let Some(mode) = crate::platform::shim_fs::mode_bits(&fs::metadata(&target).unwrap()) {
            assert_eq!(mode & 0o111, 0o111);
        }
    }

    #[test]
    fn failing_every_method_is_the_only_error() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        assert!(install_alias(&missing, &tmp.path().join("rm")).is_err());
        let source = source_in(tmp.path(), b"clud");
        assert!(install_alias_with(&source, &tmp.path().join("rm"), &[]).is_err());
    }

    #[test]
    fn an_upgrade_refreshes_every_kind_of_alias() {
        let tmp = TempDir::new().unwrap();
        let source = source_in(tmp.path(), b"clud v1");
        let bin = tmp.path().join("bin");
        let kinds = [
            ("hard", Method::Hardlink),
            ("sym", Method::Symlink),
            ("copy", Method::Copy),
        ];
        for (name, method) in kinds {
            install_alias_with(&source, &bin.join(name), &[method, Method::Copy]).unwrap();
        }
        // The upgrade replaces `clud` with a new file (as the installer does).
        let replacement = tmp.path().join("clud.new");
        fs::write(&replacement, b"clud v2 is longer").unwrap();
        fs::rename(&replacement, &source).unwrap();
        let canonical = fs::canonicalize(&source).unwrap();
        for (name, method) in kinds {
            let alias = bin.join(name);
            // A symlink follows the replaced path, so it never goes stale.
            let stale_expected = !fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink();
            let _ = method;
            assert_eq!(!is_fresh(&canonical, &alias), stale_expected, "{name}");
            install_alias_with(&source, &alias, &[method, Method::Copy]).unwrap();
            assert!(is_fresh(&canonical, &alias), "{name} refreshed");
            assert_eq!(fs::read(&alias).unwrap(), b"clud v2 is longer", "{name}");
        }
    }

    #[test]
    fn a_replaced_alias_is_repaired() {
        let tmp = TempDir::new().unwrap();
        let source = source_in(tmp.path(), b"trusted");
        let target = tmp.path().join("rm");
        install_alias(&source, &target).unwrap();
        fs::remove_file(&target).unwrap();
        fs::write(&target, b"replaced").unwrap();
        assert_eq!(
            install_alias(&source, &target).unwrap(),
            Placed::Made(Method::Hardlink)
        );
        assert_eq!(fs::read(&target).unwrap(), b"trusted");
    }
}

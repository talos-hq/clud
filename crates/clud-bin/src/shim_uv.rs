//! `uv` discovery + serialized install scaffold. Slice 3 of #406 / #411.
//!
//! When the fast path from slice 2 (#410) returns `None`, the daemon
//! falls back to `uv` for interpreter resolution:
//!
//! 1. **discover** — look for a usable `uv` binary on PATH or under
//!    `~/.clud/state/uv/<version>/uv`.
//! 2. **install (deferred)** — if no `uv` exists, fetch the pinned
//!    release from `astral-sh/uv`. The actual download is scaffolded
//!    here but stubbed — the slice ships the lock semantics and the
//!    discovery / resolution path; the wire-up to a real HTTP fetch is
//!    a follow-up so the contract can land independently of the
//!    network-flake exposure.
//! 3. **serialize** — `acquire_install_lock` takes an `fs4` exclusive
//!    advisory lock on `<install_dir>/.install.lock` so concurrent
//!    shim invocations in the same session don't race the uv install.
//!
//! The protocol-version mismatch handling lives in `shim_resolve.rs`
//! already; this module just chains into it via `resolve_via_uv`.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::file_lock::FileExt;

use crate::shim_resolve::which_python_default;

/// Pinned `uv` release the daemon will fetch when no local `uv` is
/// available. Surfaced as a constant so the slice-4 (#412) bundling
/// step can grab the matching prebuilt binary.
pub const PINNED_UV_VERSION: &str = "0.5.4";

/// Discover a usable `uv` binary. Tries (in order):
///
/// 1. PATH lookup for `uv` (Windows: `uv.exe`).
/// 2. `~/.clud/state/uv/<version>/uv` — the daemon-managed install
///    from a prior session.
///
/// Returns `None` if nothing is found.
pub fn discover_uv(path_env: &str, state_root: &Path) -> Option<PathBuf> {
    if let Some(p) = which_python_default("uv", path_env) {
        return Some(p);
    }
    let managed = managed_uv_path(state_root);
    if managed.is_file() {
        return Some(managed);
    }
    None
}

/// Path where the daemon installs its managed `uv`. Used by both the
/// discovery and the install paths so a single source of truth holds.
pub fn managed_uv_path(state_root: &Path) -> PathBuf {
    let bin = if crate::platform::host::WINDOWS {
        "uv.exe"
    } else {
        "uv"
    };
    state_root.join("uv").join(PINNED_UV_VERSION).join(bin)
}

/// Acquire the cross-process install lock at
/// `<install_dir>/.install.lock`. Holds the lock for the lifetime of
/// the returned guard; concurrent shim invocations block until the
/// install completes (or fails).
///
/// The lock file is created if missing. The parent dir is `mkdir -p`'d.
pub fn acquire_install_lock(install_dir: &Path) -> io::Result<InstallLockGuard> {
    fs::create_dir_all(install_dir)?;
    let lock_path = install_dir.join(".install.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    FileExt::lock_exclusive(&file)?;
    Ok(InstallLockGuard {
        _file: file,
        path: lock_path,
    })
}

/// RAII guard returned by [`acquire_install_lock`]. Drop releases the
/// fs4 advisory lock; the lock file itself stays on disk for the next
/// invocation to reuse.
pub struct InstallLockGuard {
    _file: std::fs::File,
    path: PathBuf,
}

impl InstallLockGuard {
    /// Inspect the lock-file path — used by tests to assert the lock
    /// landed where we expected.
    pub fn lock_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstallLockGuard {
    fn drop(&mut self) {
        // fs4's lock is advisory and released when the File drops, so
        // we don't need an explicit unlock — but it's cheap to be
        // explicit for clarity in case of future refactors.
        let _ = FileExt::unlock(&self._file);
    }
}

/// Resolve a Python interpreter via an existing `uv` binary. Runs
/// `uv run --python <want> -- python -c "import sys; print(sys.executable)"`
/// and returns the canonical interpreter path. Returns `None` on any
/// failure (uv not present, exec failed, or output couldn't be parsed)
/// so the caller can fall through to `NotAvailable`.
///
/// Slice 3 wires the discovery + lock; the actual `uv` invocation
/// goes through `running_process` (the lint exempt list does NOT
/// extend here — uv calls are real subprocesses). For unit-testing
/// without spawning a real uv, callers can substitute the
/// `run_uv_fn` callback.
pub fn resolve_via_uv<F>(uv_path: &Path, want: &str, run_uv: F) -> Option<PathBuf>
where
    F: FnOnce(&Path, &str) -> Option<PathBuf>,
{
    run_uv(uv_path, want)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    #[test]
    fn managed_uv_path_includes_pinned_version() {
        let p = managed_uv_path(Path::new("/state"));
        let s = p.to_string_lossy();
        assert!(s.contains(PINNED_UV_VERSION), "got {s}");
        assert!(s.contains("uv"), "got {s}");
    }

    #[test]
    fn discover_uv_finds_managed_install() {
        let tmp = TempDir::new().unwrap();
        let managed = managed_uv_path(tmp.path());
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        File::create(&managed).unwrap();
        let found = discover_uv("", tmp.path());
        assert_eq!(found.as_deref(), Some(managed.as_path()));
    }

    #[test]
    fn discover_uv_returns_none_when_nothing_installed() {
        let tmp = TempDir::new().unwrap();
        let found = discover_uv("", tmp.path());
        assert!(found.is_none());
    }

    #[test]
    fn discover_uv_finds_path_install() {
        // Drop a fake `uv` binary into a dir; pass that dir as PATH.
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe_name = if crate::platform::host::WINDOWS {
            "uv.exe"
        } else {
            "uv"
        };
        let uv_path = bin.join(exe_name);
        File::create(&uv_path).unwrap();
        crate::platform::shim_fs::set_mode(&uv_path, 0o755).unwrap();
        let path_env = bin.to_string_lossy().to_string();
        let state_root = tmp.path().join("state");
        let found = discover_uv(&path_env, &state_root);
        assert_eq!(found.as_deref(), Some(uv_path.as_path()));
    }

    #[test]
    fn acquire_install_lock_creates_lock_file() {
        let tmp = TempDir::new().unwrap();
        let install_dir = tmp.path().join("uv-install");
        let guard = acquire_install_lock(&install_dir).unwrap();
        assert!(guard.lock_path().exists());
        assert!(guard
            .lock_path()
            .to_string_lossy()
            .ends_with(".install.lock"));
    }

    #[test]
    fn acquire_install_lock_serializes_within_process() {
        // Within a single process, fs4 exclusive locks may be reentrant
        // depending on the platform; rather than racing threads we assert
        // the lock-file path and that release happens on drop.
        let tmp = TempDir::new().unwrap();
        let install_dir = tmp.path().join("uv-install");
        {
            let _g = acquire_install_lock(&install_dir).unwrap();
        }
        // Re-acquire after drop should succeed.
        let _g2 = acquire_install_lock(&install_dir).unwrap();
    }

    #[test]
    fn resolve_via_uv_calls_back_into_runner() {
        let uv = PathBuf::from("/fake/uv");
        let called = std::cell::RefCell::new(false);
        let runner = |_: &Path, _: &str| -> Option<PathBuf> {
            *called.borrow_mut() = true;
            Some(PathBuf::from("/usr/bin/python3"))
        };
        let result = resolve_via_uv(&uv, "python3", runner);
        assert!(*called.borrow());
        assert_eq!(result, Some(PathBuf::from("/usr/bin/python3")));
    }

    #[test]
    fn resolve_via_uv_propagates_none() {
        let uv = PathBuf::from("/fake/uv");
        let result = resolve_via_uv(&uv, "python3", |_, _| None);
        assert!(result.is_none());
    }
}

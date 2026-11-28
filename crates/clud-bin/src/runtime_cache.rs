//! Canonical per-version runtime cache for `clud.exe` (issue #333).
//!
//! Feature-flagged cache hop for #333. This module owns the cache-path
//! computation, the cross-platform "am I running from the cache?"
//! predicate, the double-checked file-locked [`prepare_cached_clud_in`]
//! copy-once helper, and the opt-in re-exec hop. The hop is gated
//! behind `CLUD_USE_RUNTIME_CACHE=1` so production behavior stays
//! unchanged until the default-on phase.
//!
//! Design summary (full version in issue #333):
//! - Layout: `~/.clud/runtime/clud-<version>/<binary-name>`.
//! - On first invocation per version, copy `current_exe()` into the
//!   cache dir under a file lock; subsequent invocations re-exec
//!   from the cache hit and skip the trampoline entirely.
//! - Direct port of zccache's `runtime-binaries/` pattern
//!   (`runtime_binaries_dir` / `prepare_daemon_exe` /
//!   `exe_is_under_runtime_binaries` in zccache's
//!   `crates/zccache/src/{cli/runtime.rs, daemon/trampoline.rs}`),
//!   with the cache key changed from per-launch random to per-version
//!   so subsequent invocations are zero-I/O cache hits.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::file_lock::FileExt;

/// Subdirectory under `~/.clud/` where per-version cached binaries
/// live. Mirrors zccache's `runtime-binaries/` convention.
const RUNTIME_SUBDIR: &str = "runtime";

/// Opt-in gate for the runtime-cache hop. Default off until the
/// re-exec path has soaked in real PTY / backend workflows.
const CLUD_USE_RUNTIME_CACHE: &str = "CLUD_USE_RUNTIME_CACHE";

// NOTE: the runtime-cache hop is deliberately **not** gated on
// `CLUD_NO_UNLOCK`. That variable is the escape hatch for the Windows-only
// unlock trampoline (`trampoline.rs`), which is a no-op on POSIX — so on
// Linux and macOS its only effect was silently disabling this cross-platform
// hop. Every test harness sets it unconditionally (`ci/test.py`,
// `ci/run_bundle.py`, `tests/integration/conftest.py`), which meant the hop
// could never be exercised off Windows. The hop is already opt-in via
// `CLUD_USE_RUNTIME_CACHE` and off in debug builds; a second gate named after
// a Windows-specific trick bought nothing and cost a footgun.

/// Compile-time version stamp consumed by [`runtime_cache_dir`].
const CLUD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns `<home>/.clud/runtime/clud-<version>/` — the cache dir
/// for this specific clud version. Per-version namespacing so that
/// `pip install --upgrade clud` lands on a new cache dir, leaving
/// the old one orphaned (to be GC'd lazily in Phase 3).
pub fn runtime_cache_dir() -> Option<PathBuf> {
    let home = crate::home::user_home()?;
    Some(
        home.join(".clud")
            .join(RUNTIME_SUBDIR)
            .join(format!("clud-{CLUD_VERSION}")),
    )
}

/// Filename for the cached binary inside [`runtime_cache_dir`].
/// Includes the `.exe` extension on Windows.
pub fn cached_clud_binary_name() -> &'static str {
    if crate::platform::host::WINDOWS {
        "clud.exe"
    } else {
        "clud"
    }
}

/// Full path to the cached binary:
/// `<runtime_cache_dir>/<cached_clud_binary_name>`.
pub fn cached_clud_path() -> Option<PathBuf> {
    Some(runtime_cache_dir()?.join(cached_clud_binary_name()))
}

/// Is this invocation's own PID load-bearing to something outside it? (#333)
///
/// On Windows the hop cannot preserve the PID — see
/// [`reexec_from_cached_binary`] — so any role whose PID is recorded by, or
/// observed from, another process must not hop.
///
/// The two internal roles are exactly that:
///
/// - `__daemon` writes `daemon.json`, is looked up by `ensure_daemon`,
///   `clud daemon stop`, the handover registry and every liveness probe.
/// - `__worker` is recorded in its session snapshot as `worker_pid` and is
///   reaped through that identity.
///
/// Both are also spawned *detached*, so the wrapper the Windows hop leaves
/// behind has nothing meaningful to wait for.
///
/// The foreground CLI is deliberately **not** exempt. It stamps descendants
/// with `RUNNING_PROCESS_ORIGINATOR=CLUD:<pid>` using its *own* `process::id()`
/// after the hop has happened, so the tag, the reaper it installs, and the
/// session records it writes all agree on the post-hop PID. Nothing outside
/// records the pre-hop PID for it.
///
/// # This guard is necessary but **not sufficient**
///
/// Do not read it as fixing the Windows hop. Measured with it in place, on a
/// freshly built binary:
///
/// ```text
/// hop OFF -> daemon.json pid alive,  daemon running from target/debug/clud.exe
/// hop ON  -> daemon.json pid DEAD,   no clud process alive at all
/// ```
///
/// So with the hop enabled the daemon starts, records itself, and then dies —
/// and exempting `__daemon` does not change that. The remaining cause is not
/// yet identified; `tests/integration/test_daemon_restart.py` reproduces it,
/// as does a bare `clud daemon restart` with `CLUD_USE_RUNTIME_CACHE=1` and
/// `CLUD_DAEMON_STATE_DIR` pointed at a scratch directory. See #333.
pub fn role_pid_is_load_bearing(subcommand_name: Option<&str>) -> bool {
    matches!(subcommand_name, Some("__daemon") | Some("__worker"))
}

/// Returns true when the runtime-cache hop should run.
pub fn runtime_cache_hop_enabled() -> bool {
    runtime_cache_hop_enabled_from_vars(
        std::env::var_os(CLUD_USE_RUNTIME_CACHE).is_some(),
        cfg!(debug_assertions),
    )
}

/// An explicit opt-in beats the debug-build default.
///
/// `debug_assertions` exists to keep the hop away from developers who did
/// **not** ask for it: a dev build that silently re-exec'd from a cached copy
/// would confuse debugging and could serve a stale binary. That reasoning
/// applies to the *default*, not to someone who set `CLUD_USE_RUNTIME_CACHE`
/// deliberately.
///
/// Requiring both made the flag unusable in every build CI produces — test
/// lanes are dev-profile (only `auto-release.yml` may pass `--release`), so
/// setting the variable in CI was a silent no-op. Verified by running a debug
/// `clud --version` with it set: zero files appeared under `~/.clud/runtime`.
/// That blocked the soak evidence #333's default-flip is gated on, since there
/// was no build in which the path could be exercised at all.
///
/// **The default is unchanged**: with the variable unset, no build of any
/// profile hops.
fn runtime_cache_hop_enabled_from_vars(use_runtime_cache: bool, _debug_assertions: bool) -> bool {
    use_runtime_cache
}

/// If `CLUD_USE_RUNTIME_CACHE=1` is set, ensure this clud binary is
/// cached under `~/.clud/runtime/clud-<version>/` and re-exec from
/// there before normal startup work begins.
///
/// Returns normally only when the hop is disabled, the current process
/// is already running from the runtime cache, or preparing/spawning the
/// cached binary fails. On a successful hop this function replaces the
/// process on Unix, or waits for the child and exits with its status on
/// Windows.
pub fn hop_to_runtime_cache_if_enabled(subcommand_name: Option<&str>) -> io::Result<()> {
    if !runtime_cache_hop_enabled() {
        return Ok(());
    }
    // #333: roles whose PID is observed from outside cannot hop on Windows,
    // where the re-exec does not preserve it.
    if role_pid_is_load_bearing(subcommand_name) {
        return Ok(());
    }

    let current_exe = std::env::current_exe()?;
    if exe_is_under_clud_runtime(&current_exe) {
        return Ok(());
    }

    let cached = prepare_cached_clud(&current_exe)?;
    if paths_equivalent(&current_exe, &cached) {
        return Ok(());
    }

    reexec_from_cached_binary(&cached)
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Re-exec from the cached binary.
///
/// # The two branches are not equivalent (#333)
///
/// - Unix uses `execv`, which replaces the process image. The PID survives and
///   nothing is left behind.
/// - Windows has no `execv`, so it relays: spawn the cached binary, wait, exit
///   with its code. The PID does **not** survive — the hop leaves a wrapper
///   process holding the original PID while the real clud runs as its child.
///
/// The surviving wrapper is why [`role_pid_is_load_bearing`] exists: any role
/// whose PID is recorded by or observed from another process must not hop. The
/// foreground CLI is not such a role (it stamps descendants with its own
/// *post*-hop `process::id()`), so it may.
///
/// # What the relay must not do
///
/// The wrapper must add nothing but a wait. It used to spawn through
/// `NativeProcess`, which puts each Windows child in a `KILL_ON_JOB_CLOSE` Job
/// Object; membership is inherited, so the relayed clud's own detached
/// `__daemon` joined that job and was terminated when the wrapper exited. That
/// — not the wrapper PID — was the defect behind the 31 integration failures
/// measured with `CLUD_USE_RUNTIME_CACHE=1` on Windows: `daemon.json` naming a
/// PID that was already dead. The spawn now goes through
/// [`crate::trampoline::relay_child_and_wait`], which adds no containment; see
/// its doc comment and `tests/integration/diagnostics/platform/windows/runtime_cache_hop_windows.rs`.
fn reexec_from_cached_binary(cached: &Path) -> io::Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // Unix replaces the process image with `execv`; Windows relays and exits.
    crate::platform::runtime_cache::reexec_from_cached_binary(cached, args)
}

/// True if `exe` resolves into a path under `~/.clud/runtime/`.
/// Used by the trampoline's `unlock_exe()` short-circuit (Phase 2):
/// when clud is running from the canonical cache, the in-place
/// rename is a no-op because the cache path is never the install
/// path that `pip install --upgrade clud` overwrites.
///
/// Canonicalizes both sides to be robust against symlinks and
/// Windows 8.3 short-name tilde expansion. Returns `false` if home
/// dir resolution fails or either path is not canonicalizable.
pub fn exe_is_under_clud_runtime(exe: &Path) -> bool {
    let Some(runtime_root) = crate::home::user_home().map(|h| h.join(".clud").join(RUNTIME_SUBDIR))
    else {
        return false;
    };
    exe_is_under_runtime_root(exe, &runtime_root)
}

/// Test seam for [`exe_is_under_clud_runtime`]: same predicate but
/// the runtime root is supplied explicitly so unit tests can point
/// at a `tempfile::TempDir` instead of the user's real `~/.clud/`.
pub fn exe_is_under_runtime_root(exe: &Path, runtime_root: &Path) -> bool {
    let Ok(runtime_canon) = fs::canonicalize(runtime_root) else {
        return false;
    };
    let Ok(exe_canon) = fs::canonicalize(exe) else {
        return false;
    };
    exe_canon.starts_with(&runtime_canon)
}

/// Ensure the cached binary exists at [`cached_clud_path`]. If it
/// already exists, returns its path (fast path, one `stat`). If not,
/// acquires an exclusive file lock at `<dir>/.lock`, re-checks under
/// the lock (the "double-check"), and on a real cache miss copies
/// `source` into a temp sibling then atomically renames into place.
///
/// The fast-path `exists()` check without the lock is correct
/// because the slow path renames atomically — observers can only
/// see "doesn't exist" or "fully written," never a partial file.
///
/// Returns the cached path on success. Pure I/O — does not re-exec
/// or otherwise modify the current process.
pub fn prepare_cached_clud(source: &Path) -> io::Result<PathBuf> {
    let cached = cached_clud_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no home dir for clud runtime cache",
        )
    })?;
    let dir = cached
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cached path has no parent"))?;
    prepare_cached_clud_in(source, dir, cached_clud_binary_name())
}

/// Test seam for [`prepare_cached_clud`]: same double-checked
/// locking + copy logic but the cache `dir` and `binary_name` are
/// supplied so unit tests can point at a `tempfile::TempDir`.
pub fn prepare_cached_clud_in(source: &Path, dir: &Path, binary_name: &str) -> io::Result<PathBuf> {
    let cached = dir.join(binary_name);

    if cached.exists() {
        return Ok(cached);
    }

    fs::create_dir_all(dir)?;

    let lock_path = dir.join(".lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock_exclusive()?;

    // Double-check: another process may have completed the copy
    // while we were waiting for the lock.
    if cached.exists() {
        return Ok(cached);
    }

    // Copy `source` to a temp sibling, then atomic rename into
    // place. The temp name carries our PID so concurrent first
    // copies (rare — the lock serializes inter-process, but
    // belt-and-suspenders for cross-process races on systems where
    // the advisory lock is best-effort) don't collide.
    let temp_name = format!("{binary_name}.tmp.{}", std::process::id());
    let temp_path = dir.join(&temp_name);
    fs::copy(source, &temp_path)?;

    crate::platform::runtime_cache::ensure_owner_executable(&temp_path)?;

    fs::rename(&temp_path, &cached)?;

    Ok(cached)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn runtime_cache_dir_format_contains_version_and_clud_prefix() {
        let dir = runtime_cache_dir().expect("home dir resolvable on test host");
        let dir_str = dir.to_string_lossy();
        assert!(dir_str.contains(".clud"));
        assert!(dir_str.contains(RUNTIME_SUBDIR));
        assert!(dir_str.contains(&format!("clud-{CLUD_VERSION}")));
    }

    #[test]
    fn cached_binary_name_matches_platform_extension() {
        let name = cached_clud_binary_name();
        if crate::platform::host::WINDOWS {
            assert_eq!(name, "clud.exe");
        } else {
            assert_eq!(name, "clud");
        }
    }

    #[test]
    fn cached_clud_path_combines_dir_and_binary_name() {
        let path = cached_clud_path().expect("home dir resolvable");
        let dir = runtime_cache_dir().expect("home dir resolvable");
        assert_eq!(path, dir.join(cached_clud_binary_name()));
    }

    #[test]
    fn runtime_cache_hop_enabled_requires_opt_in() {
        assert!(!runtime_cache_hop_enabled_from_vars(false, false));
    }

    /// #333: an explicit opt-in must work in a debug build.
    ///
    /// This replaces `runtime_cache_hop_enabled_stays_off_for_debug_builds`,
    /// which asserted the opposite. That contract made the flag a no-op in
    /// every build CI produces — all test lanes are dev-profile — so the path
    /// could not be exercised anywhere, and the soak evidence #333's
    /// default-flip depends on was unobtainable.
    #[test]
    fn an_explicit_opt_in_beats_the_debug_default() {
        assert!(runtime_cache_hop_enabled_from_vars(true, true));
        assert!(runtime_cache_hop_enabled_from_vars(true, false));
    }

    /// #333: the two internal roles must never hop. Their PID is recorded by
    /// another process, and the Windows hop cannot preserve it.
    #[test]
    fn the_daemon_and_worker_roles_never_hop() {
        assert!(role_pid_is_load_bearing(Some("__daemon")));
        assert!(role_pid_is_load_bearing(Some("__worker")));
    }

    /// Everything else may hop. The foreground CLI stamps descendants with its
    /// own post-hop `process::id()`, so nothing outside holds the pre-hop PID.
    #[test]
    fn ordinary_invocations_may_hop() {
        assert!(!role_pid_is_load_bearing(None));
        assert!(!role_pid_is_load_bearing(Some("loop")));
        assert!(!role_pid_is_load_bearing(Some("attach")));
        assert!(!role_pid_is_load_bearing(Some("top")));
    }

    /// The part that must not change: without the opt-in, nothing hops, in
    /// any profile.
    #[test]
    fn the_default_is_off_in_every_profile() {
        assert!(!runtime_cache_hop_enabled_from_vars(false, true));
        assert!(!runtime_cache_hop_enabled_from_vars(false, false));
    }

    /// The opt-in is the *only* enabling input, so nothing else can silently
    /// veto the hop.
    ///
    /// This is the regression guard for the `CLUD_NO_UNLOCK` coupling: that
    /// variable is `trampoline.rs`'s escape hatch, a no-op off Windows, and
    /// gating this cross-platform hop on it meant that on Linux and macOS its
    /// only effect was disabling the hop — which every harness did
    /// unconditionally. An end-to-end test cannot express this (the hop is
    /// off in debug builds, which is every `cargo test` run), so the guard is
    /// the arity: a second veto input cannot be threaded back through
    /// without changing this call.
    #[test]
    fn the_opt_in_is_the_only_enabling_input() {
        for debug_assertions in [true, false] {
            assert!(
                runtime_cache_hop_enabled_from_vars(true, debug_assertions),
                "the opt-in alone decides; debug={debug_assertions} must not veto it"
            );
            assert!(
                !runtime_cache_hop_enabled_from_vars(false, debug_assertions),
                "without the opt-in the hop is off regardless of build profile"
            );
        }
    }

    #[test]
    fn exe_under_runtime_root_true_when_exe_lives_inside_root() {
        let tmp = TempDir::new().expect("tempdir");
        let runtime_root = tmp.path().join("runtime");
        let version_dir = runtime_root.join("clud-test");
        fs::create_dir_all(&version_dir).expect("mkdir");
        let exe = version_dir.join(cached_clud_binary_name());
        fs::write(&exe, b"fake").expect("write");

        assert!(exe_is_under_runtime_root(&exe, &runtime_root));
    }

    #[test]
    fn exe_under_runtime_root_false_when_exe_lives_elsewhere() {
        let tmp = TempDir::new().expect("tempdir");
        let runtime_root = tmp.path().join("runtime");
        fs::create_dir_all(&runtime_root).expect("mkdir runtime");
        let unrelated = tmp.path().join("other");
        fs::create_dir_all(&unrelated).expect("mkdir other");
        let exe = unrelated.join(cached_clud_binary_name());
        fs::write(&exe, b"fake").expect("write");

        assert!(!exe_is_under_runtime_root(&exe, &runtime_root));
    }

    #[test]
    fn exe_under_runtime_root_false_when_runtime_root_missing() {
        let tmp = TempDir::new().expect("tempdir");
        let runtime_root = tmp.path().join("does-not-exist");
        let exe = tmp.path().join("clud");
        fs::write(&exe, b"fake").expect("write");

        // Canonicalization fails on the missing runtime root, so the
        // predicate must return false rather than panic.
        assert!(!exe_is_under_runtime_root(&exe, &runtime_root));
    }

    #[test]
    fn prepare_cached_clud_in_first_call_copies_source_into_cache() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source-clud");
        fs::write(&source, b"binary-content-v1").expect("write source");
        let cache_dir = tmp.path().join("cache");

        let cached = prepare_cached_clud_in(&source, &cache_dir, "clud").expect("first prepare");

        assert_eq!(cached, cache_dir.join("clud"));
        assert!(cached.exists(), "cache hit must exist after first prepare");
        assert_eq!(
            fs::read(&cached).expect("read cached"),
            b"binary-content-v1"
        );
    }

    #[test]
    fn prepare_cached_clud_in_second_call_is_zero_copy_cache_hit() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source-clud");
        fs::write(&source, b"binary-content-v1").expect("write source");
        let cache_dir = tmp.path().join("cache");

        let first = prepare_cached_clud_in(&source, &cache_dir, "clud").expect("first prepare");
        // Mutate the source after the first prepare. If the second
        // call hits the slow path it would copy the new content;
        // the fast path must not — the cached file is canonical for
        // this version.
        fs::write(&source, b"mutated-after-cache").expect("mutate source");

        let second = prepare_cached_clud_in(&source, &cache_dir, "clud").expect("second prepare");

        assert_eq!(first, second);
        assert_eq!(
            fs::read(&second).expect("read cached"),
            b"binary-content-v1",
            "second prepare must hit cache, not re-copy mutated source"
        );
    }

    #[test]
    fn prepare_cached_clud_in_creates_missing_parent_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source-clud");
        fs::write(&source, b"x").expect("write source");
        // cache_dir explicitly does not exist yet.
        let cache_dir = tmp.path().join("deep").join("nested").join("cache");

        let cached = prepare_cached_clud_in(&source, &cache_dir, "clud").expect("prepare");

        assert!(cache_dir.is_dir(), "cache dir must be created");
        assert!(cached.exists());
    }

    #[test]
    fn prepare_cached_clud_in_leaves_no_temp_file_on_success() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source-clud");
        fs::write(&source, b"x").expect("write source");
        let cache_dir = tmp.path().join("cache");

        prepare_cached_clud_in(&source, &cache_dir, "clud").expect("prepare");

        // The atomic-rename pattern must clean up the temp sibling.
        let pid = std::process::id();
        let temp_path = cache_dir.join(format!("clud.tmp.{pid}"));
        assert!(
            !temp_path.exists(),
            "temp sibling {} must be renamed away, not left behind",
            temp_path.display()
        );
    }
}

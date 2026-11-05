//! Per-user shim extraction. Slice 4 of #406 / #412.
//!
//! Materializes `clud` itself under `~/.clud/state/shims/` as the alias names
//! downstream tooling will invoke (`python`, `python3`, `python.exe`,
//! `python3.exe`, `rm`, ...). `clud` is multicall (#1551): each alias is a
//! hardlink, symlink or copy of the running `clud` (see
//! [`crate::alias_link`]) that [`crate::multicall`] routes to the shim by
//! argv[0]. The shim dir is **per-user**, shared across all sessions and
//! concurrent clud processes for that user; an upgrade replaces `clud`, and
//! the next launch relinks every alias that no longer resolves to it.
//!
//! Startup resolves the real Python before rewriting PATH and exports that
//! target for the shim to execute without recursive lookup.

use std::path::{Path, PathBuf};

use crate::alias_link;
use crate::shim_registry::{self, ShimDir, ShimKind};

/// Subdirectory under the user's home where the interpreter aliases live.
/// Joined to the resolved home by [`shims_dir`].
pub const SHIMS_SUBDIR: &str = shim_registry::INTERPRETER_SUBDIR;

/// Alias filenames to install under [`SHIMS_SUBDIR`], derived from
/// [`shim_registry::SHIMS`]. Each alias is a link to (or copy of) `clud`,
/// which dispatches on argv\[0\].
pub fn alias_names() -> Vec<String> {
    shim_registry::file_names_in(ShimDir::Interpreter)
}

/// `~/.clud/state/shims/`. Returns `None` when the user's home dir
/// cannot be resolved — callers degrade silently (the launch path
/// just skips shim install).
pub fn shims_dir() -> Option<PathBuf> {
    crate::home::user_home().map(|h| h.join(SHIMS_SUBDIR))
}

/// Testable variant — install all aliases under `home_root` as links to
/// `shim_source` (the running `clud`). Returns the count of aliases created
/// or refreshed.
///
/// If `shim_source` does not exist, returns 0 — the session can still
/// proceed; the agent's `python` invocation just won't route through the
/// shim. This is the deliberate graceful-fallback behavior the no-daemon case
/// relies on. A failed hardlink or symlink falls back to a copy; only a
/// failed copy is an error.
pub fn extract_shims_at(home_root: &Path, shim_source: &Path) -> std::io::Result<usize> {
    let shims_dir = home_root.join(SHIMS_SUBDIR);
    std::fs::create_dir_all(&shims_dir)?;
    if !shim_source.is_file() {
        return Ok(0);
    }
    // The pre-#1551 byte-hash sentinel; freshness is now file identity.
    let _ = std::fs::remove_file(shims_dir.join(".shim-hash"));
    purge_retired_aliases(&shims_dir);
    let mut installed = 0;
    for alias in alias_names() {
        let target = shims_dir.join(&alias);
        if let alias_link::Placed::Made(_) =
            crate::toolstore::install_owned(home_root, shim_source, &target)?
        {
            installed += 1;
        }
    }
    Ok(installed)
}

/// Install the interpreter aliases and make them authoritative for one child
/// environment. The real interpreter is resolved before PATH is rewritten so
/// the shim can never select itself recursively.
pub fn prepare_session_shims_at(
    home_root: &Path,
    current_exe: &Path,
    env: &mut Vec<(String, String)>,
) -> std::io::Result<bool> {
    if !is_clud_exe(current_exe) || !current_exe.is_file() {
        return Ok(false);
    }
    let source = current_exe;
    let inherited_target = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(shim_registry::PYTHON_TARGET_KEY))
        .map(|(_, value)| PathBuf::from(value))
        .filter(|path| path.is_file());
    let target = match inherited_target {
        Some(path) => path,
        None => {
            let path = env
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
                .map(|(_, value)| value.as_str())
                .unwrap_or("");
            let python = shim_registry::SHIMS
                .iter()
                .find(|spec| spec.kind == ShimKind::Python)
                .map_or_else(String::new, |spec| spec.name.to_string());
            let Some(path) = crate::shim_resolve::which_python_default(&python, path) else {
                return Ok(false);
            };
            path
        }
    };

    extract_shims_at(home_root, source)?;
    let installed = home_root.join(SHIMS_SUBDIR);
    crate::shim_session::prepend_to_path(env, &installed);
    crate::shim_session::set_env(
        env,
        shim_registry::PYTHON_TARGET_KEY,
        &target.to_string_lossy(),
    );
    crate::shim_session::set_env(env, shim_registry::ABI_KEY, shim_registry::SHIM_ABI);
    Ok(true)
}

/// Best-effort startup wiring for the current process. Called after the Ctrl+C
/// handler is installed so a cold-HOME shim extract cannot delay SIGINT
/// delivery, but before the backend is spawned so every launch path inherits
/// the same aliases.
pub fn prepare_current_session() -> std::io::Result<bool> {
    let Some(home) = crate::home::user_home() else {
        return Ok(false);
    };
    let current_exe = std::env::current_exe()?;
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    if !prepare_session_shims_at(&home, &current_exe, &mut env)? {
        return Ok(false);
    }
    for key in [
        "PATH",
        shim_registry::PYTHON_TARGET_KEY,
        shim_registry::ABI_KEY,
    ] {
        if let Some((_, value)) = env.iter().find(|(candidate, _)| candidate == key) {
            // SAFETY: startup-only write to the shim session keys, before the backend
            // is spawned and before any thread that reads the environment runs.
            unsafe { std::env::set_var(key, value) };
        }
    }
    Ok(true)
}

/// Whether `exe` is `clud` itself (any case, `.exe` or not) and so a valid
/// alias source. A test harness or another program is never one.
fn is_clud_exe(exe: &Path) -> bool {
    shim_registry::invoked_name(exe.as_os_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("clud"))
}

/// The running `clud`: the trusted alias source, never resolved through PATH
/// or an env override. Errors when the process is not `clud` (a unit-test
/// harness, say), so nothing links that.
pub fn packaged_shim() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if !is_clud_exe(&exe) {
        return Err(std::io::Error::other(format!(
            "{} is not the clud executable",
            exe.display()
        )));
    }
    Ok(exe)
}

/// The session aliases, derived from [`shim_registry::SHIMS`]: `rm`,
/// `safe-rm` (#1461) and `gh` (#1518) today. All are links to (or copies of)
/// `clud`, which dispatches on argv\[0\].
pub fn rm_alias_names() -> Vec<String> {
    shim_registry::file_names_in(ShimDir::Session)
}

/// Alias names clud no longer serves. An install made them in its own alias
/// dirs, and nothing relinks or removes a name the registry dropped, so each
/// stayed behind as a full copy of an old build (the two deletion helpers
/// retired by #1461, were 115 MB apiece; #1954). Append a name when the
/// registry drops it.
pub const RETIRED_ALIASES: &[&str] = &["rm-file", "rm-dir"]; // legacy-deletion-purge: allow

/// Remove [`RETIRED_ALIASES`] from one of clud's own alias dirs. Best effort.
fn purge_retired_aliases(dir: &Path) {
    for name in RETIRED_ALIASES {
        let _ = crate::rm_tool::remove_link_or_file(&dir.join(shim_registry::file_name(name)));
    }
}

fn purge_stale_aliases(dir: &Path, expected: &[String]) -> std::io::Result<()> {
    purge_retired_aliases(dir);
    // Only known legacy aliases are ours to remove. In particular, do not
    // delete another concurrent installer's NamedTempFile before it persists.
    for alias in alias_names() {
        if expected.contains(&alias) {
            continue;
        }
        let target = dir.join(&alias);
        if std::fs::symlink_metadata(&target).is_ok() {
            if let Err(error) = crate::rm_tool::remove_link_or_file(&target) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

/// Session activation installs only the active aliases, keeping unfinished
/// Python relays off PATH. A separate directory also avoids activating
/// previously extracted Python aliases.
pub fn install_rm_at(home: &Path, source: &Path) -> std::io::Result<PathBuf> {
    install_rm_with(home, source, &crate::toolstore::OWNED_ORDER, &|_| true)
        .map(|installed| installed.unwrap_or_else(|| home.join(shim_registry::SESSION_SUBDIR)))
}

/// [`install_rm_at`] with the link methods to try and a precondition checked
/// on the alias dir under the install lock; `Ok(None)` when it refused.
fn install_rm_with(
    home: &Path,
    source: &Path,
    order: &[alias_link::Method],
    proceed: &dyn Fn(&Path) -> bool,
) -> std::io::Result<Option<PathBuf>> {
    use fs4::fs_std::FileExt;
    use std::fs::OpenOptions;
    use std::sync::Mutex;

    static LOCAL_INSTALL_LOCK: Mutex<()> = Mutex::new(());
    let _local_guard = LOCAL_INSTALL_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let state = home.join(".clud/state");
    std::fs::create_dir_all(&state)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(state.join(".rm-shim.lock"))?;
    FileExt::lock_exclusive(&lock)?;
    if std::fs::metadata(source)?.len() == 0 {
        return Err(std::io::Error::other("packaged shim is empty"));
    }
    let dir = home.join(shim_registry::SESSION_SUBDIR);
    if !proceed(&dir) {
        return Ok(None);
    }
    std::fs::create_dir_all(&dir)?;
    let expected = rm_alias_names();
    purge_stale_aliases(&dir, &expected)?;
    for name in expected {
        // A stale or replaced entry is renamed over atomically, so concurrent
        // installers never see a partial alias and never write through one.
        crate::toolstore::install_owned_with(home, source, &dir.join(name), order)?;
    }
    Ok(Some(dir))
}

/// Refresh the shared session aliases from a `clud` started inside a
/// running session (#1743). A session keeps its launch-time alias dir, so
/// after an upgrade its `gh`, `git` and `rm` run the old binary until the
/// next launch relinks them. The session's statusline, hooks and `clud tool`
/// calls run the installed `clud` by path, so `main` calls this on every CLI
/// start and the first one after an upgrade relinks the dir. Best effort and
/// silent: a failure leaves the dir for the next launch.
pub fn refresh_running_session_aliases() {
    let (Some(home), Ok(exe)) = (crate::home::user_home(), std::env::current_exe()) else {
        return;
    };
    let var = |key: &str| std::env::var_os(key);
    let _ = refresh_running_session_aliases_at(&home, &exe, &var);
}

/// [`refresh_running_session_aliases`] for an injected env; `Ok(true)` when it
/// relinked. It acts only when every gate holds:
///
/// - `current_exe` is the session's own installed `clud` (`CLUD_EXE`), so a
///   dev build or another install run by hand never takes over the dir;
/// - the session's `CLUD_SHIM_ABI` is this binary's, so the relinked aliases
///   keep validating this session instead of failing open;
/// - the session's alias dir is the shared one under `home`, and exists;
/// - some alias is stale, and none is newer than `current_exe` (by mtime), so
///   two installs never take the dir back and forth.
///
/// Only hardlinks and symlinks are tried: a failure is cheap to retry on the
/// next call, unlike a full copy of `clud`. Each alias is still replaced by
/// rename under the install lock, never truncated in place.
pub fn refresh_running_session_aliases_at(
    home: &Path,
    current_exe: &Path,
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> std::io::Result<bool> {
    if !is_clud_exe(current_exe)
        || var(shim_registry::ABI_KEY).as_deref()
            != Some(std::ffi::OsStr::new(shim_registry::SHIM_ABI))
    {
        return Ok(false);
    }
    let canonical = |path: PathBuf| std::fs::canonicalize(path).ok();
    let Some(source) = canonical(current_exe.to_path_buf()) else {
        return Ok(false);
    };
    if var(shim_registry::CLUD_EXE_KEY)
        .map(PathBuf::from)
        .and_then(canonical)
        .as_ref()
        != Some(&source)
    {
        return Ok(false);
    }
    let dir = home.join(shim_registry::SESSION_SUBDIR);
    let Some(shared) = canonical(dir.clone()) else {
        return Ok(false);
    };
    if var(shim_registry::SESSION_DIR_KEY)
        .map(PathBuf::from)
        .and_then(canonical)
        .as_ref()
        != Some(&shared)
    {
        return Ok(false);
    }
    let source_modified = std::fs::metadata(&source)?.modified()?;
    let behind = |dir: &Path| aliases_behind(home, dir, &source, source_modified);
    if !behind(&dir) {
        return Ok(false);
    }
    // Checked again under the lock: a newer launch may have relinked since.
    let installed = install_rm_with(
        home,
        &source,
        &[alias_link::Method::Symlink, alias_link::Method::Hardlink],
        &behind,
    )?;
    Ok(installed.is_some())
}

/// Whether some session alias in `dir` is not `source` and none of those is
/// as new as `source` (mtime): the dir belongs to an older install.
fn aliases_behind(
    home: &Path,
    dir: &Path,
    source: &Path,
    source_modified: std::time::SystemTime,
) -> bool {
    let mut stale = false;
    for name in rm_alias_names() {
        let alias = dir.join(name);
        if crate::toolstore::is_current(home, source, &alias) {
            continue;
        }
        stale = true;
        if std::fs::metadata(&alias)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified >= source_modified)
        {
            return false;
        }
    }
    stale
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_source(content: &[u8]) -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("clud");
        fs::write(&src, content).unwrap();
        (tmp, src)
    }

    /// Replace `path` the way an upgrade does: a new file renamed over it, so
    /// hardlinked aliases keep pointing at the old inode.
    fn replace_file(path: &Path, content: &[u8]) {
        let staged = path.with_extension("new");
        fs::write(&staged, content).unwrap();
        fs::rename(&staged, path).unwrap();
    }

    /// #1954: the retired deletion helpers outlived their retirement as old-build
    /// copies in both alias dirs, because only current names were managed.
    #[test]
    fn retired_aliases_are_purged_from_both_alias_dirs() {
        let (tmp, src) = make_source(b"clud");
        let home = tmp.path().join("home");
        let shims = home.join(SHIMS_SUBDIR);
        let session = home.join(shim_registry::SESSION_SUBDIR);
        for dir in [&shims, &session] {
            fs::create_dir_all(dir).unwrap();
            for name in RETIRED_ALIASES {
                fs::write(dir.join(shim_registry::file_name(name)), b"old build").unwrap();
            }
        }
        extract_shims_at(&home, &src).unwrap();
        install_rm_at(&home, &src).unwrap();
        for dir in [&shims, &session] {
            for name in RETIRED_ALIASES {
                let path = dir.join(shim_registry::file_name(name));
                assert!(!path.exists(), "{} survived", path.display());
            }
        }
    }

    /// Replace an alias the way an attacker or a bad tool would: unlink and
    /// write a different file.
    fn swap_alias(path: &Path, content: &[u8]) {
        fs::remove_file(path).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn repairs_replaced_bytes_even_with_matching_sentinel() {
        let home = TempDir::new().unwrap();
        let (_source_dir, source) = make_source(b"trusted");
        extract_shims_at(home.path(), &source).unwrap();
        let target = home.path().join(SHIMS_SUBDIR).join(&alias_names()[0]);
        swap_alias(&target, b"replaced");
        assert_eq!(extract_shims_at(home.path(), &source).unwrap(), 1);
        assert_eq!(fs::read(target).unwrap(), b"trusted");
    }

    #[test]
    fn session_installs_only_active_aliases_and_repairs_replacement() {
        let home = TempDir::new().unwrap();
        let (_source_dir, source) = make_source(b"trusted");
        let dir = install_rm_at(home.path(), &source).unwrap();
        for name in rm_alias_names() {
            let target = dir.join(&name);
            swap_alias(&target, b"replacement");
            install_rm_at(home.path(), &source).unwrap();
            assert_eq!(fs::read(&target).unwrap(), b"trusted", "{name}");
        }
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let mut expected: Vec<String> = rm_alias_names().iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(names, expected, "no Python relay is activated");
    }

    #[test]
    fn concurrent_session_installs_repair_gh_without_deleting_staging_files() {
        let home = TempDir::new().unwrap();
        let (_source_dir, source) = make_source(b"trusted");
        let dir = install_rm_at(home.path(), &source).unwrap();
        let gh = dir.join(shim_registry::file_name("gh"));
        swap_alias(&gh, b"replacement");
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| install_rm_at(home.path(), &source)))
                .collect();
            for worker in workers {
                worker.join().unwrap().unwrap();
            }
        });
        assert_eq!(fs::read(gh).unwrap(), b"trusted");
    }

    #[test]
    fn a_process_that_is_not_clud_links_nothing() {
        let home = TempDir::new().unwrap();
        let other = home
            .path()
            .join(shim_registry::file_name("some-test-harness"));
        fs::write(&other, b"x").unwrap();
        let mut env = vec![("PATH".to_string(), String::new())];
        assert!(!prepare_session_shims_at(home.path(), &other, &mut env).unwrap());
        assert!(!home.path().join(SHIMS_SUBDIR).exists());
    }

    #[test]
    fn an_upgraded_clud_refreshes_every_session_alias() {
        let home = TempDir::new().unwrap();
        let (_source_dir, source) = make_source(b"v1");
        let dir = install_rm_at(home.path(), &source).unwrap();
        replace_file(&source, b"v2 is different");
        install_rm_at(home.path(), &source).unwrap();
        for name in rm_alias_names() {
            assert_eq!(
                fs::read(dir.join(&name)).unwrap(),
                b"v2 is different",
                "{name}"
            );
        }
    }

    /// The env a running session hands every `clud` it starts: its own
    /// installed `clud`, the ABI stamp and the shared alias dir.
    fn running_session(
        home: &Path,
        clud: &Path,
    ) -> std::collections::HashMap<&'static str, std::ffi::OsString> {
        std::collections::HashMap::from([
            (shim_registry::CLUD_EXE_KEY, clud.as_os_str().to_owned()),
            (shim_registry::ABI_KEY, shim_registry::SHIM_ABI.into()),
            (
                shim_registry::SESSION_DIR_KEY,
                home.join(shim_registry::SESSION_SUBDIR).into_os_string(),
            ),
        ])
    }

    fn refresh(
        home: &Path,
        clud: &Path,
        vars: &std::collections::HashMap<&'static str, std::ffi::OsString>,
    ) -> bool {
        let var = |key: &str| vars.get(key).cloned();
        refresh_running_session_aliases_at(home, clud, &var).unwrap()
    }

    /// The alias resolves to `source`'s toolstore build (#2558).
    fn links_to(home: &Path, source: &Path, alias: &Path) -> bool {
        crate::toolstore::is_current(home, source, alias)
    }

    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    /// An upgraded `clud`, run from inside a session launched by an older
    /// one (its statusline, a hook, `clud tool`), relinks the shared alias
    /// dir so that session's `gh`/`rm` run current code (#1743).
    #[test]
    fn an_upgraded_clud_refreshes_a_running_sessions_aliases() {
        let home = TempDir::new().unwrap();
        let bin = TempDir::new().unwrap();
        let clud = bin.path().join(shim_registry::file_name("clud"));
        fs::write(&clud, b"v1").unwrap();
        let dir = install_rm_at(home.path(), &clud).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        set_mtime(&clud, old);
        replace_file(&clud, b"v2 is different");
        let vars = running_session(home.path(), &clud);

        assert!(refresh(home.path(), &clud, &vars));
        for name in rm_alias_names() {
            assert!(links_to(home.path(), &clud, &dir.join(&name)), "{name}");
        }
        assert!(!refresh(home.path(), &clud, &vars), "fresh aliases stay");
    }

    #[test]
    fn a_running_session_refresh_never_downgrades_or_strays() {
        let home = TempDir::new().unwrap();
        let bin = TempDir::new().unwrap();
        let clud = bin.path().join(shim_registry::file_name("clud"));
        let newer = bin
            .path()
            .join("newer")
            .join(shim_registry::file_name("clud"));
        fs::create_dir_all(newer.parent().unwrap()).unwrap();
        fs::write(&clud, b"older install").unwrap();
        fs::write(&newer, b"newer install").unwrap();
        set_mtime(
            &clud,
            std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
        );
        let dir = install_rm_at(home.path(), &newer).unwrap();
        let alias = dir.join(&rm_alias_names()[0]);
        let vars = running_session(home.path(), &clud);
        assert!(
            !refresh(home.path(), &clud, &vars),
            "a newer install's aliases are not replaced by an older clud"
        );
        assert!(links_to(home.path(), &newer, &alias));

        // Make `clud` the newest so only the gate under test can refuse.
        set_mtime(
            &newer,
            std::time::SystemTime::now() - std::time::Duration::from_secs(7200),
        );
        replace_file(&clud, b"newest install");
        let session = || running_session(home.path(), &clud);
        let mut foreign_exe = session();
        foreign_exe.insert(shim_registry::CLUD_EXE_KEY, newer.clone().into_os_string());
        let mut other_abi = session();
        other_abi.insert(shim_registry::ABI_KEY, "0".into());
        let mut no_abi = session();
        no_abi.remove(shim_registry::ABI_KEY);
        let mut other_dir = session();
        other_dir.insert(shim_registry::SESSION_DIR_KEY, bin.path().into());
        for (label, vars) in [
            ("not this session's installed clud", foreign_exe),
            (
                "another shim ABI: the refreshed alias would fail open",
                other_abi,
            ),
            ("no ABI stamp", no_abi),
            ("a session dir that is not the shared one", other_dir),
        ] {
            assert!(!refresh(home.path(), &clud, &vars), "{label}");
            assert!(links_to(home.path(), &newer, &alias), "{label}");
        }
        assert!(
            refresh(home.path(), &clud, &running_session(home.path(), &clud)),
            "the control: every gate open"
        );
        assert!(links_to(home.path(), &clud, &alias));

        let empty = TempDir::new().unwrap();
        let vars = running_session(empty.path(), &clud);
        assert!(!refresh(empty.path(), &clud, &vars));
        assert!(
            !empty.path().join(shim_registry::SESSION_SUBDIR).exists(),
            "no session ever ran here: nothing is created"
        );
    }

    #[test]
    fn alias_names_are_platform_appropriate() {
        let names = alias_names();
        assert!(!names.is_empty());
        if crate::platform::host::WINDOWS {
            assert!(names.iter().any(|n| n.ends_with(".exe")));
        } else {
            assert!(names.iter().all(|n| !n.ends_with(".exe")));
        }
    }

    #[test]
    fn extract_creates_aliases() {
        let home = TempDir::new().unwrap();
        let (_src_dir, src) = make_source(b"#!fake shim binary\n");
        let count = extract_shims_at(home.path(), &src).unwrap();
        assert_eq!(count, alias_names().len());
        let shims = home.path().join(SHIMS_SUBDIR);
        for alias in alias_names() {
            assert!(shims.join(&alias).is_file(), "missing alias {alias}");
        }
        assert!(!shims.join(".shim-hash").exists(), "no byte-hash sentinel");
    }

    #[test]
    fn extract_is_noop_when_hash_matches() {
        let home = TempDir::new().unwrap();
        let (_src_dir, src) = make_source(b"fake shim v1");
        // First pass: writes all aliases.
        assert_eq!(
            extract_shims_at(home.path(), &src).unwrap(),
            alias_names().len()
        );
        // Second pass with identical source: nothing should change.
        assert_eq!(extract_shims_at(home.path(), &src).unwrap(), 0);
    }

    #[test]
    fn extract_refreshes_when_source_changes() {
        let home = TempDir::new().unwrap();
        let (src_dir, src) = make_source(b"v1");
        extract_shims_at(home.path(), &src).unwrap();
        // Upgrade: a new `clud` replaces the old one.
        replace_file(&src, b"v2-different");
        let count = extract_shims_at(home.path(), &src).unwrap();
        assert_eq!(
            count,
            alias_names().len(),
            "all aliases should refresh on drift"
        );
        // Confirm new bytes landed.
        let shims = home.path().join(SHIMS_SUBDIR);
        let first_alias = shims.join(&alias_names()[0]);
        assert_eq!(fs::read(&first_alias).unwrap(), b"v2-different");
        let _ = src_dir; // keep tmpdir alive
    }

    #[test]
    fn extract_no_op_when_source_missing() {
        let home = TempDir::new().unwrap();
        let nonexistent = home.path().join("not-there");
        let count = extract_shims_at(home.path(), &nonexistent).unwrap();
        assert_eq!(count, 0);
        // Dir should still be created — the launch path can populate
        // it later when the bundled binary becomes available.
        assert!(home.path().join(SHIMS_SUBDIR).is_dir());
    }

    #[test]
    fn shims_dir_resolves_under_home() {
        let resolved = shims_dir();
        // Best-effort: if home exists, the path should end with the suffix.
        if let Some(p) = resolved {
            let s = p.to_string_lossy();
            assert!(s.ends_with("shims") || s.contains("shims"), "got {s}");
        }
    }

    #[test]
    fn prepare_session_shims_extracts_aliases_and_prepends_path() {
        let home = TempDir::new().unwrap();
        let bin = home.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let current_exe = bin.join(shim_registry::file_name("clud"));
        let python = bin.join(shim_registry::file_name("python3"));
        fs::write(&current_exe, b"clud").unwrap();
        fs::write(&python, b"python").unwrap();
        crate::platform::shim_fs::set_mode(&python, 0o755).unwrap();
        let mut env = vec![("PATH".to_string(), bin.to_string_lossy().into_owned())];

        let prepared = prepare_session_shims_at(home.path(), &current_exe, &mut env).unwrap();

        assert!(prepared);
        let installed = home.path().join(SHIMS_SUBDIR);
        for alias in alias_names() {
            assert!(installed.join(&alias).is_file(), "missing {alias}");
        }
        let path = env
            .iter()
            .find(|(key, _)| key == "PATH")
            .unwrap()
            .1
            .as_str();
        assert!(
            path.starts_with(installed.to_string_lossy().as_ref()),
            "got {path}"
        );
        assert_eq!(
            env.iter()
                .find(|(key, _)| key == shim_registry::PYTHON_TARGET_KEY)
                .map(|(_, value)| value.as_str()),
            Some(python.to_string_lossy().as_ref())
        );
        assert!(
            env.iter()
                .any(|(key, value)| key == shim_registry::ABI_KEY
                    && value == shim_registry::SHIM_ABI),
            "the prepared session carries the shim ABI stamp"
        );

        assert!(prepare_session_shims_at(home.path(), &current_exe, &mut env).unwrap());
        assert_eq!(
            env.iter()
                .find(|(key, _)| key == shim_registry::PYTHON_TARGET_KEY)
                .map(|(_, value)| value.as_str()),
            Some(python.to_string_lossy().as_ref()),
            "a nested clud launch must not resolve the alias back to itself"
        );
    }
}

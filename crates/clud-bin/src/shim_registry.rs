//! The one list of `clud-shim` aliases, and the session contract they share
//! (#1546).
//!
//! Every alias name the `clud-shim` personality of `clud` answers to
//! ([`crate::multicall`], #1551) is one [`ShimSpec`] row in
//! [`SHIMS`]. The single dispatch path (`shim_main.rs`, module
//! `dispatch`), the alias installers ([`crate::shim_install`]) and the child
//! env policy ([`crate::shim_session::activate_rm`]) all derive from it, so a
//! new shim is one row here plus one handler arm, which the compiler forces
//! through the exhaustive [`ShimKind`] match.
//!
//! The contract: a `Passthrough` shim runs its clud behavior only inside a
//! valid session, meaning [`ABI_KEY`] equals [`SHIM_ABI`] and the shim's own
//! target keys are present and sane. Otherwise it execs the next same-named
//! binary on PATH after the shim directories ([`next_on_path`]). The alias
//! directories are shared by every live session of every installed clud
//! version, so a session started by an older clud routinely meets a newer
//! alias; the ABI stamp is what makes that meeting fail open instead of
//! failing closed. See `docs/architecture/shim-dispatch.md`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Session stamp: the shim ABI the launching clud activated. Absent or
/// different means the env was built by another clud version, and every
/// `Passthrough` shim relays to the real binary.
pub const ABI_KEY: &str = "CLUD_SHIM_ABI";
/// Bump when a session key changes meaning. Adding a shim or a key does not
/// need a bump: an older session simply lacks the new key and passes through.
pub const SHIM_ABI: &str = "1";
/// The session alias directory; also re-prepended by the generated BASH_ENV
/// after a login shell resets PATH.
pub const SESSION_DIR_KEY: &str = "CLUD_RM_SHIM_DIR";
/// Real `python` the launching clud resolved before PATH was rewritten.
pub const PYTHON_TARGET_KEY: &str = "CLUD_PYTHON_SHIM_TARGET";
/// Real `gh` the launching clud resolved before PATH was rewritten.
pub const GH_TARGET_KEY: &str = "CLUD_GH_SHIM_TARGET";
/// ghd's variable naming the real gh (forge-ci/ghd#24). The session exports
/// it so a direct `ghd gh` never relays back into the `gh` alias.
pub const GHD_REAL_GH_KEY: &str = "GHD_REAL_GH";
/// `ghd` when the session's `gh.backend` routes gh through ghd (#2352), else
/// `gh`. The bundled watchers send plain REST reads under `ghd` so ghd can
/// share them across concurrent watchers (#2368).
pub const GH_BACKEND_KEY: &str = "CLUD_GH_BACKEND";
/// Real `git` the launching clud resolved before PATH was rewritten (#1486).
pub const GIT_TARGET_KEY: &str = "CLUD_GIT_SHIM_TARGET";
/// `1` when the `gh` alias has a target; read by the command guard.
pub const GH_ACTIVE_KEY: &str = "CLUD_GH_SHIM_ACTIVE";
/// `0` turns the `gh pr checks --watch` upgrade off.
pub const GH_FAIL_FAST_KEY: &str = "CLUD_GH_SHIM_FAIL_FAST";
/// The clud executable the `gh` watch upgrade runs the bundled watcher with.
pub const CLUD_EXE_KEY: &str = "CLUD_EXE";
/// `1` routes in-session `gh api` GETs through the daemon's read broker
/// (#1743, `git.gh_read_broker`). Any other value leaves every `gh` call on
/// the real binary. Absent (a session launched before the key) defers to the
/// current setting (DD-152).
pub const GH_READ_BROKER_KEY: &str = "CLUD_GH_READ_BROKER";
/// The daemon state dir whose `daemon.json` names the broker's listener.
pub const DAEMON_STATE_DIR_KEY: &str = "CLUD_DAEMON_STATE_DIR";
/// The clud session id, recorded in the broker's ledger.
pub const SESSION_ID_KEY: &str = "CLUD_SESSION_ID";
/// `1` makes a brokered read skip its TTL (it still revalidates).
pub const GH_FRESH_KEY: &str = "CLUD_GH_FRESH";

/// Every session key a shim reads. The guard test in `shim_main.rs`
/// refuses these names anywhere in the shim personality outside its `dispatch` module.
pub const SESSION_KEYS: &[&str] = &[
    ABI_KEY,
    SESSION_DIR_KEY,
    PYTHON_TARGET_KEY,
    GH_TARGET_KEY,
    GH_ACTIVE_KEY,
    GH_FAIL_FAST_KEY,
    GH_READ_BROKER_KEY,
    GIT_TARGET_KEY,
];

/// Interpreter aliases, prepared by `shim_install::prepare_current_session`.
pub const INTERPRETER_SUBDIR: &str = ".clud/state/shims";
/// Session aliases, installed by `shim_install::install_rm_at`.
pub const SESSION_SUBDIR: &str = ".clud/state/rm-shim";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimKind {
    Python,
    Gh,
    /// In-session `git`: a pass-through that records one telemetry line per
    /// invocation (#1486).
    Git,
    Rm,
    SafeRm,
    /// `safe-mktemp` (#1667): the only creation-ledger writer.
    SafeMktemp,
}

/// What a shim does without a valid session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Exec the next same-named binary on PATH.
    Passthrough,
    /// The shim is a clud command with its own out-of-session mode.
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimDir {
    Interpreter,
    Session,
}

impl ShimDir {
    pub fn subdir(self) -> &'static str {
        match self {
            ShimDir::Interpreter => INTERPRETER_SUBDIR,
            ShimDir::Session => SESSION_SUBDIR,
        }
    }
}

#[derive(Debug)]
pub struct ShimSpec {
    pub name: &'static str,
    pub kind: ShimKind,
    /// Keys the dispatch path reads to validate this shim's session.
    pub session_keys: &'static [&'static str],
    pub fallback: Fallback,
    /// Alias directories this name is installed into.
    pub dirs: &'static [ShimDir],
}

pub const SHIMS: &[ShimSpec] = &[
    ShimSpec {
        name: "python",
        kind: ShimKind::Python,
        session_keys: &[ABI_KEY, PYTHON_TARGET_KEY],
        fallback: Fallback::Passthrough,
        dirs: &[ShimDir::Interpreter],
    },
    ShimSpec {
        name: "python3", // python-name-lint: allow (the alias clud installs)
        kind: ShimKind::Python,
        session_keys: &[ABI_KEY, PYTHON_TARGET_KEY],
        fallback: Fallback::Passthrough,
        dirs: &[ShimDir::Interpreter],
    },
    ShimSpec {
        name: "gh",
        kind: ShimKind::Gh,
        session_keys: &[ABI_KEY, GH_TARGET_KEY, GH_FAIL_FAST_KEY, GH_READ_BROKER_KEY],
        fallback: Fallback::Passthrough,
        dirs: &[ShimDir::Session],
    },
    ShimSpec {
        name: "git",
        kind: ShimKind::Git,
        session_keys: &[ABI_KEY, GIT_TARGET_KEY],
        fallback: Fallback::Passthrough,
        dirs: &[ShimDir::Session],
    },
    ShimSpec {
        name: "rm",
        kind: ShimKind::Rm,
        session_keys: &[ABI_KEY, SESSION_DIR_KEY],
        fallback: Fallback::Passthrough,
        dirs: &[ShimDir::Interpreter, ShimDir::Session],
    },
    ShimSpec {
        name: crate::rm_tool::COMMAND,
        kind: ShimKind::SafeRm,
        session_keys: &[],
        fallback: Fallback::Native,
        dirs: &[ShimDir::Interpreter, ShimDir::Session],
    },
    ShimSpec {
        name: crate::safe_mktemp::COMMAND,
        kind: ShimKind::SafeMktemp,
        session_keys: &[],
        fallback: Fallback::Native,
        dirs: &[ShimDir::Interpreter, ShimDir::Session],
    },
];

/// The row for an invoked name: argv\[0\]'s file name, with `.exe` and case
/// ignored so Windows and POSIX spell a shim the same way (NTFS is
/// case-insensitive, so `RM.EXE` must reach the `rm` row).
pub fn lookup(argv0: &OsStr) -> Option<&'static ShimSpec> {
    let name = invoked_name(argv0)?;
    SHIMS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(&name))
}

/// argv\[0\]'s file name without a trailing `.exe`, split on either
/// separator so a Windows path parses the same on every host.
pub fn invoked_name(argv0: &OsStr) -> Option<String> {
    let raw = argv0.to_str()?;
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let lower = base.to_ascii_lowercase();
    let name = if lower.ends_with(".exe") {
        &base[..base.len() - 4]
    } else {
        base
    };
    (!name.is_empty()).then(|| name.to_string())
}

/// The on-disk file name for `name` on this platform.
pub fn file_name(name: &str) -> String {
    if crate::platform::host::WINDOWS {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// File names installed into `dir`, in registry order.
pub fn file_names_in(dir: ShimDir) -> Vec<String> {
    SHIMS
        .iter()
        .filter(|spec| spec.dirs.contains(&dir))
        .map(|spec| file_name(spec.name))
        .collect()
}

/// Directories whose entries are clud aliases, never real binaries: the
/// shim's own directory, the session's alias directory, and both managed
/// directories under `home`.
pub fn shim_dirs(self_exe: &Path, session_dir: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // `clud` itself lives beside unrelated programs (a venv's `python`), so
    // its directory is never an alias directory. An alias that is a hardlink
    // or copy has its own name, and its directory is one.
    if !is_clud_name(self_exe) {
        if let Some(parent) = canonical(self_exe).parent() {
            dirs.push(parent.to_path_buf());
        }
    }
    dirs.extend(session_dir.map(canonical));
    if let Some(home) = home {
        for dir in [ShimDir::Interpreter, ShimDir::Session] {
            dirs.push(canonical(&home.join(dir.subdir())));
        }
    }
    dirs.dedup();
    dirs
}

/// Whether `target` can stand in for the real binary: absolute, an
/// executable file, outside every shim directory, and not a copy of the
/// running shim.
pub fn valid_target(target: &Path, self_exe: &Path, shim_dirs: &[PathBuf]) -> bool {
    if !target.is_absolute() || !executable(target) {
        return false;
    }
    let resolved = canonical(target);
    let in_shim_dir = resolved
        .parent()
        .is_some_and(|parent| shim_dirs.iter().any(|dir| dir == parent))
        || target
            .parent()
            .is_some_and(|parent| shim_dirs.iter().any(|dir| *dir == canonical(parent)));
    !in_shim_dir && !is_self(&resolved, self_exe)
}

/// Issue #1486: the real `git` / `gh` for clud's own spawns, so they bypass
/// the session's telemetry alias (no recorded line, no extra hop).
///
/// `name` must be `git` or `gh`; anything else is `None`. The target is the
/// session's `CLUD_GIT_SHIM_TARGET` / `CLUD_GH_SHIM_TARGET`, resolved by the
/// launching clud before the alias directory went on PATH, and is used only
/// when it is absolute, an existing file, and outside every alias
/// directory. Otherwise `None`, and the caller spawns the bare name: outside
/// a session nothing is shimmed, and a session without a valid target runs
/// the alias as a plain passthrough.
pub fn real_program(name: &str) -> Option<PathBuf> {
    real_program_with(
        name,
        &|key| std::env::var_os(key),
        crate::home::user_home().as_deref(),
    )
}

/// [`real_program`] over injected env and home, for tests.
pub fn real_program_with(
    name: &str,
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let key = match name {
        "git" => GIT_TARGET_KEY,
        "gh" => GH_TARGET_KEY,
        _ => return None,
    };
    let target = PathBuf::from(var(key)?);
    if !target.is_absolute() || !target.is_file() {
        return None;
    }
    let session_dir = var(SESSION_DIR_KEY).map(PathBuf::from);
    let mut alias_dirs: Vec<PathBuf> = session_dir.iter().map(|dir| canonical(dir)).collect();
    if let Some(home) = home {
        for dir in [ShimDir::Interpreter, ShimDir::Session] {
            alias_dirs.push(canonical(&home.join(dir.subdir())));
        }
    }
    let parent = target.parent().map(canonical);
    if parent.is_some_and(|parent| alias_dirs.contains(&parent)) {
        return None;
    }
    Some(target)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextOnPathError {
    /// Strict mode only: the shim's directory is not on PATH.
    ShimDirMissing,
    NotFound,
}

/// The next executable named `file_name` on `path_env`, skipping every
/// directory in `shim_dirs` and any copy of `self_exe`.
///
/// The search starts after the first PATH entry that is the shim's own
/// directory, as a shell's lookup would have continued. When that entry is
/// absent (the shim was run by absolute path), `strict` reports
/// [`NextOnPathError::ShimDirMissing`] and non-strict searches all of PATH.
/// Each hop moves strictly later on PATH and never selects a shim, so a
/// chain of shims cannot loop. The returned path keeps the PATH spelling,
/// so a multi-call binary (BusyBox's `rm` symlink) sees its applet name.
pub fn next_on_path(
    file_name: &str,
    path_env: &OsStr,
    self_exe: &Path,
    shim_dirs: &[PathBuf],
    strict: bool,
) -> Result<PathBuf, NextOnPathError> {
    let own_dir = (!is_clud_name(self_exe))
        .then(|| canonical(self_exe).parent().map(Path::to_path_buf))
        .flatten();
    let entries: Vec<PathBuf> = std::env::split_paths(path_env).collect();
    // A symlink alias runs as `clud` (its own directory is not the alias
    // directory), so the first shim directory on PATH also marks the start.
    let start = entries.iter().position(|dir| {
        let dir = canonical(dir);
        own_dir.as_deref() == Some(dir.as_path()) || shim_dirs.contains(&dir)
    });
    let start = match (start, strict) {
        (Some(index), _) => index + 1,
        (None, true) => return Err(NextOnPathError::ShimDirMissing),
        (None, false) => 0,
    };
    scan(&entries[start..], file_name, self_exe, shim_dirs).ok_or(NextOnPathError::NotFound)
}

/// The first executable named `file_name` anywhere on `path_env`, with the
/// same exclusions as [`next_on_path`]. For resolving a real binary before
/// the alias directory is on PATH.
pub fn first_on_path(
    file_name: &str,
    path_env: &OsStr,
    self_exe: &Path,
    shim_dirs: &[PathBuf],
) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = std::env::split_paths(path_env).collect();
    scan(&entries, file_name, self_exe, shim_dirs)
}

fn scan(
    entries: &[PathBuf],
    file_name: &str,
    self_exe: &Path,
    shim_dirs: &[PathBuf],
) -> Option<PathBuf> {
    for dir in entries {
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            continue;
        }
        if shim_dirs.iter().any(|shim| *shim == canonical(dir)) {
            continue;
        }
        let candidate = dir.join(file_name);
        if executable(&candidate) && !is_self(&canonical(&candidate), self_exe) {
            return Some(candidate);
        }
    }
    None
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_self(resolved: &Path, self_exe: &Path) -> bool {
    let me = canonical(self_exe);
    resolved == me || crate::alias_link::is_alias_of(&me, resolved) || same_bytes(resolved, &me)
}

/// Whether the running executable is `clud` itself rather than one of its
/// aliases, judged by file name (`clud`, `clud.exe`, any case).
fn is_clud_name(exe: &Path) -> bool {
    invoked_name(exe.as_os_str()).is_some_and(|name| name.eq_ignore_ascii_case("clud"))
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(am), Ok(bm)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    am.len() == bm.len() && std::fs::read(a).ok() == std::fs::read(b).ok()
}

pub fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        // A Windows App Execution Alias (the Store `python.exe`) is a reparse
        // point that cannot be followed, yet CreateProcess runs it.
        return crate::platform::host::WINDOWS
            && path.symlink_metadata().is_ok_and(|meta| !meta.is_dir());
    };
    if !metadata.is_file() {
        return false;
    }
    crate::platform::shim_fs::has_exec_bit(&metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use tempfile::TempDir;

    fn write_exe(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        crate::platform::shim_fs::set_mode(path, 0o755).unwrap();
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn names_are_unique_and_lookup_ignores_exe_and_directories() {
        let mut names: Vec<_> = SHIMS.iter().map(|spec| spec.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), SHIMS.len());
        for spec in SHIMS {
            for argv0 in [
                spec.name.to_string(),
                format!("{}.exe", spec.name),
                format!("{}.EXE", spec.name),
                format!("/a/b/{}", spec.name),
                format!("C:\\a\\{}.exe", spec.name),
            ] {
                assert_eq!(lookup(OsStr::new(&argv0)).map(|s| s.name), Some(spec.name));
            }
        }
        assert!(lookup(OsStr::new("clud-shim")).is_none());
        assert_eq!(
            lookup(OsStr::new("RM.EXE")).map(|s| s.name),
            Some("rm"),
            "NTFS is case-insensitive"
        );
    }

    #[test]
    fn every_safe_alias_the_deletion_rules_install_is_registered() {
        for alias in crate::deletion_rules::generated().safe_aliases {
            let spec = SHIMS.iter().find(|spec| spec.name == alias);
            assert_eq!(spec.map(|s| s.kind), Some(ShimKind::SafeRm), "{alias}");
        }
    }

    #[test]
    fn only_clud_commands_have_a_native_mode() {
        for spec in SHIMS {
            assert_eq!(
                spec.fallback == Fallback::Native,
                matches!(spec.kind, ShimKind::SafeRm | ShimKind::SafeMktemp),
                "{}",
                spec.name
            );
        }
    }

    /// #1486: `git` and `gh` are session aliases (`.exe` on Windows).
    #[test]
    fn git_and_gh_are_registered_session_aliases() {
        for name in ["git", "gh"] {
            let spec = lookup(OsStr::new(name)).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(spec.name, name);
            assert!(spec.dirs.contains(&ShimDir::Session), "{name}");
            assert!(
                file_names_in(ShimDir::Session).contains(&file_name(name)),
                "{name}"
            );
            let windows = format!(r"C:\u\.clud\state\rm-shim\{name}.EXE");
            assert_eq!(lookup(OsStr::new(&windows)).map(|s| s.name), Some(name));
        }
        assert_eq!(
            file_name("git").ends_with(".exe"),
            crate::platform::host::WINDOWS
        );
    }

    /// #1486 acceptance 5: a target inside an alias directory is never
    /// used, so clud's own spawns cannot recurse into the shim.
    #[test]
    fn real_program_uses_only_a_target_outside_the_alias_dirs() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let session = home.join(SESSION_SUBDIR);
        let alias = session.join(file_name("git"));
        let real = temp.path().join("usr").join(file_name("git"));
        write_exe(&alias, b"alias");
        write_exe(&real, b"real");
        let vars = |pairs: Vec<(&'static str, PathBuf)>| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.clone().into_os_string())
            }
        };
        let good = vars(vec![
            (GIT_TARGET_KEY, real.clone()),
            (SESSION_DIR_KEY, session.clone()),
        ]);
        assert_eq!(
            real_program_with("git", &good, Some(&home)),
            Some(real.clone())
        );
        assert_eq!(real_program_with("gh", &good, Some(&home)), None);
        assert_eq!(real_program_with("rm", &good, Some(&home)), None);
        let recursive = vars(vec![
            (GIT_TARGET_KEY, alias.clone()),
            (SESSION_DIR_KEY, session),
        ]);
        assert_eq!(
            real_program_with("git", &recursive, Some(&home)),
            None,
            "a target inside the alias dir would recurse into the shim"
        );
        let relative = vars(vec![(GIT_TARGET_KEY, PathBuf::from("git"))]);
        assert_eq!(real_program_with("git", &relative, Some(&home)), None);
        let gone = vars(vec![(GIT_TARGET_KEY, temp.path().join("missing"))]);
        assert_eq!(real_program_with("git", &gone, Some(&home)), None);
        let gh = vars(vec![(GH_TARGET_KEY, real.clone())]);
        assert_eq!(real_program_with("gh", &gh, None), Some(real));
    }

    /// Session key names are spelled once, here. Every module that produces
    /// or reads shim session state goes through the constants, so the guard in
    /// `shim_main.rs` can find each reader by name. The sources are
    /// compiled in: CI runs this test from a bundle with no source tree.
    #[test]
    fn session_key_literals_live_only_in_the_registry() {
        let sources = [
            ("shim_session.rs", include_str!("shim_session.rs")),
            ("shim_install.rs", include_str!("shim_install.rs")),
            ("runner.rs", include_str!("runner.rs")),
            ("shell/nounset.rs", include_str!("shell/nounset.rs")),
            ("block_bad_cmd.rs", include_str!("block_bad_cmd.rs")),
            ("rm_guard.rs", include_str!("rm_guard.rs")),
            ("rm_tool.rs", include_str!("rm_tool.rs")),
            ("shim_main.rs", include_str!("shim_main.rs")),
            (
                "shim_main/dispatch.rs",
                include_str!("shim_main/dispatch.rs"),
            ),
        ];
        for (file, text) in sources {
            let production = text.split("#[cfg(test)]").next().unwrap();
            for key in SESSION_KEYS {
                assert!(
                    !production.contains(&format!("\"{key}\"")),
                    "{file} spells {key}; use crate::shim_registry"
                );
            }
        }
    }

    #[test]
    fn every_session_key_a_row_reads_is_a_known_session_key() {
        for spec in SHIMS {
            for key in spec.session_keys {
                assert!(SESSION_KEYS.contains(key), "{} reads {key}", spec.name);
            }
            if spec.fallback == Fallback::Passthrough {
                assert!(spec.session_keys.contains(&ABI_KEY), "{}", spec.name);
            }
        }
    }

    #[test]
    fn next_on_path_skips_the_shim_dir_and_copies_of_the_shim() {
        let temp = TempDir::new().unwrap();
        let shim = temp.path().join("shim").join(file_name("gh"));
        let copy = temp.path().join("copy").join(file_name("gh"));
        let real = temp.path().join("real").join(file_name("gh"));
        write_exe(&shim, b"shim");
        write_exe(&copy, b"shim");
        write_exe(&real, b"real");
        let dirs = shim_dirs(&shim, None, None);
        let path = path_of(&[
            real.parent().unwrap(),
            shim.parent().unwrap(),
            copy.parent().unwrap(),
            real.parent().unwrap(),
        ]);
        assert_eq!(
            next_on_path(&file_name("gh"), &path, &shim, &dirs, false),
            Ok(real.clone()),
            "the entry after the shim, not the one before it"
        );
        let only_shims = path_of(&[shim.parent().unwrap(), copy.parent().unwrap()]);
        assert_eq!(
            next_on_path(&file_name("gh"), &only_shims, &shim, &dirs, false),
            Err(NextOnPathError::NotFound)
        );
    }

    #[test]
    fn strict_mode_requires_the_shim_dir_on_path() {
        let temp = TempDir::new().unwrap();
        let shim = temp.path().join("shim").join(file_name("rm"));
        let real = temp.path().join("real").join(file_name("rm"));
        write_exe(&shim, b"shim");
        write_exe(&real, b"real");
        let path = path_of(&[real.parent().unwrap()]);
        assert_eq!(
            next_on_path(&file_name("rm"), &path, &shim, &[], true),
            Err(NextOnPathError::ShimDirMissing)
        );
        assert_eq!(
            next_on_path(&file_name("rm"), &path, &shim, &[], false),
            Ok(real)
        );
    }

    #[test]
    fn a_target_inside_a_shim_dir_or_equal_to_the_shim_is_invalid() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let shim = home.join(SESSION_SUBDIR).join(file_name("gh"));
        let python_alias = home.join(INTERPRETER_SUBDIR).join(file_name("gh"));
        let copy = temp.path().join("copy").join(file_name("gh"));
        let real = temp.path().join("real").join(file_name("gh"));
        write_exe(&shim, b"shim");
        write_exe(&python_alias, b"other shim version");
        write_exe(&copy, b"shim");
        write_exe(&real, b"real");
        let dirs = shim_dirs(&shim, None, Some(&home));
        assert!(valid_target(&real, &shim, &dirs));
        for bad in [&shim, &python_alias, &copy] {
            assert!(!valid_target(bad, &shim, &dirs), "{}", bad.display());
        }
        assert!(!valid_target(Path::new("relative/gh"), &shim, &dirs));
        assert!(!valid_target(&temp.path().join("missing"), &shim, &dirs));
    }
}

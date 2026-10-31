//! Session-launch env for the `clud-shim` aliases.
//!
//! [`activate_rm`] is the one child-env layer that installs the session
//! aliases, puts their directory first on PATH, and exports the session keys
//! the shims validate. The key names and alias list come from
//! [`crate::shim_registry`]; the out-of-session contract (fail open to the
//! next real binary) is in `docs/architecture/shim-dispatch.md`.

use std::path::Path;

use crate::shim_registry;

/// Env var name for `PATH`. Same on Unix and Windows (Windows is
/// case-insensitive but uppercase is conventional).
pub const PATH_ENV_VAR: &str = "PATH";
/// Read by the generated BASH_ENV file after a login shell resets PATH.
pub const RM_SHIM_DIR_KEY: &str = shim_registry::SESSION_DIR_KEY;
pub const GH_SHIM_TARGET_KEY: &str = shim_registry::GH_TARGET_KEY;
pub const GH_SHIM_ACTIVE_KEY: &str = shim_registry::GH_ACTIVE_KEY;
pub const GH_SHIM_FAIL_FAST_KEY: &str = shim_registry::GH_FAIL_FAST_KEY;
/// #1743: `git.gh_read_broker`, exported for the `gh` alias.
pub const GH_READ_BROKER_KEY: &str = shim_registry::GH_READ_BROKER_KEY;
/// #1486: the real `git`, for the `git` alias and clud's own spawns.
pub const GIT_SHIM_TARGET_KEY: &str = shim_registry::GIT_TARGET_KEY;
/// Git's terminal-prompt switch. An agent cannot answer a terminal prompt, so
/// a credential failure must error instead of waiting (#1546).
pub const GIT_TERMINAL_PROMPT_KEY: &str = "GIT_TERMINAL_PROMPT";
/// Set to empty when the caller did not choose one. Git then skips askpass
/// instead of falling back to `SSH_ASKPASS`, whose GUI dialog blocked agent
/// pushes when the credential helper failed (#1546). `SSH_ASKPASS` itself is
/// left alone for ssh's own key unlock.
pub const GIT_ASKPASS_KEY: &str = "GIT_ASKPASS";

/// Prepend `dir` to the PATH entry in `env` (or create a new entry if
/// PATH isn't set). Idempotent: re-injection doesn't duplicate the
/// prepended dir.
pub fn prepend_to_path(env: &mut Vec<(String, String)>, dir: &Path) -> bool {
    let dir_str = dir.to_string_lossy().into_owned();
    let sep = path_sep();
    for (key, value) in env.iter_mut() {
        if key.eq_ignore_ascii_case(PATH_ENV_VAR) {
            if value.split(sep).next() == Some(dir_str.as_str()) {
                return false; // already present
            }
            *value = value
                .split(sep)
                .filter(|p| *p != dir_str)
                .collect::<Vec<_>>()
                .join(&sep.to_string());
            let new = if value.is_empty() {
                dir_str.clone()
            } else {
                format!("{dir_str}{sep}{value}")
            };
            *value = new;
            return true;
        }
    }
    // PATH not in env at all — create it.
    env.push((PATH_ENV_VAR.to_string(), dir_str));
    true
}

/// Set `key=value` in `env`, replacing any existing entry with the
/// same key (case-insensitive match — Windows env vars are
/// case-insensitive and clud aims to behave consistently).
/// A direct `ghd gh` in the session resolves `gh` on PATH to the alias, which
/// routes back to ghd: a relay loop. Name the real gh unless the user already
/// did.
fn set_ghd_real_gh(env: &mut Vec<(String, String)>, target: &Path) {
    let key = shim_registry::GHD_REAL_GH_KEY;
    if !env.iter().any(|(k, _)| k.eq_ignore_ascii_case(key)) {
        set_env(env, key, &target.to_string_lossy());
    }
}

pub fn set_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    for (k, v) in env.iter_mut() {
        if k.eq_ignore_ascii_case(key) {
            *v = value.to_string();
            return;
        }
    }
    env.push((key.to_string(), value.to_string()));
}

fn path_sep() -> char {
    if crate::platform::host::WINDOWS {
        ';'
    } else {
        ':'
    }
}

/// Both foreground and daemon call this after assembling the effective child env.
/// Installation errors remain visible and the mandatory hook identity check denies.
pub fn activate_rm(env: &mut Vec<(String, String)>) {
    set_env(env, GIT_TERMINAL_PROMPT_KEY, "0");
    if !env
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case(GIT_ASKPASS_KEY))
    {
        set_env(env, GIT_ASKPASS_KEY, "");
    }
    let fail_fast = crate::clud_settings::load_pr_wait_fail_fast_enabled().unwrap_or(true);
    set_env(
        env,
        GH_SHIM_FAIL_FAST_KEY,
        if fail_fast { "1" } else { "0" },
    );
    let read_broker = crate::clud_settings::load_gh_read_broker_enabled().unwrap_or(true);
    set_env(env, GH_READ_BROKER_KEY, if read_broker { "1" } else { "0" });
    set_soldr_porcelain(env, launch_soldr_porcelain());
    set_env(env, GH_SHIM_ACTIVE_KEY, "0");
    let original_path = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(PATH_ENV_VAR))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let inherited = |wanted: &str| {
        env.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| std::path::PathBuf::from(value))
    };
    let inherited_target = inherited(GH_SHIM_TARGET_KEY);
    let inherited_git = inherited(GIT_SHIM_TARGET_KEY);
    let home_key = if crate::platform::host::WINDOWS {
        "USERPROFILE"
    } else {
        "HOME"
    };
    let result = env
        .iter()
        .find(|(k, _)| k == home_key)
        .ok_or_else(|| std::io::Error::other("missing session home"))
        .and_then(|(_, home)| {
            crate::shim_install::packaged_shim()
                .and_then(|source| crate::shim_install::install_rm_at(Path::new(home), &source))
        });
    match result {
        Ok(dir) => {
            let home = env
                .iter()
                .find(|(k, _)| k == home_key)
                .map(|(_, home)| std::path::PathBuf::from(home));
            let target = resolve_gh_target(&original_path, inherited_target, &dir, home.as_deref());
            if let Some(target) = target {
                let target = target.canonicalize().unwrap_or(target);
                set_env(env, GH_SHIM_TARGET_KEY, &target.to_string_lossy());
                set_env(env, GH_SHIM_ACTIVE_KEY, "1");
                set_ghd_real_gh(env, &target);
                set_env(env, shim_registry::GH_BACKEND_KEY, launch_gh_backend());
            } else {
                set_env(env, GH_SHIM_ACTIVE_KEY, "0");
            }
            // #1486: the real git, resolved before the prepend like gh's. No
            // target leaves the `git` alias a plain passthrough.
            match resolve_target("git", &original_path, inherited_git, &dir, home.as_deref()) {
                Some(target) => set_env(env, GIT_SHIM_TARGET_KEY, &target.to_string_lossy()),
                None => env.retain(|(key, _)| !key.eq_ignore_ascii_case(GIT_SHIM_TARGET_KEY)),
            }
            prepend_to_path(env, &dir);
            set_env(env, RM_SHIM_DIR_KEY, &dir.to_string_lossy());
            set_env(env, shim_registry::ABI_KEY, shim_registry::SHIM_ABI);
        }
        Err(error) => {
            eprintln!("[clud rm shim] installation failed; shell identity guard will deny: {error}")
        }
    }
}

/// soldr's agent mode: one JSON line on success, a failure digest otherwise.
const SOLDR_PORCELAIN_KEY: &str = "SOLDR_PORCELAIN";

/// #2443: `rust.porcelain` for the launch directory exports
/// `SOLDR_PORCELAIN=1` to the agent, so no agent has to remember it. Only the
/// child session gets it: clud's own soldr calls parse soldr's output. A
/// value the user already set wins.
fn set_soldr_porcelain(env: &mut Vec<(String, String)>, enabled: bool) {
    if enabled
        && !env
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(SOLDR_PORCELAIN_KEY))
    {
        set_env(env, SOLDR_PORCELAIN_KEY, "1");
    }
}

fn launch_soldr_porcelain() -> bool {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::repo_clud_config::discover_effective_clud_config(&cwd))
        .is_some_and(|config| config.rust.use_soldr && config.rust.porcelain)
}

/// #2368: `gh.backend` for the launch directory, as `CLUD_GH_BACKEND`.
fn launch_gh_backend() -> &'static str {
    let backend = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::repo_clud_config::discover_effective_clud_config(&cwd))
        .map(|config| config.gh.backend)
        .unwrap_or_default();
    gh_backend_value(backend)
}

fn gh_backend_value(backend: crate::repo_clud_config::GhBackend) -> &'static str {
    match backend {
        crate::repo_clud_config::GhBackend::Gh => "gh",
        crate::repo_clud_config::GhBackend::Ghd => "ghd",
    }
}

/// The real `gh`: an inherited target that is still valid, else the first
/// `gh` on the PATH the session had before the alias directory was
/// prepended. Copies of the packaged shim and every shim directory are
/// skipped, so a nested launch cannot select an alias.
fn resolve_gh_target(
    original_path: &str,
    inherited_target: Option<std::path::PathBuf>,
    shim_dir: &Path,
    home: Option<&Path>,
) -> Option<std::path::PathBuf> {
    resolve_target_raw("gh", original_path, inherited_target, shim_dir, home)
}

/// [`resolve_gh_target`] for any relayed alias name (`gh`, `git`), with the
/// result canonicalized so a later PATH change cannot re-point it.
fn resolve_target(
    name: &str,
    original_path: &str,
    inherited_target: Option<std::path::PathBuf>,
    shim_dir: &Path,
    home: Option<&Path>,
) -> Option<std::path::PathBuf> {
    resolve_target_raw(name, original_path, inherited_target, shim_dir, home)
        .map(|target| target.canonicalize().unwrap_or(target))
}

fn resolve_target_raw(
    name: &str,
    original_path: &str,
    inherited_target: Option<std::path::PathBuf>,
    shim_dir: &Path,
    home: Option<&Path>,
) -> Option<std::path::PathBuf> {
    let shim = crate::shim_install::packaged_shim().unwrap_or_else(|_| shim_dir.join(name));
    let mut dirs = shim_registry::shim_dirs(&shim, Some(shim_dir), home);
    // The packaged shim's own directory holds `clud` itself, not aliases.
    if let Some(own) = std::fs::canonicalize(&shim)
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        dirs.retain(|dir| *dir != own);
    }
    inherited_target
        .filter(|path| shim_registry::valid_target(path, &shim, &dirs))
        .or_else(|| {
            shim_registry::first_on_path(
                &shim_registry::file_name(name),
                std::ffi::OsStr::new(original_path),
                &shim,
                &dirs,
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn gh_target_resolution_skips_the_alias_before_path_prepend() {
        let temp = TempDir::new().unwrap();
        let shim_dir = temp.path().join("shim");
        let real_dir = temp.path().join("real");
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        let name = if crate::platform::host::WINDOWS {
            "gh.exe"
        } else {
            "gh"
        };
        std::fs::write(shim_dir.join(name), b"shim").unwrap();
        let real = real_dir.join(name);
        std::fs::write(&real, b"real").unwrap();
        crate::platform::shim_fs::set_mode(&real, 0o755).unwrap();
        let path = std::env::join_paths([&shim_dir, &real_dir]).unwrap();
        let path = path.to_string_lossy();
        assert_eq!(
            resolve_gh_target(&path, None, &shim_dir, None),
            Some(real.clone())
        );
        assert_eq!(
            resolve_gh_target(&path, Some(real.clone()), &shim_dir, None),
            Some(real.clone())
        );
        assert_eq!(
            resolve_gh_target(&path, Some(shim_dir.join(name)), &shim_dir, None),
            Some(real),
            "an inherited target inside the alias directory is re-resolved"
        );
    }

    /// #1486 acceptance 5: the `git` target resolves to the real binary,
    /// never to the alias in the session directory or the managed shim dirs.
    #[test]
    fn git_target_resolution_never_selects_an_alias_dir() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let shim_dir = home.join(shim_registry::SESSION_SUBDIR);
        let interp_dir = home.join(shim_registry::INTERPRETER_SUBDIR);
        let real_dir = temp.path().join("real");
        let name = shim_registry::file_name("git");
        for dir in [&shim_dir, &interp_dir, &real_dir] {
            std::fs::create_dir_all(dir).unwrap();
            let file = dir.join(&name);
            std::fs::write(&file, dir.to_string_lossy().as_bytes()).unwrap();
            crate::platform::shim_fs::set_mode(&file, 0o755).unwrap();
        }
        let real = real_dir.join(&name).canonicalize().unwrap();
        let path = std::env::join_paths([&shim_dir, &interp_dir, &real_dir]).unwrap();
        let path = path.to_string_lossy();
        for inherited in [
            None,
            Some(shim_dir.join(&name)),
            Some(interp_dir.join(&name)),
        ] {
            let got = resolve_target("git", &path, inherited, &shim_dir, Some(&home)).unwrap();
            assert_eq!(got, real);
            for alias_dir in [&shim_dir, &interp_dir] {
                assert!(!got.starts_with(alias_dir.canonicalize().unwrap()));
            }
        }
        let only_aliases = std::env::join_paths([&shim_dir, &interp_dir]).unwrap();
        assert_eq!(
            resolve_target(
                "git",
                &only_aliases.to_string_lossy(),
                None,
                &shim_dir,
                Some(&home)
            ),
            None,
            "no real git: no target, so the alias is a plain passthrough"
        );
    }

    #[test]
    fn soldr_porcelain_is_exported_only_when_enabled_and_keeps_a_user_value() {
        let mut e = env(&[]);
        set_soldr_porcelain(&mut e, false);
        assert!(e.is_empty());
        set_soldr_porcelain(&mut e, true);
        assert_eq!(e, env(&[("SOLDR_PORCELAIN", "1")]));
        let mut user = env(&[("SOLDR_PORCELAIN", "0")]);
        set_soldr_porcelain(&mut user, true);
        assert_eq!(user, env(&[("SOLDR_PORCELAIN", "0")]));
    }

    #[test]
    fn moves_existing_later_entry_to_front() {
        let sep = path_sep();
        let mut e = env(&[("PATH", &format!("/system{sep}/shims{sep}/last"))]);
        assert!(prepend_to_path(&mut e, Path::new("/shims")));
        assert_eq!(e[0].1, format!("/shims{sep}/system{sep}/last"));
    }

    #[test]
    fn prepend_to_path_when_path_already_present() {
        let mut e = env(&[("PATH", "/usr/bin:/bin")]);
        let added = prepend_to_path(&mut e, Path::new("/home/u/.clud/state/shims"));
        assert!(added);
        let path = &e.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert!(path.starts_with("/home/u/.clud/state/shims"), "got {path}");
        assert!(path.contains("/usr/bin"));
    }

    #[test]
    fn prepend_to_path_is_idempotent() {
        let initial_path = format!("/home/u/.clud/state/shims{}/usr/bin", path_sep());
        let mut e = env(&[("PATH", initial_path.as_str())]);
        let added = prepend_to_path(&mut e, Path::new("/home/u/.clud/state/shims"));
        assert!(!added, "second prepend should be a no-op");
        // Path unchanged.
        let path = &e.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert_eq!(path, &initial_path);
    }

    #[test]
    fn prepend_to_path_creates_path_when_absent() {
        let mut e = env(&[("OTHER", "x")]);
        let added = prepend_to_path(&mut e, Path::new("/shims"));
        assert!(added);
        let path = &e.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert_eq!(path, "/shims");
    }

    #[test]
    fn ghd_real_gh_names_the_target_but_keeps_a_user_value() {
        let mut env = Vec::new();
        set_ghd_real_gh(&mut env, Path::new("/real/gh"));
        assert_eq!(env, vec![("GHD_REAL_GH".into(), "/real/gh".into())]);
        let mut env = vec![("GHD_REAL_GH".to_string(), "/mine/gh".to_string())];
        set_ghd_real_gh(&mut env, Path::new("/real/gh"));
        assert_eq!(env, vec![("GHD_REAL_GH".into(), "/mine/gh".into())]);
    }

    #[test]
    fn gh_backend_value_names_the_route_for_the_watchers() {
        use crate::repo_clud_config::GhBackend;
        assert_eq!(gh_backend_value(GhBackend::Gh), "gh");
        assert_eq!(gh_backend_value(GhBackend::Ghd), "ghd");
        assert_eq!(shim_registry::GH_BACKEND_KEY, "CLUD_GH_BACKEND");
    }

    #[test]
    fn set_env_replaces_existing_value() {
        let mut e = env(&[("CLUD_EXAMPLE_KEY", "/old/sock")]);
        set_env(&mut e, "CLUD_EXAMPLE_KEY", "/new/sock");
        let v = &e.iter().find(|(k, _)| k == "CLUD_EXAMPLE_KEY").unwrap().1;
        assert_eq!(v, "/new/sock");
    }

    #[test]
    fn set_env_creates_when_absent() {
        let mut e = env(&[("OTHER", "x")]);
        set_env(&mut e, "CLUD_EXAMPLE_KEY", "/sock");
        assert!(e
            .iter()
            .any(|(k, v)| k == "CLUD_EXAMPLE_KEY" && v == "/sock"));
    }

    #[test]
    fn path_handling_uses_platform_separator() {
        let mut e = env(&[("PATH", "/usr/bin")]);
        prepend_to_path(&mut e, &PathBuf::from("/shims"));
        let path = &e.iter().find(|(k, _)| k == "PATH").unwrap().1;
        let sep = path_sep();
        assert!(
            path.contains(sep),
            "PATH {path} must contain platform sep {sep}"
        );
    }
}

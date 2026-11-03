//! Fallback for Claude Code's native installer when the home volume is too
//! small or not writable.
//!
//! A live USB (an Arch ISO, for one) gives `$HOME` a writable layer of
//! about 256 MB, while Claude Code needs about 320 MB plus room for the
//! download. The installer then fails with "No space left on device". The
//! system temp directory is usually a RAM-backed tmpfs with far more room,
//! so clud reruns the official installer once with `HOME` set to a private
//! directory there and links the binary back to `~/.local/bin/claude`, the
//! location clud already checks. Only the installer sees the temporary
//! `HOME`; the login and settings stay under the real `~/.claude`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Name prefix of every temp install home clud creates.
pub const TEMP_HOME_PREFIX: &str = "clud-claude-home-";

/// Where the fallback install put the binary and the link clud resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempHomeInstall {
    pub temp_home: PathBuf,
    pub binary: PathBuf,
    pub link: PathBuf,
}

/// Yellow warning printed when the fallback engages.
pub fn engaged_warning(real_home: &Path, temp_home: &Path) -> String {
    format!(
        "\x1b[33m[clud] WARNING: Claude Code could not be installed in {} (out of space or not writable); \
         falling back to a temporary install home at {}\x1b[0m",
        real_home.display(),
        temp_home.display()
    )
}

/// Yellow warning printed after the fallback install succeeded.
pub fn installed_warning(done: &TempHomeInstall) -> String {
    format!(
        "\x1b[33m[clud] WARNING: Claude Code is installed in temporary storage at {} (linked from {}); \
         it uses RAM on a tmpfs and is lost on reboot\x1b[0m",
        done.binary.display(),
        done.link.display()
    )
}

/// Create a private (0700, randomly named) home under the system temp dir.
/// It is kept on purpose: the installed binary lives there.
pub fn create_temp_home() -> Result<PathBuf, String> {
    tempfile::Builder::new()
        .prefix(TEMP_HOME_PREFIX)
        .tempdir_in(std::env::temp_dir())
        .map(tempfile::TempDir::keep)
        .map_err(|err| format!("cannot create a temporary install home: {err}"))
}

/// `<temp_home>/.local/bin/claude`, where the native installer puts the binary.
pub fn binary_in(temp_home: &Path) -> PathBuf {
    temp_home.join(".local").join("bin").join("claude")
}

/// Find a Claude Code binary in a temp install home under the system temp
/// dir, for when `~/.local/bin/claude` is missing. POSIX only: the fallback
/// never runs on Windows.
pub fn discover() -> Option<PathBuf> {
    if crate::platform::host::WINDOWS {
        return None;
    }
    discover_in(&std::env::temp_dir())
}

/// The newest `<temp_root>/clud-claude-home-*/.local/bin/claude`. The temp
/// root is shared, so a home counts only when it is a real directory owned
/// by this user and writable by no one else; anything another user planted
/// is ignored.
pub fn discover_in(temp_root: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(temp_root).ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(TEMP_HOME_PREFIX)
        {
            continue;
        }
        let home = entry.path();
        let Ok(meta) = home.symlink_metadata() else {
            continue;
        };
        if !meta.is_dir() || !crate::platform::self_install::user_controlled_dir(&meta) {
            continue;
        }
        let binary = binary_in(&home);
        let Ok(binary_meta) = binary.metadata() else {
            continue;
        };
        if !binary_meta.is_file() {
            continue;
        }
        let modified = binary_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, binary));
        }
    }
    newest.map(|(_, binary)| binary)
}

/// The temp install home `binary` really lives in (after following links),
/// or `None` for an ordinary install.
pub fn temp_home_of(binary: &Path) -> Option<PathBuf> {
    let binary = std::fs::canonicalize(binary).ok()?;
    let temp_root = std::fs::canonicalize(std::env::temp_dir()).ok()?;
    let first = binary.strip_prefix(&temp_root).ok()?.components().next()?;
    let name = first.as_os_str().to_string_lossy();
    name.starts_with(TEMP_HOME_PREFIX)
        .then(|| temp_root.join(first.as_os_str()))
}

/// The parent environment with `HOME` replaced by `temp_home`.
pub fn installer_env(parent: &[(String, String)], temp_home: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = parent
        .iter()
        .filter(|(key, _)| key != "HOME")
        .cloned()
        .collect();
    env.push(("HOME".into(), temp_home.to_string_lossy().into_owned()));
    env
}

/// After the installer succeeded under `temp_home`, drop its download cache
/// (it costs RAM on tmpfs) and link the binary to `<real_home>/.local/bin`.
pub fn finish(temp_home: &Path, real_home: &Path) -> Result<TempHomeInstall, String> {
    let binary = binary_in(temp_home);
    if !binary.is_file() {
        return Err(format!(
            "the installer finished but left no binary at {}",
            binary.display()
        ));
    }
    let _ = std::fs::remove_dir_all(temp_home.join(".claude").join("downloads"));
    let bin_dir = real_home.join(".local").join("bin");
    std::fs::create_dir_all(&bin_dir)
        .map_err(|err| format!("cannot create {}: {err}", bin_dir.display()))?;
    let link = bin_dir.join("claude");
    // The normal install failed, so whatever sits here is a leftover
    // (typically a dangling link); a working binary would have been found.
    if link.symlink_metadata().is_ok() {
        std::fs::remove_file(&link)
            .map_err(|err| format!("cannot replace {}: {err}", link.display()))?;
    }
    crate::platform::alias_link::symlink(&binary, &link)
        .map_err(|err| format!("cannot link {}: {err}", link.display()))?;
    Ok(TempHomeInstall {
        temp_home: temp_home.to_path_buf(),
        binary,
        link,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_warnings_are_yellow_and_name_the_paths() {
        let engaged = engaged_warning(Path::new("/root"), Path::new("/tmp/h"));
        let installed = installed_warning(&TempHomeInstall {
            temp_home: PathBuf::from("/tmp/h"),
            binary: PathBuf::from("/tmp/h/.local/bin/claude"),
            link: PathBuf::from("/root/.local/bin/claude"),
        });
        for (warning, paths) in [
            (&engaged, ["/root", "/tmp/h"]),
            (
                &installed,
                ["/tmp/h/.local/bin/claude", "/root/.local/bin/claude"],
            ),
        ] {
            assert!(warning.starts_with("\x1b[33m") && warning.ends_with("\x1b[0m"));
            assert!(warning.contains("WARNING"));
            for path in paths {
                assert!(warning.contains(path), "{warning}");
            }
        }
    }

    fn plant_binary(home: &Path) -> PathBuf {
        let binary = binary_in(home);
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"bin").unwrap();
        binary
    }

    #[test]
    fn discover_finds_a_private_temp_home_and_ignores_others() {
        if crate::platform::host::WINDOWS {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ours = tempfile::Builder::new()
            .prefix(TEMP_HOME_PREFIX)
            .tempdir_in(root.path())
            .unwrap();
        let binary = plant_binary(ours.path());
        // Unrelated name, and a home with no binary: both ignored.
        plant_binary(&root.path().join("someone-else"));
        std::fs::create_dir(root.path().join(format!("{TEMP_HOME_PREFIX}empty"))).unwrap();
        assert_eq!(discover_in(root.path()), Some(binary));
    }

    #[test]
    fn discover_ignores_a_temp_home_others_can_write() {
        if crate::platform::host::WINDOWS {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join(format!("{TEMP_HOME_PREFIX}shared"));
        plant_binary(&shared);
        crate::platform::shim_fs::set_mode(&shared, 0o777).unwrap();
        assert_eq!(discover_in(root.path()), None);
    }

    #[test]
    fn discover_returns_none_without_any_temp_home() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(discover_in(root.path()), None);
    }

    #[test]
    fn installer_env_replaces_only_home() {
        let parent = vec![
            ("HOME".to_string(), "/root".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ];
        let env = installer_env(&parent, Path::new("/tmp/h"));
        assert_eq!(
            env,
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("HOME".to_string(), "/tmp/h".to_string()),
            ]
        );
    }

    #[test]
    fn finish_links_the_binary_and_drops_the_download_cache() {
        if crate::platform::host::WINDOWS {
            return; // The fallback runs only on POSIX hosts.
        }
        let temp_home = tempfile::tempdir().unwrap();
        let real_home = tempfile::tempdir().unwrap();
        let bin = temp_home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("claude"), b"bin").unwrap();
        let downloads = temp_home.path().join(".claude/downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        // A dangling link left by the failed normal install.
        std::fs::create_dir_all(real_home.path().join(".local/bin")).unwrap();
        crate::platform::alias_link::symlink(
            Path::new("/nonexistent/claude"),
            &real_home.path().join(".local/bin/claude"),
        )
        .unwrap();

        let done = finish(temp_home.path(), real_home.path()).unwrap();

        assert_eq!(done.link, real_home.path().join(".local/bin/claude"));
        assert!(done.link.is_file(), "link resolves to the binary");
        assert_eq!(std::fs::read(&done.link).unwrap(), b"bin");
        assert!(!downloads.exists());
    }

    #[test]
    fn finish_rejects_an_install_that_left_no_binary() {
        let temp_home = tempfile::tempdir().unwrap();
        let real_home = tempfile::tempdir().unwrap();
        let err = finish(temp_home.path(), real_home.path()).unwrap_err();
        assert!(err.contains("left no binary"), "{err}");
    }
}

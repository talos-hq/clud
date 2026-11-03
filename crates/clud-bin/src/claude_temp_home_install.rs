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
        .prefix("clud-claude-home-")
        .tempdir_in(std::env::temp_dir())
        .map(tempfile::TempDir::keep)
        .map_err(|err| format!("cannot create a temporary install home: {err}"))
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
    let binary = temp_home.join(".local").join("bin").join("claude");
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

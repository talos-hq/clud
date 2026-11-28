//! Unix half of `crate::backend_bootstrap`: the Linux-only tests of the
//! trusted Codex standalone updater, which runs only on Linux.

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::Path;

    use crate::backend_bootstrap::{
        run_captured_command, run_verified_codex_installer,
        run_verified_codex_installer_with_parent_env, trusted_codex_update_env,
    };

    #[test]
    fn codex_update_env_does_not_inherit_session_shims_or_installer_overrides() {
        let home = Path::new("/tmp/clud-codex-update-home");
        let env =
            trusted_codex_update_env(home, Some("/untrusted/shims:/usr/bin"), Some("/bin/bash"));
        assert!(env
            .iter()
            .any(|(key, value)| key == "PATH" && value.ends_with("/usr/bin:/bin")));
        assert!(env
            .iter()
            .all(|(_, value)| !value.contains("/untrusted/shims")));
        assert!(env
            .iter()
            .any(|(key, value)| key == "SHELL" && value == "/bin/bash"));
        assert!(env
            .iter()
            .any(|(key, value)| key == "HOME" && value == &home.to_string_lossy()));
        assert!(env
            .iter()
            .any(|(key, value)| key == "CODEX_NON_INTERACTIVE" && value == "1"));
        assert!(!env.iter().any(|(key, _)| key == "CLUD_RM_DRY_RUN"));
        assert!(!env.iter().any(|(key, _)| key == "CODEX_INSTALL_DIR"));
        let with_visible_bin = trusted_codex_update_env(
            home,
            Some("/untrusted/shims:/tmp/clud-codex-update-home/.local/bin:/usr/bin"),
            None,
        );
        assert!(with_visible_bin.iter().any(|(key, value)| key == "PATH"
            && value.ends_with(":/tmp/clud-codex-update-home/.local/bin")));
    }

    #[test]
    fn codex_update_fixture_covers_fresh_complete_and_stale_staging() {
        use kernal_api::hash::Sha256Hasher as Sha256;
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::write(
            home.join("fixture-codex"),
            b"#!/bin/sh\nprintf 'codex-cli 0.156.0\\n'\n",
        )
        .unwrap();
        let script = br#"set -eu
root="$HOME/.codex/packages/standalone"
releases="$root/releases"
mkdir -p "$releases"
tmp_dir="$(mktemp -d "$HOME/.codex/tmp.XXXXXX")"
remove=rm
cleanup() { "$remove" -rf "$tmp_dir"; }
trap cleanup EXIT
find "$releases" -mindepth 1 -maxdepth 1 -name '.staging.*' -exec "$remove" -rf {} +
stage="$releases/.staging.fixture.$$"
"$remove" -rf "$stage"
if [ ! -f "$releases/0.156.0/version" ]; then
  mkdir -p "$stage/bin"
  cp "$HOME/fixture-codex" "$stage/bin/codex"
  chmod 0755 "$stage/bin/codex"
  printf '0.156.0' > "$stage/version"
  mv "$stage" "$releases/0.156.0"
fi
ln -sfn releases/0.156.0 "$root/current"
"#;
        let digest = format!("{:x}", Sha256::digest(script));
        let root = home.join(".codex/packages/standalone");
        let releases = root.join("releases");
        run_verified_codex_installer(script, &digest, home).unwrap();
        assert_eq!(
            std::fs::read_to_string(releases.join("0.156.0/version")).unwrap(),
            "0.156.0"
        );
        assert_eq!(
            std::fs::read_link(root.join("current")).unwrap(),
            Path::new("releases/0.156.0")
        );
        let (code, version) = run_captured_command(vec![
            root.join("current/bin/codex")
                .to_string_lossy()
                .into_owned(),
            "--version".into(),
        ])
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(version.trim(), "codex-cli 0.156.0");
        run_verified_codex_installer(script, &digest, home).unwrap();
        let stale = releases.join(".staging.old");
        std::fs::create_dir(&stale).unwrap();
        run_verified_codex_installer(script, &digest, home).unwrap();
        assert!(!stale.exists());
        assert!(std::fs::read_dir(home.join(".codex"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("tmp.")));
        assert!(std::fs::read_dir(releases).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging.")));
    }

    #[test]
    fn codex_update_removes_stale_files_from_both_session_routes() {
        use kernal_api::hash::Sha256Hasher as Sha256;
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_string_lossy().into_owned();
        let shim_dir = home.path().join("session-shim");
        std::fs::create_dir(&shim_dir).unwrap();
        let poisoned_rm = shim_dir.join("rm");
        std::fs::write(&poisoned_rm, b"#!/bin/sh\nexit 27\n").unwrap();
        std::fs::set_permissions(&poisoned_rm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let base = vec![
            ("HOME".to_string(), home_path),
            (
                "PATH".to_string(),
                format!("{}:/usr/bin:/bin", shim_dir.display()),
            ),
        ];
        let routes = [
            (
                "foreground",
                crate::runner::apply_child_env_policy(base.clone()),
            ),
            ("daemon", crate::daemon::io_helpers::child_env_from(&base)),
        ];
        let script = b"set -eu\nrm -rf \"$HOME/stale\"\n";
        let digest = format!("{:x}", Sha256::digest(script));
        for (route, parent_env) in routes {
            let path = parent_env
                .iter()
                .find(|(key, _)| key == "PATH")
                .map(|(_, value)| value.as_str())
                .unwrap();
            assert!(
                path.contains("session-shim"),
                "{route} route lost the shim: {path}"
            );
            let stale = home.path().join("stale");
            std::fs::create_dir(&stale).unwrap();
            std::fs::write(stale.join("old"), b"old").unwrap();
            run_verified_codex_installer_with_parent_env(script, &digest, home.path(), &parent_env)
                .unwrap();
            assert!(!stale.exists(), "{route} route retained stale files");
        }
    }

    #[test]
    fn codex_update_rejects_changed_installer_before_execution() {
        let home = tempfile::tempdir().unwrap();
        let err =
            run_verified_codex_installer(b"touch \"$HOME/not-run\"\n", "wrong-digest", home.path())
                .unwrap_err();
        assert!(err.contains("installer contents changed"));
        assert!(!home.path().join("not-run").exists());
    }
}

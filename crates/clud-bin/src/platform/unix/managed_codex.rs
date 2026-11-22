//! Persistent Codex gateway for product configuration. The lazy clud daemon
//! has no login registration; Linux uses the user service manager instead.

use std::io::Write;
use std::path::Path;

/// Install and enable the gateway only when a product selects clud-codex.
/// No system service, privilege escalation, or daemon settings are involved.
pub fn install() -> Result<&'static str, String> {
    if !super::host::LINUX || which::which("systemctl").is_err() {
        return Ok("automatic Codex gateway startup is unavailable on this host; run `clud serve --codex` manually before using clud-codex in Hermes; leave CLUD_SERVE_CLIENT_TOKEN unset to use the saved client token");
    }
    let home = crate::home::user_home().ok_or("cannot resolve the home directory")?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    if !config.is_absolute() {
        return Err("XDG_CONFIG_HOME must be absolute".into());
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    install_at(&config, &executable, &mut systemctl)?;
    Ok("Codex gateway enabled at login; if port 8317 is occupied, it retries every 5 seconds until the port is released (see `systemctl --user status clud-serve-codex.service`)")
}

const UNIT: &str = "clud-serve-codex.service";
const MARKER: &str = "# Managed by clud install: Codex gateway\n";

fn unit_text(executable: &Path) -> Result<String, String> {
    let executable = executable
        .to_str()
        .ok_or("clud executable path must be UTF-8")?;
    if !Path::new(executable).is_absolute() || executable.chars().any(char::is_control) {
        return Err("clud executable must be an absolute path without control characters".into());
    }
    if executable.contains(['"', '\'', '\\']) {
        return Err("clud executable path must not contain quotes or backslashes".into());
    }
    // Quote the command path for spaces and double systemd specifiers. The ':' prefix
    // disables environment expansion; '$' must stay literal in the path.
    let executable = executable.replace('%', "%%");
    Ok(format!(
        "{MARKER}[Unit]\nDescription=clud Codex gateway for Hermes\nStartLimitIntervalSec=0\n\n\
         [Service]\nType=exec\nExecStart=\":{executable}\" serve --codex --port 8317\n\
         UnsetEnvironment=CLUD_SERVE_CLIENT_TOKEN\nRestart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n"
    ))
}

fn install_at(
    config: &Path,
    executable: &Path,
    run: &mut dyn FnMut(&[&str]) -> Result<(), String>,
) -> Result<(), String> {
    use fs4::fs_std::FileExt;

    let text = unit_text(executable)?;
    let directory = config.join("systemd/user");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let lock = std::fs::File::create(directory.join(".clud-codex.lock"))
        .map_err(|error| error.to_string())?;
    lock.lock_exclusive().map_err(|error| error.to_string())?;
    write_unit(&directory.join(UNIT), &text)?;
    run(&["--user", "daemon-reload"])?;
    // enable --now is idempotent and does not restart an already running unit.
    // In particular, never stop or replace a transient unit on the user's behalf.
    run(&["--user", "enable", "--now", UNIT])
}

fn write_unit(path: &Path, text: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(format!(
                    "refusing to replace {}: not a regular file",
                    path.display()
                ));
            }
            let existing = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
            if !existing.starts_with(MARKER) {
                return Err(format!("refusing to replace unmanaged {}", path.display()));
            }
            if existing == text {
                return Ok(());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let directory = path.parent().ok_or("unit path has no parent directory")?;
    let mut staged =
        tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    staged
        .write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    staged
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    staged.persist(path).map_err(|error| error.to_string())?;
    Ok(())
}

fn systemctl(arguments: &[&str]) -> Result<(), String> {
    let argv = std::iter::once("systemctl".to_string())
        .chain(arguments.iter().map(|argument| (*argument).to_string()))
        .collect();
    let process = crate::subprocess::ManagedSubprocess::start_without_stdin(
        argv,
        None,
        std::env::vars().collect(),
        false,
        None,
    )?;
    let code = process.wait(Some(std::time::Duration::from_secs(30)))?;
    if code != 0 {
        return Err(format!("systemctl {} failed (exit {code}); check the user service manager and any existing transient {UNIT}", arguments.join(" ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_codex_unit_preserves_the_authenticated_loopback_server() {
        let text = unit_text(Path::new("/opt/my clud/clud")).unwrap();
        assert!(text.contains("ExecStart=\":/opt/my clud/clud\" serve --codex --port 8317\n"));
        assert!(text.contains("UnsetEnvironment=CLUD_SERVE_CLIENT_TOKEN\n"));
        assert!(text.contains("Restart=on-failure\n"));
        assert!(text.contains("RestartSec=5\n"));
        assert!(text.contains("StartLimitIntervalSec=0\n"));
        assert!(text.contains("Type=exec\n"));
        assert!(text.contains("WantedBy=default.target\n"));
        assert!(!text.contains("api_key"));
    }

    #[test]
    fn managed_codex_unit_escapes_systemd_syntax_and_rejects_injection() {
        let text = unit_text(Path::new("/opt/%u/$USER/my clud")).unwrap();
        assert!(
            text.contains("ExecStart=\":/opt/%%u/$USER/my clud\" serve --codex --port 8317\n"),
            "{text}"
        );
        for invalid in ["/opt/\"clud", "/opt/'clud", "/opt/clud\\bin"] {
            assert!(unit_text(Path::new(invalid)).is_err(), "{invalid}");
        }
        assert!(unit_text(Path::new("clud")).is_err());
        assert!(unit_text(Path::new("/opt/clud\nExecStart=bad")).is_err());
    }

    #[test]
    fn managed_codex_first_install_and_reruns_only_reload_and_enable() {
        let root = tempfile::tempdir().unwrap();
        let mut calls = Vec::new();
        for _ in 0..2 {
            install_at(root.path(), Path::new("/opt/clud"), &mut |args| {
                if args.contains(&"reset-failed") {
                    return Err("unit not loaded on first install".into());
                }
                calls.push(
                    args.iter()
                        .map(|arg| (*arg).to_string())
                        .collect::<Vec<_>>(),
                );
                Ok(())
            })
            .unwrap();
        }
        let text = std::fs::read_to_string(root.path().join("systemd/user").join(UNIT)).unwrap();
        assert_eq!(text, unit_text(Path::new("/opt/clud")).unwrap());
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0], ["--user", "daemon-reload"]);
        assert_eq!(calls[1], ["--user", "enable", "--now", UNIT]);
        assert_eq!(calls[0..2], calls[2..4]);
    }

    #[test]
    fn managed_codex_busy_port_leaves_the_existing_listener_running() {
        use crate::codex_bridge::{BridgeConfig, BridgeError, BridgeHandle};

        let owner = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = owner.local_addr().unwrap().port();
        let config = || {
            BridgeConfig::default()
                .with_standalone_listener(port, "x".repeat(32))
                .unwrap()
        };
        let error = BridgeHandle::start(config()).unwrap_err();
        assert!(
            matches!(error, BridgeError::Bind(ref error) if error.kind() == std::io::ErrorKind::AddrInUse)
        );
        // Recovery is provided by the unit's unlimited delayed retries, not
        // immediate port reuse inside this process. A failed start must leave
        // the incumbent listener usable.
        let client = std::net::TcpStream::connect(owner.local_addr().unwrap()).unwrap();
        let (accepted, _) = owner.accept().unwrap();
        drop(client);
        drop(accepted);
    }

    #[test]
    fn managed_codex_atomic_write_ignores_the_old_predictable_staging_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(UNIT);
        let victim = root.path().join("operator-file");
        std::fs::write(&victim, "untouched").unwrap();
        std::os::unix::fs::symlink(&victim, path.with_extension("service.clud-new")).unwrap();
        let text = unit_text(Path::new("/opt/clud")).unwrap();
        write_unit(&path, &text).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
    }

    #[test]
    fn managed_codex_install_refuses_unmanaged_units_and_reports_manager_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("systemd/user").join(UNIT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# operator unit\n").unwrap();
        let result = install_at(root.path(), Path::new("/opt/clud"), &mut |_| {
            panic!("must not run systemctl")
        });
        assert!(result.unwrap_err().contains("unmanaged"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# operator unit\n");
        std::fs::write(&path, unit_text(Path::new("/opt/clud")).unwrap()).unwrap();
        let result = install_at(root.path(), Path::new("/opt/clud"), &mut |_| {
            Err("no user manager".into())
        });
        assert_eq!(result, Err("no user manager".into()));
    }
}

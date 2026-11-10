//! Status-only lokr adapter. Request arguments contain metadata, never credentials.
use super::{Failure, Options};
use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
use std::path::PathBuf;

fn resolve() -> Result<PathBuf, Failure> {
    if super::test_mode() {
        return std::env::var_os("CLUD_TEST_LOKR_BINARY")
            .map(PathBuf::from)
            .ok_or(Failure::Unavailable);
    }
    if super::real_ui_disabled() {
        return Err(Failure::Unavailable);
    }
    // Native builds embed lokr; wheels fetch the same checksum-pinned release.
    // Prefer it over unrelated or obsolete PATH installations (including 0.0.1).
    if crate::managed_lokr::has_distribution() {
        return crate::managed_lokr::ensure_installed().map_err(|_| Failure::Unavailable);
    }
    // Prefer a co-distributed executable, then the user's installed tool on PATH.
    let sibling = std::env::current_exe().ok().and_then(|exe| {
        exe.parent().map(|dir| {
            dir.join(if crate::platform::host::WINDOWS {
                "lokr.exe"
            } else {
                "lokr"
            })
        })
    });
    sibling
        .filter(|path| path.is_file())
        .or_else(|| which::which("lokr").ok())
        .ok_or(Failure::Unavailable)
}

fn argv(options: &Options, binary: PathBuf) -> Result<Vec<String>, Failure> {
    let mut args = vec![
        binary.to_string_lossy().into_owned(),
        "request".into(),
        "--title".into(),
        options.title.clone(),
        "--message".into(),
        options.label.clone(),
    ];
    if let Some(hint) = &options.hint {
        args.extend(["--hint".into(), hint.clone()]);
    }
    if let Some(pattern) = &options.pattern {
        args.extend(["--pattern".into(), pattern.as_str().into()]);
    }
    if options.multiline {
        args.push("--multiline".into());
    }
    if let Some(path) = &options.to_file {
        args.extend(["--to-file".into(), path.to_string_lossy().into_owned()]);
    } else if !options.to_keyring.is_empty() {
        args.push("--to-keyring".into());
        args.extend(options.to_keyring.iter().cloned());
    } else if let Some(vault) = options.vault {
        let provider = match vault {
            super::Vault::Deepseek => "deepseek",
            super::Vault::Kimi => "kimi",
            super::Vault::Openrouter => "openrouter",
        };
        let exe = std::env::current_exe().map_err(|_| Failure::Sink)?;
        args.extend(["--to-command".into(), exe.to_string_lossy().into_owned()]);
        if !matches!(
            crate::multicall::personality_for(exe.as_os_str()),
            Some(crate::multicall::Personality::Secret)
        ) {
            args.push("secret".into());
        }
        args.extend([
            "--internal-vault-stdin".into(),
            "--title".into(),
            options.title.clone(),
            "--message".into(),
            options.label.clone(),
            "--vault".into(),
            provider.into(),
        ]);
        if options.multiline {
            args.push("--multiline".into());
        }
    } else {
        args.push("--to-command".into());
        args.extend(options.to_command.iter().cloned());
    }
    Ok(args)
}

pub(super) fn request(options: &Options) -> Result<(), Failure> {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv(options, resolve()?)?),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Pipe,
        stdin_mode: StdinMode::Piped,
        create_process_group: true,
        creationflags: crate::win_creation_flags::invisible_helper_creationflags(),
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().map_err(|_| Failure::Unavailable)?;
    // lokr reads the controlling terminal, never the agent's stdin.
    process.write_stdin(b"").map_err(|_| Failure::Sink)?;
    match process.wait(None) {
        Ok(0) => Ok(()),
        Ok(3) => Err(Failure::Cancelled),
        _ => Err(Failure::Sink),
    }
}

/// Delegate the catalog too: one installed lokr owns its current MCP schema and UI.
pub(crate) fn catalog(command: &str) -> i32 {
    let Ok(binary) = resolve() else {
        eprintln!("lokr is required: install forge-ci/lokr and put lokr on PATH");
        return 1;
    };
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![binary.to_string_lossy().into_owned(), command.into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Pipe,
        stdin_mode: StdinMode::Inherit,
        create_process_group: true,
        creationflags: crate::win_creation_flags::invisible_helper_creationflags(),
        nice: None,
        address_space_limit_bytes: None,
    });
    if process.start().is_err() {
        return 1;
    }
    process.wait(None).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn vault_routes_to_stdin_only_internal_writer() {
        let options = Options::try_parse_from([
            "secret",
            "--title",
            "OpenRouter API token",
            "--message",
            "Enable requests; find the token in OpenRouter settings",
            "--vault",
            "openrouter",
        ])
        .unwrap();
        let args = argv(&options, PathBuf::from("lokr")).unwrap();
        let sink = args.iter().position(|arg| arg == "--to-command").unwrap();
        assert_eq!(args[sink + 2], "secret");
        assert_eq!(args[sink + 3], "--internal-vault-stdin");
        assert_eq!(&args[args.len() - 2..], ["--vault", "openrouter"]);
    }
}

//! Secret sinks discard child output, including failure diagnostics that may echo input.

use std::time::{Duration, Instant};

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode, StreamKind,
};
use zeroize::Zeroize;

use super::{Failure, Options, Secret, Vault};

pub(super) fn deliver(options: &Options, secret: &Secret) -> Result<(), Failure> {
    if let Some(path) = &options.to_file {
        crate::fs_private::write_private_atomic(path, secret.as_bytes()).map_err(|_| Failure::Sink)
    } else if let Some(provider) = options.vault {
        store_in_vault(provider, secret)
    } else if !options.to_command.is_empty() {
        run_command(&options.to_command, secret)
    } else {
        Err(Failure::Sink)
    }
}

fn store_in_vault(provider: Vault, secret: &str) -> Result<(), Failure> {
    use crate::provider_auth::SecretStore;
    let provider = match provider {
        Vault::Deepseek => crate::backend::ModelProvider::DeepSeek,
        Vault::Kimi => crate::backend::ModelProvider::Kimi,
        Vault::Openrouter => crate::backend::ModelProvider::OpenRouter,
    };
    let descriptor = crate::provider_registry::descriptor_for(provider).ok_or(Failure::Sink)?;
    crate::provider_auth::NativeSecretStore::new_for(
        descriptor.vault_service,
        descriptor.vault_account,
    )
    .and_then(|store| store.set(secret))
    .map_err(|_| Failure::Sink)
}

fn run_command(argv: &[String], secret: &str) -> Result<(), Failure> {
    let process = std::sync::Arc::new(NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv.to_vec()),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Pipe,
        stdin_mode: StdinMode::Piped,
        create_process_group: true,
        creationflags: crate::win_creation_flags::invisible_helper_creationflags(),
        nice: None,
        address_space_limit_bytes: None,
    }));
    process.start().map_err(|_| Failure::Sink)?;
    let deadline = Instant::now() + command_timeout();
    let writer_process = std::sync::Arc::clone(&process);
    let input = zeroize::Zeroizing::new(secret.to_string());
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let writer = std::thread::spawn(move || {
        let ok = writer_process.write_stdin(input.as_bytes()).is_ok();
        let _ = sent.send(ok);
    });
    let mut closed = [false; 2];
    while !closed.iter().all(|value| *value) && Instant::now() < deadline {
        for (index, stream) in [StreamKind::Stdout, StreamKind::Stderr]
            .into_iter()
            .enumerate()
        {
            if !closed[index] {
                match process.read_stream(stream, Some(Duration::from_millis(50))) {
                    ReadStatus::Line(mut bytes) => bytes.zeroize(),
                    ReadStatus::Eof => closed[index] = true,
                    ReadStatus::Timeout => {}
                }
            }
        }
    }
    let result = process.wait(Some(deadline.saturating_duration_since(Instant::now())));
    let written = received.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    if matches!(written, Ok(true)) {
        let _ = writer.join();
    }
    if matches!(result, Ok(0)) && matches!(written, Ok(true)) {
        Ok(())
    } else {
        let _ = process.kill();
        Err(Failure::Sink)
    }
}

fn command_timeout() -> Duration {
    if super::test_mode() {
        if let Some(millis) = std::env::var("CLUD_TEST_SECRET_PROMPT_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            return Duration::from_millis(millis.clamp(50, 1000));
        }
    }
    Duration::from_secs(120)
}

pub(super) fn askpass_caller_allowed() -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let Ok(mut pid) = sysinfo::get_current_pid() else {
        return false;
    };
    let mut system = System::new();
    for _ in 0..3 {
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        let Some(parent) = system.process(pid).and_then(sysinfo::Process::parent) else {
            return false;
        };
        pid = parent;
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        let Some(process) = system.process(pid) else {
            return false;
        };
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if allowed_caller_name(name.trim_end_matches(".exe")) {
            return true;
        }
        if !matches!(
            name.as_str(),
            "sh" | "bash" | "dash" | "cmd.exe" | "powershell.exe" | "pwsh.exe"
        ) {
            return false;
        }
    }
    false
}

fn allowed_caller_name(name: &str) -> bool {
    matches!(
        name,
        "git" | "git-remote-https" | "git-remote-http" | "ssh" | "ssh-add" | "ssh-keygen" | "sudo"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn askpass_does_not_accept_agents_or_arbitrary_tools_as_callers() {
        for name in [
            "git",
            "git-remote-https",
            "ssh",
            "ssh-add",
            "ssh-keygen",
            "sudo",
        ] {
            assert!(allowed_caller_name(name));
        }
        for name in ["claude", "codex", "python", "echo", "git-fake"] {
            assert!(!allowed_caller_name(name));
        }
    }
}

/// Only stdin carries secret bytes; never report parser/store errors containing input.
pub(super) fn vault_from_stdin(options: &Options) -> Result<(), Failure> {
    use std::io::Read;
    let provider = options.vault.ok_or(Failure::Sink)?;
    let mut value = zeroize::Zeroizing::new(String::new());
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut value)
        .map_err(|_| Failure::Invalid)?;
    let value = super::validate_secret(value, options.multiline)?;
    store_in_vault(provider, &value)
}

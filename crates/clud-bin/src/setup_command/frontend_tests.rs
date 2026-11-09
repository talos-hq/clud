use std::path::{Path, PathBuf};

use super::*;
use crate::backend_bootstrap::InstallPlatform;

/// A host whose frontend is missing until the installer runs.
#[derive(Default)]
struct Host {
    installed: bool,
    installer_runs: usize,
    installer_fails: bool,
}

impl BackendBootstrapHost for Host {
    fn platform(&self) -> InstallPlatform {
        InstallPlatform::Linux
    }
    fn find_backend(&mut self, backend: Backend) -> Option<PathBuf> {
        self.installed
            .then(|| PathBuf::from(format!("/bin/{}", backend.executable_name())))
    }
    fn run_backend_installer(&mut self, _spec: &BackendInstallSpec) -> Result<(), String> {
        self.installer_runs += 1;
        if self.installer_fails {
            return Err("network down".to_string());
        }
        self.installed = true;
        Ok(())
    }
    fn native_backend_path(&self, _spec: &BackendInstallSpec) -> Option<PathBuf> {
        None
    }
    fn verify_backend(&mut self, _backend: Backend, _path: &Path) -> Result<(), String> {
        Ok(())
    }
}

fn run(provider: ModelProvider, mode: Mode, typed: &str, host: &mut Host) -> (i32, String) {
    let mut input = typed.as_bytes();
    let mut out = Vec::new();
    let code = ensure_frontend(provider, mode, &mut input, host, &mut out);
    (code, String::from_utf8(out).unwrap())
}

#[test]
fn openrouter_and_claude_launch_through_claude_code_codex_through_codex() {
    assert_eq!(frontend_for(ModelProvider::OpenRouter), Backend::Claude);
    assert_eq!(frontend_for(ModelProvider::Claude), Backend::Claude);
    assert_eq!(frontend_for(ModelProvider::Codex), Backend::Codex);
}

#[test]
fn a_present_frontend_is_reported_and_never_reinstalled() {
    let mut host = Host {
        installed: true,
        ..Host::default()
    };
    let (code, out) = run(ModelProvider::OpenRouter, Mode::Yes, "", &mut host);
    assert_eq!(code, 0);
    assert!(out.contains("frontend: claude found"), "{out}");
    assert_eq!(host.installer_runs, 0);
}

#[test]
fn yes_installs_a_missing_frontend_without_asking() {
    let mut host = Host::default();
    let (code, out) = run(ModelProvider::OpenRouter, Mode::Yes, "", &mut host);
    assert_eq!(code, 0, "{out}");
    assert_eq!(host.installer_runs, 1);
    assert!(out.contains("frontend: claude installed"), "{out}");
}

#[test]
fn the_prompt_installs_on_yes_and_reports_the_command_on_no() {
    let mut host = Host::default();
    let (code, _) = run(ModelProvider::Codex, Mode::Prompt, "y\n", &mut host);
    assert_eq!((code, host.installer_runs), (0, 1));

    let mut host = Host::default();
    let (code, out) = run(ModelProvider::Claude, Mode::Prompt, "n\n", &mut host);
    assert_eq!((code, host.installer_runs), (1, 0));
    assert!(out.contains("claude.ai/install.sh"), "{out}");
}

#[test]
fn dry_run_reports_a_missing_frontend_and_installs_nothing() {
    let mut host = Host::default();
    let (code, out) = run(ModelProvider::OpenRouter, Mode::DryRun, "", &mut host);
    assert_eq!(code, 0);
    assert_eq!(host.installer_runs, 0);
    assert!(out.contains("would install Claude Code"), "{out}");
}

#[test]
fn a_failed_install_fails_setup_with_the_manual_command() {
    let mut host = Host {
        installer_fails: true,
        ..Host::default()
    };
    let (code, out) = run(ModelProvider::OpenRouter, Mode::Yes, "", &mut host);
    assert_eq!(code, 1);
    assert!(out.contains("network down"), "{out}");
}

#[test]
fn installed_check_follows_the_provider_frontend() {
    assert!(!frontend_installed(
        ModelProvider::OpenRouter,
        &mut Host::default()
    ));
    let mut host = Host {
        installed: true,
        ..Host::default()
    };
    assert!(frontend_installed(ModelProvider::Codex, &mut host));
}

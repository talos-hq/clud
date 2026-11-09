//! Setup step 3: the frontend the chosen provider launches through (#2755).
//!
//! OpenRouter and Claude launch through Claude Code, Codex through Codex.
//! Setup used to finish without checking, so the first launch after a
//! fresh OpenRouter setup failed with "claude not found on PATH".

use std::io::Write;

use crate::backend::{Backend, ModelProvider};
use crate::backend_bootstrap::{
    locate_backend, resolve_backend_path, BackendBootstrapHost, BackendInstallSpec,
};

/// The frontend a provider launches through.
pub(super) fn frontend_for(provider: ModelProvider) -> Backend {
    match provider {
        ModelProvider::Codex => Backend::Codex,
        _ => Backend::Claude,
    }
}

/// Whether the provider's frontend is installed (PATH or native location).
pub(super) fn frontend_installed<H: BackendBootstrapHost>(
    provider: ModelProvider,
    host: &mut H,
) -> bool {
    let backend = frontend_for(provider);
    let spec = BackendInstallSpec::for_backend(backend, host.platform());
    locate_backend(host, backend, &spec).is_some()
}

/// How the step may act when the frontend is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Report only.
    DryRun,
    /// Install without asking (`--yes`).
    Yes,
    /// Ask on the terminal.
    Prompt,
}

/// Ensure the frontend exists; install it (asking first under `Prompt`).
pub(super) fn ensure_frontend<H: BackendBootstrapHost>(
    provider: ModelProvider,
    mode: Mode,
    input: &mut dyn std::io::BufRead,
    host: &mut H,
    out: &mut dyn Write,
) -> i32 {
    let backend = frontend_for(provider);
    let spec = BackendInstallSpec::for_backend(backend, host.platform());
    let name = backend.executable_name();
    if let Some(path) = locate_backend(host, backend, &spec) {
        let _ = writeln!(out, "frontend: {name} found at {}", path.display());
        return 0;
    }
    if mode == Mode::DryRun {
        let _ = writeln!(
            out,
            "frontend: {name} is not installed; would install {} ({})",
            spec.product_name, spec.manual_install_command
        );
        return 0;
    }
    let mut consent: &[u8] = b"y\n";
    let mut input: &mut dyn std::io::BufRead = if mode == Mode::Yes {
        &mut consent
    } else {
        input
    };
    match resolve_backend_path(backend, false, true, &mut input, &mut &mut *out, host) {
        Ok(path) => {
            let _ = writeln!(out, "frontend: {name} installed at {path}");
            0
        }
        Err(error) => {
            let _ = writeln!(out, "{error}");
            error.exit_code()
        }
    }
}

#[cfg(test)]
#[path = "frontend_tests.rs"]
mod tests;

/// A host whose frontend is always installed, for setup tests that are
/// about other steps.
#[cfg(test)]
pub(super) mod tests_support {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::backend_bootstrap::InstallPlatform;

    pub(in crate::setup_command) struct Present;

    impl BackendBootstrapHost for Present {
        fn platform(&self) -> InstallPlatform {
            InstallPlatform::Linux
        }
        fn find_backend(&mut self, backend: Backend) -> Option<PathBuf> {
            Some(PathBuf::from(format!("/bin/{}", backend.executable_name())))
        }
        fn run_backend_installer(&mut self, _spec: &BackendInstallSpec) -> Result<(), String> {
            panic!("setup tests must never run a real installer")
        }
        fn native_backend_path(&self, _spec: &BackendInstallSpec) -> Option<PathBuf> {
            None
        }
        fn verify_backend(&mut self, _backend: Backend, _path: &Path) -> Result<(), String> {
            Ok(())
        }
    }
}

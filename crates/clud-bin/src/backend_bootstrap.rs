//! Backend executable resolution plus first-run bootstrap helpers.
//!
//! Keep installer prompting here rather than in the runner so every launch
//! path consumes the same resolved backend path before `LaunchPlan` is built.

use std::fmt;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::{self, Backend};
use crate::platform::host;
use crate::subprocess;
use crate::win_creation_flags::invisible_helper_creationflags;
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

const CLAUDE_POSIX_INSTALL_COMMAND: &str = "curl -fsSL https://claude.ai/install.sh | bash";
const CLAUDE_WINDOWS_POWERSHELL_INSTALL_COMMAND: &str = "irm https://claude.ai/install.ps1 | iex";
const CLAUDE_WINDOWS_CMD_INSTALL_COMMAND: &str =
    "curl -fsSL https://claude.ai/install.cmd -o install.cmd && install.cmd && del install.cmd";

const CODEX_POSIX_INSTALL_COMMAND: &str = "curl -fsSL https://chatgpt.com/codex/install.sh | sh";
const CODEX_LINUX_MANAGED_INSTALL_COMMAND: &str = "clud codex-update";
const CODEX_TRUSTED_INSTALLER_URL: &str = "https://releases.openai.com/codex/install.sh";
// Audited 2026-09-22. Update this only after reviewing the new installer.
const CODEX_TRUSTED_INSTALLER_SHA256: &str =
    "150e3cf675682efeaac115aa3747add3f27887896d04ce6d0b56478d8b428bf6";
const CODEX_WINDOWS_POWERSHELL_INSTALL_COMMAND: &str =
    "irm https://chatgpt.com/codex/install.ps1 | iex";
const DEEPSEEK_PROMPT: &str = "DeepSeek Harness is not installed.";

const MIN_UNIFIED_CLAUDE_CODE_VERSION: ClaudeCodeVersion = ClaudeCodeVersion {
    major: 2,
    minor: 1,
    patch: 223,
};

/// The version floor for the `CwdChanged` hook event, which Claude Code
/// introduced in 2.1.83. Older clients never fire it, so the backstop line
/// would sit inert.
const MIN_CWD_CHANGED_CLAUDE_CODE_VERSION: ClaudeCodeVersion = ClaudeCodeVersion {
    major: 2,
    minor: 1,
    patch: 83,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClaudeCodeVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl fmt::Display for ClaudeCodeVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPlatform {
    MacOs,
    Linux,
    Windows,
}

impl InstallPlatform {
    pub fn current() -> Self {
        if host::WINDOWS {
            Self::Windows
        } else if host::MACOS {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerPlan {
    TrustedCodex,
    PosixShell {
        command: &'static str,
    },
    WindowsPowerShell {
        command: &'static str,
        cmd_fallback: Option<&'static str>,
    },
    /// clud's private npm prefix for DeepSeek Harness (#1829).
    ManagedDsh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallLocation {
    HomeLocalBin {
        executable: &'static str,
    },
    UserProfileLocalBin {
        executable: &'static str,
    },
    LocalAppDataCodexBin,
    /// The versioned clud-managed dsh prefix (#1829).
    ManagedDsh {
        windows: bool,
    },
}

impl InstallLocation {
    pub fn resolve(&self, env: &InstallPathEnv) -> Option<PathBuf> {
        match self {
            Self::HomeLocalBin { executable } => env
                .home
                .as_ref()
                .map(|home| home.join(".local").join("bin").join(executable)),
            Self::UserProfileLocalBin { executable } => env
                .user_profile
                .as_ref()
                .map(|home| home.join(".local").join("bin").join(executable)),
            Self::LocalAppDataCodexBin => env.local_app_data.as_ref().map(|root| {
                root.join("Programs")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin")
                    .join("codex.exe")
            }),
            Self::ManagedDsh { windows } => crate::home::resolve(
                *windows,
                env.user_profile.clone().map(Into::into),
                env.home.clone().map(Into::into),
                || None,
            )
            .map(|home| crate::dsh_harness::managed_executable(&home, *windows)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallPathEnv {
    pub home: Option<PathBuf>,
    pub user_profile: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
}

impl InstallPathEnv {
    pub fn current() -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            user_profile: std::env::var_os("USERPROFILE").map(PathBuf::from),
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendInstallSpec {
    pub backend: Backend,
    pub platform: InstallPlatform,
    pub product_name: &'static str,
    pub vendor_name: &'static str,
    pub installer_kind: &'static str,
    pub prompt_text: &'static str,
    pub manual_install_command: &'static str,
    pub installer: InstallerPlan,
    pub fallback_location: InstallLocation,
}

impl BackendInstallSpec {
    pub fn for_backend(backend: Backend, platform: InstallPlatform) -> Self {
        match (backend, platform) {
            (Backend::Claude, InstallPlatform::MacOs | InstallPlatform::Linux) => Self {
                backend,
                platform,
                product_name: "Claude Code",
                vendor_name: "Anthropic",
                installer_kind: "native",
                prompt_text:
                    "Claude Code is not installed. Install Anthropic's native Claude Code binary now? [y/N]",
                manual_install_command: CLAUDE_POSIX_INSTALL_COMMAND,
                installer: InstallerPlan::PosixShell {
                    command: CLAUDE_POSIX_INSTALL_COMMAND,
                },
                fallback_location: InstallLocation::HomeLocalBin {
                    executable: "claude",
                },
            },
            (Backend::Claude, InstallPlatform::Windows) => Self {
                backend,
                platform,
                product_name: "Claude Code",
                vendor_name: "Anthropic",
                installer_kind: "native",
                prompt_text:
                    "Claude Code is not installed. Install Anthropic's native Claude Code binary now? [y/N]",
                manual_install_command: CLAUDE_WINDOWS_POWERSHELL_INSTALL_COMMAND,
                installer: InstallerPlan::WindowsPowerShell {
                    command: CLAUDE_WINDOWS_POWERSHELL_INSTALL_COMMAND,
                    cmd_fallback: Some(CLAUDE_WINDOWS_CMD_INSTALL_COMMAND),
                },
                fallback_location: InstallLocation::UserProfileLocalBin {
                    executable: "claude.exe",
                },
            },
            (Backend::Codex, InstallPlatform::MacOs | InstallPlatform::Linux) => Self {
                backend,
                platform,
                product_name: "Codex CLI",
                vendor_name: "OpenAI",
                installer_kind: "standalone",
                prompt_text:
                    "Codex CLI is not installed. Install OpenAI's standalone Codex CLI now? [y/N]",
                manual_install_command: if platform == InstallPlatform::Linux {
                    CODEX_LINUX_MANAGED_INSTALL_COMMAND
                } else {
                    CODEX_POSIX_INSTALL_COMMAND
                },
                installer: if platform == InstallPlatform::Linux {
                    InstallerPlan::TrustedCodex
                } else {
                    InstallerPlan::PosixShell {
                        command: CODEX_POSIX_INSTALL_COMMAND,
                    }
                },
                fallback_location: InstallLocation::HomeLocalBin {
                    executable: "codex",
                },
            },
            (Backend::Codex, InstallPlatform::Windows) => Self {
                backend,
                platform,
                product_name: "Codex CLI",
                vendor_name: "OpenAI",
                installer_kind: "standalone",
                prompt_text:
                    "Codex CLI is not installed. Install OpenAI's standalone Codex CLI now? [y/N]",
                manual_install_command: CODEX_WINDOWS_POWERSHELL_INSTALL_COMMAND,
                installer: InstallerPlan::WindowsPowerShell {
                    command: CODEX_WINDOWS_POWERSHELL_INSTALL_COMMAND,
                    cmd_fallback: None,
                },
                fallback_location: InstallLocation::LocalAppDataCodexBin,
            },
            (Backend::OpenCode, platform) => Self::for_opencode(platform),
            (Backend::DeepSeek, platform) => Self {
                backend,
                platform,
                product_name: "DeepSeek Harness",
                vendor_name: "DeepSeek AI",
                installer_kind: "clud-managed npm prefix",
                prompt_text: DEEPSEEK_PROMPT,
                manual_install_command: crate::dsh_harness::UPDATE_COMMAND,
                installer: InstallerPlan::ManagedDsh,
                fallback_location: InstallLocation::ManagedDsh {
                    windows: platform == InstallPlatform::Windows,
                },
            },
        }
    }

    fn for_opencode(platform: InstallPlatform) -> Self {
        Self {
            backend: Backend::OpenCode,
            platform,
            product_name: "OpenCode",
            vendor_name: "OpenCode",
            installer_kind: "npm",
            prompt_text:
                "OpenCode is not installed. Install opencode-ai@1.19.5 with npm now? [y/N]",
            manual_install_command: "npm install -g opencode-ai@1.19.5",
            installer: if platform == InstallPlatform::Windows {
                InstallerPlan::WindowsPowerShell {
                    command: "npm install -g opencode-ai@1.19.5",
                    cmd_fallback: None,
                }
            } else {
                InstallerPlan::PosixShell {
                    command: "npm install -g opencode-ai@1.19.5",
                }
            },
            fallback_location: InstallLocation::HomeLocalBin {
                executable: "opencode",
            },
        }
    }

    pub fn prompt(&self) -> String {
        if self.backend == Backend::DeepSeek {
            return format!(
                "{} Install DeepSeek AI's dsh {} into clud's private prefix now? [y/N]",
                self.prompt_text,
                crate::dsh_harness::DSH_VERSION
            );
        }
        self.prompt_text.to_string()
    }

    fn fallback_path(&self, env: &InstallPathEnv) -> Option<PathBuf> {
        self.fallback_location.resolve(env)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendBootstrapError {
    BackendMissing {
        backend: Backend,
    },
    BackendMissingNonInteractive {
        backend: Backend,
        product_name: &'static str,
        install_command: &'static str,
    },
    BackendInstallDeclined {
        product_name: &'static str,
        install_command: &'static str,
    },
    BackendInstallerFailed {
        product_name: &'static str,
        message: String,
        install_command: &'static str,
    },
    BackendVerificationFailed {
        backend: Backend,
        product_name: &'static str,
        message: String,
        install_command: &'static str,
    },
}

impl BackendBootstrapError {
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl fmt::Display for BackendBootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendMissing { backend } => write!(
                f,
                "error: {} not found on PATH. Install it or use --dry-run.",
                backend.executable_name()
            ),
            Self::BackendMissingNonInteractive {
                backend,
                product_name,
                install_command,
            } => write!(
                f,
                "error: {} not found on PATH. Install {product_name} with: {install_command}",
                backend.executable_name()
            ),
            Self::BackendInstallDeclined {
                product_name,
                install_command,
            } => write!(
                f,
                "error: {product_name} install declined. Install manually with: {install_command}"
            ),
            Self::BackendInstallerFailed {
                product_name,
                message,
                install_command,
            } => write!(
                f,
                "error: {product_name} install failed: {message}. Install manually with: {install_command}"
            ),
            Self::BackendVerificationFailed {
                backend,
                product_name,
                message,
                install_command,
            } => write!(
                f,
                "error: {product_name} installer completed, but clud could not verify {}: {message}. Install manually with: {install_command}",
                backend.executable_name()
            ),
        }
    }
}

pub trait BackendBootstrapHost {
    fn platform(&self) -> InstallPlatform;
    fn find_backend(&mut self, backend: Backend) -> Option<PathBuf>;
    fn run_backend_installer(&mut self, spec: &BackendInstallSpec) -> Result<(), String>;
    fn native_backend_path(&self, spec: &BackendInstallSpec) -> Option<PathBuf>;
    fn verify_backend(&mut self, backend: Backend, path: &Path) -> Result<(), String>;
    /// A binary left by clud's temp-home install fallback, consulted only
    /// after `PATH` and the native location miss.
    fn temp_install_path(&self, spec: &BackendInstallSpec) -> Option<PathBuf> {
        let _ = spec;
        None
    }
}

pub struct ProductionBackendBootstrapHost;

impl BackendBootstrapHost for ProductionBackendBootstrapHost {
    fn platform(&self) -> InstallPlatform {
        InstallPlatform::current()
    }

    fn find_backend(&mut self, backend: Backend) -> Option<PathBuf> {
        backend::find_backend(backend)
    }

    fn run_backend_installer(&mut self, spec: &BackendInstallSpec) -> Result<(), String> {
        run_backend_installer(spec)
    }

    fn native_backend_path(&self, spec: &BackendInstallSpec) -> Option<PathBuf> {
        spec.fallback_path(&InstallPathEnv::current())
    }

    fn verify_backend(&mut self, backend: Backend, path: &Path) -> Result<(), String> {
        verify_backend(backend, path)
    }

    fn temp_install_path(&self, spec: &BackendInstallSpec) -> Option<PathBuf> {
        (spec.backend == Backend::Claude)
            .then(crate::claude_temp_home_install::discover)
            .flatten()
    }
}

/// Locate an already-installed backend for callers outside the install
/// flow. Same PATH-then-native-location rule the bootstrap gate uses, so a
/// stale `PATH` doesn't make a present binary look missing (issue #934).
pub fn locate_installed_backend(backend: Backend) -> Option<PathBuf> {
    let mut host = ProductionBackendBootstrapHost;
    let spec = BackendInstallSpec::for_backend(backend, host.platform());
    locate_backend(&mut host, backend, &spec)
}

/// Locate an already-installed backend: `PATH` first, then the documented
/// native install location for this platform.
///
/// Issue #934: both the pre-prompt gate and the post-install verification
/// go through here, so "installed" means the same thing on both sides. When
/// they disagreed, a PATH-only miss escalated to a full reinstall of a
/// binary that was already on disk.
fn locate_backend<H>(host: &mut H, backend: Backend, spec: &BackendInstallSpec) -> Option<PathBuf>
where
    H: BackendBootstrapHost + ?Sized,
{
    if let Some(path) = host.find_backend(backend) {
        return Some(path);
    }
    host.native_backend_path(spec)
        .filter(|path| path.is_file())
        .or_else(|| host.temp_install_path(spec))
}

pub fn resolve_backend_path<R, W, H>(
    backend: Backend,
    dry_run: bool,
    interactive: bool,
    input: &mut R,
    err: &mut W,
    host: &mut H,
) -> Result<String, BackendBootstrapError>
where
    R: BufRead,
    W: Write,
    H: BackendBootstrapHost,
{
    let spec = BackendInstallSpec::for_backend(backend, host.platform());

    // Issue #934: PATH first, then the documented native install location.
    // Checking only PATH here meant a transient PATH miss (stale shell, an
    // updater mid-swap) prompted for a full reinstall of a binary already
    // sitting at the fallback location — which is the very path this
    // function accepts *after* the installer runs.
    if let Some(path) = locate_backend(host, backend, &spec) {
        return Ok(path.to_string_lossy().to_string());
    }

    if dry_run {
        return Ok(backend.executable_name().to_string());
    }

    if !interactive {
        return Err(BackendBootstrapError::BackendMissingNonInteractive {
            backend,
            product_name: spec.product_name,
            install_command: spec.manual_install_command,
        });
    }

    // Issue #934: name what was actually checked, so a false "not installed"
    // is diagnosable from the transcript instead of being a mystery.
    match host.native_backend_path(&spec) {
        Some(fallback) => writeln!(
            err,
            "[clud] {} not found on PATH, and no binary at {}",
            backend.executable_name(),
            fallback.display()
        ),
        None => writeln!(
            err,
            "[clud] {} not found on PATH",
            backend.executable_name()
        ),
    }
    .ok();
    writeln!(err, "\n{}", spec.prompt()).ok();
    err.flush().ok();

    if !read_yes(input) {
        return Err(BackendBootstrapError::BackendInstallDeclined {
            product_name: spec.product_name,
            install_command: spec.manual_install_command,
        });
    }

    host.run_backend_installer(&spec).map_err(|message| {
        BackendBootstrapError::BackendInstallerFailed {
            product_name: spec.product_name,
            message,
            install_command: spec.manual_install_command,
        }
    })?;

    let Some(path) = locate_backend(host, backend, &spec) else {
        return Err(BackendBootstrapError::BackendVerificationFailed {
            backend,
            product_name: spec.product_name,
            message: format!(
                "installed binary was not found on PATH or at the default {} install location",
                spec.product_name
            ),
            install_command: spec.manual_install_command,
        });
    };

    host.verify_backend(backend, &path).map_err(|message| {
        BackendBootstrapError::BackendVerificationFailed {
            backend,
            product_name: spec.product_name,
            message,
            install_command: spec.manual_install_command,
        }
    })?;
    Ok(path.to_string_lossy().to_string())
}

fn read_yes<R: BufRead>(input: &mut R) -> bool {
    let mut line = String::new();
    if input.read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES" | "Yes")
}

pub fn official_install_command(backend: Backend, platform: InstallPlatform) -> &'static str {
    BackendInstallSpec::for_backend(backend, platform).manual_install_command
}

pub fn official_claude_install_command() -> &'static str {
    official_install_command(Backend::Claude, InstallPlatform::current())
}

/// Only this fixed, hash-checked installer is given a system-tool PATH. Session
/// shells retain the removal shim; neither shell text nor a script path is an
/// argument to this entry point.
pub fn run_trusted_codex_update() -> i32 {
    match trusted_codex_update() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Codex update refused: {error}");
            1
        }
    }
}

fn trusted_codex_update() -> Result<(), String> {
    if !host::LINUX {
        return Err("Codex standalone update is supported here only on Linux".into());
    }
    let home = std::env::var_os("HOME").ok_or("HOME is required")?;
    let home = std::fs::canonicalize(home).map_err(|e| format!("cannot resolve HOME: {e}"))?;
    let response = ureq::get(CODEX_TRUSTED_INSTALLER_URL)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| format!("cannot fetch Codex installer: {e}"))?;
    if response.get_url() != CODEX_TRUSTED_INSTALLER_URL {
        return Err("Codex installer redirected away from the audited origin".to_string());
    }
    let mut script = Vec::new();
    response
        .into_reader()
        .take(128 * 1024 + 1)
        .read_to_end(&mut script)
        .map_err(|e| format!("cannot read Codex installer: {e}"))?;
    if script.len() > 128 * 1024 {
        return Err("Codex installer exceeds the reviewed size limit".to_string());
    }
    run_verified_codex_installer(&script, CODEX_TRUSTED_INSTALLER_SHA256, &home)
}

pub(crate) fn trusted_codex_update_env(
    home: &Path,
    original_path: Option<&str>,
    shell: Option<&str>,
) -> Vec<(String, String)> {
    let system_path = if Path::new("/run/current-system/sw/bin/mkdir").is_file() {
        "/run/current-system/sw/bin:/usr/bin:/bin"
    } else {
        "/usr/bin:/bin"
    };
    let mut path = system_path.to_string();
    let visible_bin = home.join(".local/bin");
    let visible_bin = visible_bin.to_string_lossy();
    if !visible_bin.contains(':')
        && original_path
            .is_some_and(|value| value.split(':').any(|part| part == visible_bin.as_ref()))
    {
        path.push(':');
        path.push_str(&visible_bin);
    }
    let mut env = vec![
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("PATH".into(), path),
        ("CODEX_NON_INTERACTIVE".into(), "1".into()),
    ];
    if let Some(shell) = shell {
        env.push(("SHELL".into(), shell.into()));
    }
    env
}

pub(crate) fn run_verified_codex_installer(
    script: &[u8],
    expected_sha256: &str,
    home: &Path,
) -> Result<(), String> {
    let parent_env: Vec<_> = std::env::vars().collect();
    run_verified_codex_installer_with_parent_env(script, expected_sha256, home, &parent_env)
}

pub(crate) fn run_verified_codex_installer_with_parent_env(
    script: &[u8],
    expected_sha256: &str,
    home: &Path,
    parent_env: &[(String, String)],
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let actual = format!("{:x}", Sha256::digest(script));
    if actual != expected_sha256 {
        return Err(format!(
            "installer contents changed (sha256 {actual}); review the new script before updating the pin"
        ));
    }
    let temp = tempfile::tempdir().map_err(|e| format!("cannot stage installer: {e}"))?;
    let path = temp.path().join("install.sh");
    std::fs::write(&path, script).map_err(|e| format!("cannot write installer: {e}"))?;
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["/bin/sh".into(), path.to_string_lossy().into_owned()]),
        cwd: Some(home.to_path_buf()),
        env: Some(trusted_codex_update_env(
            home,
            parent_env
                .iter()
                .find(|(key, _)| key == "PATH")
                .map(|(_, value)| value.as_str()),
            parent_env
                .iter()
                .find(|(key, _)| key == "SHELL")
                .map(|(_, value)| value.as_str()),
        )),
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    process
        .start()
        .map_err(|e| format!("cannot start installer: {e}"))?;
    let code = process
        .wait(None)
        .map_err(|e| format!("cannot wait for installer: {e}"))?;
    if code == 0 {
        Ok(())
    } else {
        Err(format!("installer exited with {code}"))
    }
}

fn run_backend_installer(spec: &BackendInstallSpec) -> Result<(), String> {
    match spec.installer {
        InstallerPlan::TrustedCodex => trusted_codex_update(),
        InstallerPlan::PosixShell { command } => match run_posix_installer(command, None) {
            Err(first) if spec.backend == Backend::Claude => install_claude_in_temp_home(command)
                .map_err(|second| {
                    format!("{first}; retrying with a temporary install home also failed: {second}")
                }),
            other => other,
        },
        InstallerPlan::WindowsPowerShell {
            command,
            cmd_fallback,
        } => run_windows_powershell_installer(command, cmd_fallback, spec.backend),
        InstallerPlan::ManagedDsh => {
            let home = crate::dsh_harness::managed_home().ok_or("home directory unavailable")?;
            crate::dsh_harness::install(&home).map(|_| ())
        }
    }
}

fn run_posix_installer(
    command: &'static str,
    env: Option<Vec<(String, String)>>,
) -> Result<(), String> {
    run_interactive_command_with_env(
        CommandSpec::Argv(vec![
            "sh".to_string(),
            "-c".to_string(),
            command.to_string(),
        ]),
        None,
        env,
    )
}

/// The home volume of a live USB is too small for Claude Code; rerun the
/// installer with `HOME` in the (usually RAM-backed) temp dir and link the
/// binary back. See `claude_temp_home_install`.
fn install_claude_in_temp_home(command: &'static str) -> Result<(), String> {
    use crate::claude_temp_home_install as temp_install;
    let real_home = crate::home::user_home().ok_or("home directory unavailable")?;
    let temp_home = temp_install::create_temp_home()?;
    eprintln!("{}", temp_install::engaged_warning(&real_home, &temp_home));
    let parent: Vec<_> = std::env::vars().collect();
    let installed = run_posix_installer(
        command,
        Some(temp_install::installer_env(&parent, &temp_home)),
    )
    .and_then(|()| temp_install::finish(&temp_home, &real_home));
    match installed {
        Ok(done) => {
            eprintln!("{}", temp_install::installed_warning(&done));
            Ok(())
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&temp_home);
            Err(err)
        }
    }
}

fn run_windows_powershell_installer(
    command: &'static str,
    cmd_fallback: Option<&'static str>,
    backend: Backend,
) -> Result<(), String> {
    let argv = vec![
        "powershell.exe".to_string(),
        "-NoProfile".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-Command".to_string(),
        command.to_string(),
    ];

    match run_interactive_command(CommandSpec::Argv(argv), None) {
        Ok(()) => Ok(()),
        Err(first) => {
            let Some(fallback) = cmd_fallback else {
                return Err(first);
            };
            run_windows_cmd_installer(fallback, backend).map_err(|second| {
                format!("PowerShell installer failed ({first}); CMD fallback failed ({second})")
            })
        }
    }
}

fn run_windows_cmd_installer(command: &'static str, backend: Backend) -> Result<(), String> {
    if !host::WINDOWS {
        return Err("CMD fallback is only available on Windows".to_string());
    }
    let dir = std::env::temp_dir().join(format!(
        "clud-{}-install-{}",
        backend.executable_name(),
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create temp dir {}: {err}", dir.display()))?;
    let result = run_interactive_command(
        CommandSpec::Argv(vec![
            "cmd.exe".to_string(),
            "/D".to_string(),
            "/S".to_string(),
            "/C".to_string(),
            command.to_string(),
        ]),
        Some(dir.clone()),
    );
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn run_interactive_command(command: CommandSpec, cwd: Option<PathBuf>) -> Result<(), String> {
    run_interactive_command_with_env(command, cwd, None)
}

fn run_interactive_command_with_env(
    command: CommandSpec,
    cwd: Option<PathBuf>,
    env: Option<Vec<(String, String)>>,
) -> Result<(), String> {
    let process = NativeProcess::new(ProcessConfig {
        command,
        cwd,
        env,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    process
        .start()
        .map_err(|err| format!("failed to start command: {err}"))?;
    let exit_code = process
        .wait(None)
        .map_err(|err| format!("failed to wait for command: {err}"))?;
    if exit_code == 0 {
        Ok(())
    } else {
        Err(format!("command exited with {exit_code}"))
    }
}

fn verify_backend(backend: Backend, path: &Path) -> Result<(), String> {
    // A managed dsh runs on its private Node, which `--version` must see too.
    if backend == Backend::DeepSeek {
        if let Some(home) = crate::dsh_harness::managed_home() {
            if crate::dsh_harness::managed_bin_for(path, &home).is_some() {
                return crate::dsh_harness::verify(path);
            }
        }
    }
    let command = vec![path.to_string_lossy().to_string(), "--version".to_string()];
    let (exit_code, output) = run_captured_command(command)
        .map_err(|err| format!("failed to run {} --version: {err}", path.display()))?;
    if exit_code == 0 {
        Ok(())
    } else {
        Err(format!(
            "{} --version exited with {}; output: {}",
            path.display(),
            exit_code,
            output.trim()
        ))
    }
}

/// Unified model discovery accepts clud's synthetic IDs only in Claude Code
/// 2.1.223 and newer. Probe the executable selected by bootstrap so a stale
/// client fails before the launch-scoped gateway or any paid request starts.
pub fn require_unified_claude_version(path: &Path) -> Result<ClaudeCodeVersion, String> {
    require_claude_discovery_version(path, "unified routing")
}

/// The direct Codex-through-Claude route now uses the same synthetic model
/// discovery surface as unified routing, and therefore has the same client
/// floor.
pub fn require_codex_bridge_claude_version(path: &Path) -> Result<ClaudeCodeVersion, String> {
    require_claude_discovery_version(path, "Codex-through-Claude model discovery")
}

fn require_claude_discovery_version(
    path: &Path,
    feature: &str,
) -> Result<ClaudeCodeVersion, String> {
    let command = vec![path.to_string_lossy().to_string(), "--version".to_string()];
    let (exit_code, output) = run_captured_command(command).map_err(|error| {
        format!(
            "{feature} requires Claude Code >= {MIN_UNIFIED_CLAUDE_CODE_VERSION}, but the installed client version could not be checked: {error}"
        )
    })?;
    if exit_code != 0 {
        return Err(format!(
            "{feature} requires Claude Code >= {MIN_UNIFIED_CLAUDE_CODE_VERSION}, but the installed client's --version command exited with {exit_code}"
        ));
    }
    validate_claude_discovery_version_output(&output, feature)
}

#[cfg(test)]
fn validate_unified_claude_version_output(output: &str) -> Result<ClaudeCodeVersion, String> {
    validate_claude_discovery_version_output(output, "unified routing")
}

fn validate_claude_discovery_version_output(
    output: &str,
    feature: &str,
) -> Result<ClaudeCodeVersion, String> {
    let installed = parse_claude_code_version(output).ok_or_else(|| {
        format!(
            "{feature} requires Claude Code >= {MIN_UNIFIED_CLAUDE_CODE_VERSION}, but no installed version could be parsed from `claude --version`"
        )
    })?;
    if installed < MIN_UNIFIED_CLAUDE_CODE_VERSION {
        return Err(format!(
            "{feature} requires Claude Code >= {MIN_UNIFIED_CLAUDE_CODE_VERSION}; installed version is {installed}. Run `claude update` and retry"
        ));
    }
    Ok(installed)
}

fn parse_claude_code_version(output: &str) -> Option<ClaudeCodeVersion> {
    output
        .split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .find_map(|candidate| {
            let mut parts = candidate.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            Some(ClaudeCodeVersion {
                major,
                minor,
                patch,
            })
        })
}

/// Whether the installed Claude Code can fire the `CwdChanged` hook event
/// (zackees/clud#967 Phase 5).
///
/// The backstop is hygiene, never correctness (DD-064), so the probe is
/// deliberately conservative: any failure — the binary missing or hanging,
/// output that will not parse, an older version — answers `false`, and the
/// launch simply registers no `CwdChanged` line.
#[must_use]
pub fn probe_claude_cwd_changed_support(claude_path: &Path) -> bool {
    probe_claude_code_version(claude_path)
        .is_some_and(|version| version >= MIN_CWD_CHANGED_CLAUDE_CODE_VERSION)
}

/// The version floor for the `WorktreeCreate` / `WorktreeRemove` hook events
/// (#1962, #1971), which Claude Code introduced in 2.1.50. Before 2.1.101 an
/// unrecognized hook event made Claude Code ignore a whole settings
/// document, so the lines must never reach an older client.
const MIN_WORKTREE_HOOKS_CLAUDE_CODE_VERSION: ClaudeCodeVersion = ClaudeCodeVersion {
    major: 2,
    minor: 1,
    patch: 50,
};

/// Whether `version` knows the `WorktreeCreate` / `WorktreeRemove` events.
#[must_use]
pub fn supports_worktree_hooks(version: ClaudeCodeVersion) -> bool {
    version >= MIN_WORKTREE_HOOKS_CLAUDE_CODE_VERSION
}

/// Whether the installed Claude Code can take clud's worktree hooks; any
/// probe failure answers `false`, which keeps Claude Code's own placement.
#[must_use]
pub fn probe_claude_worktree_hooks_support(claude_path: &Path) -> bool {
    probe_claude_code_version(claude_path).is_some_and(supports_worktree_hooks)
}

/// `claude --version`, parsed, once per binary per process: a launch asks
/// several questions of the same client. `None` on any failure. The probe
/// gives up after a few seconds rather than stall a launch.
#[must_use]
pub fn probe_claude_code_version(claude_path: &Path) -> Option<ClaudeCodeVersion> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<ClaudeCodeVersion>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(claude_path).copied())
    {
        return found;
    }
    let version = run_version_probe(claude_path);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(claude_path.to_path_buf(), version);
    }
    version
}

fn run_version_probe(claude_path: &Path) -> Option<ClaudeCodeVersion> {
    const PROBE_BUDGET: Duration = Duration::from_secs(5);

    let process = subprocess::ManagedSubprocess::start_inheriting_env(
        vec![
            claude_path.to_string_lossy().into_owned(),
            "--version".to_string(),
        ],
        None,
        true,
        invisible_helper_creationflags(),
    )
    .ok()?;

    let deadline = std::time::Instant::now() + PROBE_BUDGET;
    let mut buf = Vec::<u8>::new();
    loop {
        if std::time::Instant::now() >= deadline {
            let _ = process.kill();
            return None;
        }
        match process.read_stdout(Some(Duration::from_millis(100))) {
            ReadStatus::Line(line) => buf.extend_from_slice(&line),
            ReadStatus::Timeout => {
                // Polling observes direct-root exit and closes its Job, while
                // reader threads may still be draining buffered final bytes —
                // same pattern as `run_captured_command`; the deadline above
                // is what bounds this loop.
                let _ = process.poll();
            }
            ReadStatus::Eof => break,
        }
    }

    let exit_code = process
        .wait(Some(Duration::from_secs(2)))
        .map_err(|_| ())
        .ok();
    if exit_code != Some(0) {
        return None;
    }
    parse_claude_code_version(&String::from_utf8_lossy(&buf))
}

pub(crate) fn run_captured_command(command: Vec<String>) -> Result<(i32, String), String> {
    let process = subprocess::ManagedSubprocess::start_inheriting_env(
        command,
        None,
        true,
        invisible_helper_creationflags(),
    )
    .map_err(|err| format!("failed to start command: {err}"))?;

    let mut buf = Vec::<u8>::new();
    loop {
        match process.read_stdout(Some(Duration::from_millis(100))) {
            ReadStatus::Line(line) => {
                buf.extend_from_slice(&line);
            }
            ReadStatus::Timeout => {
                // Polling observes direct-root exit and closes its Job, but
                // reader threads can still be draining buffered final bytes.
                // Continue until EOF synchronizes with both readers.
                let _ = process.poll();
            }
            ReadStatus::Eof => break,
        }
    }

    let exit_code = process
        .wait(Some(Duration::from_secs(30)))
        .map_err(|err| format!("failed to wait for command: {err}"))?;
    Ok((exit_code, String::from_utf8_lossy(&buf).into_owned()))
}

/// Like [`run_captured_command`] but bounded: a child still producing no EOF
/// after `limit` is killed and reported as an error (#2302 review: a stalled
/// `soldr` must not block the GC thread forever).
pub(crate) fn run_captured_command_with_deadline(
    command: Vec<String>,
    limit: Duration,
) -> Result<(i32, String), String> {
    let process = subprocess::ManagedSubprocess::start_inheriting_env(
        command,
        None,
        true,
        invisible_helper_creationflags(),
    )
    .map_err(|err| format!("failed to start command: {err}"))?;
    let drained = drain_until(
        std::time::Instant::now() + limit,
        || process.read_stdout(Some(Duration::from_millis(100))),
        || {
            let _ = process.poll();
        },
    );
    let Some(buf) = drained else {
        let _ = process.kill();
        return Err(format!("timed out after {}s", limit.as_secs()));
    };
    let exit_code = process
        .wait(Some(Duration::from_secs(30)))
        .map_err(|err| format!("failed to wait for command: {err}"))?;
    Ok((exit_code, String::from_utf8_lossy(&buf).into_owned()))
}

/// Read until EOF; `None` once `deadline` passes first.
fn drain_until(
    deadline: std::time::Instant,
    mut read: impl FnMut() -> ReadStatus<Vec<u8>>,
    mut on_timeout: impl FnMut(),
) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        match read() {
            ReadStatus::Line(line) => buf.extend_from_slice(&line),
            ReadStatus::Timeout => on_timeout(),
            ReadStatus::Eof => return Some(buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io;

    #[test]
    fn drain_until_gives_up_on_a_reader_that_never_finishes() {
        let started = std::time::Instant::now();
        let got = drain_until(
            started + Duration::from_millis(150),
            || ReadStatus::Timeout,
            || {},
        );
        assert!(got.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// #1962/#1971: `WorktreeCreate` exists from Claude Code 2.1.50.
    #[test]
    fn worktree_hooks_need_claude_code_2_1_50() {
        let version = |patch| ClaudeCodeVersion {
            major: 2,
            minor: 1,
            patch,
        };
        assert!(supports_worktree_hooks(version(50)));
        assert!(supports_worktree_hooks(version(300)));
        assert!(!supports_worktree_hooks(version(49)));
        assert_eq!(
            parse_claude_code_version("2.1.301 (Claude Code)").map(supports_worktree_hooks),
            Some(true)
        );
    }

    #[test]
    fn unified_claude_version_parser_accepts_supported_output_shapes() {
        assert_eq!(
            parse_claude_code_version("2.1.223 (Claude Code)"),
            Some(MIN_UNIFIED_CLAUDE_CODE_VERSION)
        );
        assert_eq!(
            parse_claude_code_version("Claude Code v2.2.0\n"),
            Some(ClaudeCodeVersion {
                major: 2,
                minor: 2,
                patch: 0,
            })
        );
        assert_eq!(parse_claude_code_version("Claude Code unknown"), None);
    }

    #[test]
    fn cwd_changed_probe_floor_is_the_event_introduction_version() {
        // 2.1.83 is the first Claude Code that fires `CwdChanged`; anything
        // older must not get the backstop line, and the probe must accept
        // exactly the floor and above.
        let floor = MIN_CWD_CHANGED_CLAUDE_CODE_VERSION;
        assert_eq!(
            floor,
            ClaudeCodeVersion {
                major: 2,
                minor: 1,
                patch: 83
            }
        );
        assert!(parse_claude_code_version("2.1.82").is_none_or(|v| v < floor));
        assert!(parse_claude_code_version("2.1.83").is_some_and(|v| v >= floor));
        assert!(parse_claude_code_version("2.2.0").is_some_and(|v| v >= floor));
        assert!(parse_claude_code_version("1.9.99").is_none_or(|v| v < floor));
        assert!(parse_claude_code_version("3.0.0").is_some_and(|v| v >= floor));
    }

    #[test]
    fn unified_claude_version_floor_rejects_every_older_discovery_build() {
        let error = validate_unified_claude_version_output("2.1.222").unwrap_err();
        assert!(error.contains("installed version is 2.1.222"), "{error}");
        assert!(error.contains("claude update"), "{error}");
        assert_eq!(
            validate_unified_claude_version_output("2.1.223").unwrap(),
            MIN_UNIFIED_CLAUDE_CODE_VERSION
        );
    }

    /// Issue #921's exact repro shape: a 2.1.212 client with the
    /// `(Claude Code)` suffix must be refused naming its version, the floor,
    /// and the remedy — and missing/unparseable output must refuse too, never
    /// a silent picker without connector rows.
    #[test]
    fn unified_claude_version_floor_rejects_the_212_repro_and_unparseable_output() {
        let error = validate_unified_claude_version_output("2.1.212 (Claude Code)").unwrap_err();
        assert!(error.contains("installed version is 2.1.212"), "{error}");
        assert!(error.contains("2.1.223"), "{error}");
        assert!(error.contains("claude update"), "{error}");
        for output in ["", "no version here"] {
            let error = validate_unified_claude_version_output(output).unwrap_err();
            assert!(
                error.contains("no installed version could be parsed"),
                "{error}"
            );
            assert!(error.contains("2.1.223"), "{error}");
        }
    }

    struct MockHost {
        platform: InstallPlatform,
        find_results: VecDeque<Option<PathBuf>>,
        find_calls: Vec<Backend>,
        installer_runs: Vec<Backend>,
        installer_result: Result<(), String>,
        native_path: Option<PathBuf>,
        /// Path the installer materializes when it runs. Models the real
        /// world, where the fallback location is empty *before* the install
        /// and populated after — the distinction the pre-prompt gate turns on.
        installer_creates: Option<PathBuf>,
        verified: Vec<(Backend, PathBuf)>,
        verify_result: Result<(), String>,
        temp_install: Option<PathBuf>,
    }

    impl Default for MockHost {
        fn default() -> Self {
            Self {
                platform: InstallPlatform::Linux,
                find_results: VecDeque::new(),
                find_calls: Vec::new(),
                installer_runs: Vec::new(),
                installer_result: Ok(()),
                native_path: None,
                installer_creates: None,
                verified: Vec::new(),
                verify_result: Ok(()),
                temp_install: None,
            }
        }
    }

    impl BackendBootstrapHost for MockHost {
        fn platform(&self) -> InstallPlatform {
            self.platform
        }

        fn find_backend(&mut self, backend: Backend) -> Option<PathBuf> {
            self.find_calls.push(backend);
            self.find_results.pop_front().unwrap_or(None)
        }

        fn run_backend_installer(&mut self, spec: &BackendInstallSpec) -> Result<(), String> {
            self.installer_runs.push(spec.backend);
            if self.installer_result.is_ok() {
                if let Some(path) = &self.installer_creates {
                    std::fs::write(path, b"mock").expect("installer writes binary");
                }
            }
            self.installer_result.clone()
        }

        fn native_backend_path(&self, spec: &BackendInstallSpec) -> Option<PathBuf> {
            let _ = spec;
            self.native_path.clone()
        }

        fn verify_backend(&mut self, backend: Backend, path: &Path) -> Result<(), String> {
            self.verified.push((backend, path.to_path_buf()));
            self.verify_result.clone()
        }

        fn temp_install_path(&self, _spec: &BackendInstallSpec) -> Option<PathBuf> {
            self.temp_install.clone()
        }
    }

    /// The temp-home fallback's binary is used, without a prompt, when
    /// `PATH` and `~/.local/bin/claude` both miss.
    #[test]
    fn temp_install_is_used_when_path_and_native_location_miss() {
        let temp = PathBuf::from("/tmp/clud-claude-home-x/.local/bin/claude");
        let mut host = MockHost {
            native_path: Some(PathBuf::from("/nonexistent/.local/bin/claude")),
            temp_install: Some(temp.clone()),
            ..Default::default()
        };
        let (path, err) = resolve_with(Backend::Claude, false, false, "", &mut host).expect("path");
        assert_eq!(path, temp.to_string_lossy());
        assert!(err.is_empty(), "no prompt: {err}");
        assert!(host.installer_runs.is_empty());
    }

    /// After a reboot wipes tmpfs, the link dangles and no temp home is
    /// left, so clud asks to install again.
    #[test]
    fn reboot_wiped_temp_install_prompts_to_install_again() {
        let home = tempfile::tempdir().expect("tempdir");
        let link = home.path().join("claude");
        if !crate::platform::host::WINDOWS {
            crate::platform::alias_link::symlink(Path::new("/nonexistent/claude"), &link)
                .expect("dangling link");
        }
        let mut host = MockHost {
            native_path: Some(link),
            installer_result: Err("declined in test".to_string()),
            ..Default::default()
        };
        let err = resolve_with(Backend::Claude, false, true, "n\n", &mut host).unwrap_err();
        assert!(matches!(
            err,
            BackendBootstrapError::BackendInstallDeclined { .. }
        ));
    }

    fn resolve_with(
        backend: Backend,
        dry_run: bool,
        interactive: bool,
        input: &str,
        host: &mut MockHost,
    ) -> Result<(String, String), BackendBootstrapError> {
        let mut input = io::Cursor::new(input.as_bytes().to_vec());
        let mut err = Vec::<u8>::new();
        let path = resolve_backend_path(backend, dry_run, interactive, &mut input, &mut err, host)?;
        Ok((path, String::from_utf8(err).expect("utf8")))
    }

    fn path_env() -> InstallPathEnv {
        InstallPathEnv {
            home: Some(PathBuf::from("/home/me")),
            user_profile: Some(PathBuf::from("C:/Users/me")),
            local_app_data: Some(PathBuf::from("C:/Users/me/AppData/Local")),
        }
    }

    #[test]
    fn claude_macos_install_spec_uses_posix_command_and_home_local_bin() {
        let spec = BackendInstallSpec::for_backend(Backend::Claude, InstallPlatform::MacOs);
        assert_eq!(spec.manual_install_command, CLAUDE_POSIX_INSTALL_COMMAND);
        assert_eq!(
            spec.prompt(),
            "Claude Code is not installed. Install Anthropic's native Claude Code binary now? [y/N]"
        );
        assert_eq!(
            spec.installer,
            InstallerPlan::PosixShell {
                command: CLAUDE_POSIX_INSTALL_COMMAND
            }
        );
        assert_eq!(
            spec.fallback_path(&path_env()).unwrap(),
            PathBuf::from("/home/me/.local/bin/claude")
        );
    }

    #[test]
    fn claude_linux_install_spec_uses_posix_command_and_home_local_bin() {
        let spec = BackendInstallSpec::for_backend(Backend::Claude, InstallPlatform::Linux);
        assert_eq!(spec.manual_install_command, CLAUDE_POSIX_INSTALL_COMMAND);
        assert_eq!(
            spec.installer,
            InstallerPlan::PosixShell {
                command: CLAUDE_POSIX_INSTALL_COMMAND
            }
        );
        assert_eq!(
            spec.fallback_path(&path_env()).unwrap(),
            PathBuf::from("/home/me/.local/bin/claude")
        );
    }

    #[test]
    fn claude_windows_install_spec_uses_powershell_cmd_fallback_and_user_profile_path() {
        let spec = BackendInstallSpec::for_backend(Backend::Claude, InstallPlatform::Windows);
        assert_eq!(
            spec.manual_install_command,
            CLAUDE_WINDOWS_POWERSHELL_INSTALL_COMMAND
        );
        assert_eq!(
            spec.installer,
            InstallerPlan::WindowsPowerShell {
                command: CLAUDE_WINDOWS_POWERSHELL_INSTALL_COMMAND,
                cmd_fallback: Some(CLAUDE_WINDOWS_CMD_INSTALL_COMMAND)
            }
        );
        assert_eq!(
            spec.fallback_path(&path_env()).unwrap(),
            PathBuf::from("C:/Users/me")
                .join(".local")
                .join("bin")
                .join("claude.exe")
        );
    }

    #[test]
    fn codex_macos_install_spec_uses_standalone_command_and_home_local_bin() {
        let spec = BackendInstallSpec::for_backend(Backend::Codex, InstallPlatform::MacOs);
        assert_eq!(spec.manual_install_command, CODEX_POSIX_INSTALL_COMMAND);
        assert_eq!(
            spec.prompt(),
            "Codex CLI is not installed. Install OpenAI's standalone Codex CLI now? [y/N]"
        );
        assert_eq!(
            spec.installer,
            InstallerPlan::PosixShell {
                command: CODEX_POSIX_INSTALL_COMMAND
            }
        );
        assert_eq!(
            spec.fallback_path(&path_env()).unwrap(),
            PathBuf::from("/home/me/.local/bin/codex")
        );
    }

    #[test]
    fn deepseek_install_spec_is_a_managed_prefix_with_clud_update_guidance() {
        for platform in [
            InstallPlatform::MacOs,
            InstallPlatform::Linux,
            InstallPlatform::Windows,
        ] {
            let spec = BackendInstallSpec::for_backend(Backend::DeepSeek, platform);
            let windows = platform == InstallPlatform::Windows;
            assert_eq!(spec.product_name, "DeepSeek Harness");
            assert_eq!(spec.manual_install_command, "clud dsh-update");
            assert_eq!(spec.installer, InstallerPlan::ManagedDsh);
            assert_eq!(
                spec.fallback_location,
                InstallLocation::ManagedDsh { windows }
            );
            assert_eq!(
                spec.fallback_path(&path_env()).unwrap(),
                // Windows resolves USERPROFILE first, like every clud path.
                crate::dsh_harness::managed_executable(
                    Path::new(if windows { "C:/Users/me" } else { "/home/me" }),
                    windows
                )
            );
            assert!(spec.prompt().contains(crate::dsh_harness::DSH_VERSION));
            assert!(spec.prompt().ends_with("[y/N]"));
        }
    }

    #[test]
    fn codex_linux_install_spec_uses_standalone_command_and_home_local_bin() {
        let spec = BackendInstallSpec::for_backend(Backend::Codex, InstallPlatform::Linux);
        assert_eq!(
            spec.manual_install_command,
            CODEX_LINUX_MANAGED_INSTALL_COMMAND
        );
        assert_eq!(spec.installer, InstallerPlan::TrustedCodex);
        assert_eq!(
            spec.fallback_path(&path_env()).unwrap(),
            PathBuf::from("/home/me/.local/bin/codex")
        );
    }

    #[test]
    fn codex_windows_install_spec_uses_powershell_without_cmd_fallback_and_local_app_data_path() {
        let spec = BackendInstallSpec::for_backend(Backend::Codex, InstallPlatform::Windows);
        assert_eq!(
            spec.manual_install_command,
            CODEX_WINDOWS_POWERSHELL_INSTALL_COMMAND
        );
        assert_eq!(
            spec.installer,
            InstallerPlan::WindowsPowerShell {
                command: CODEX_WINDOWS_POWERSHELL_INSTALL_COMMAND,
                cmd_fallback: None
            }
        );
        assert_eq!(
            spec.fallback_path(&path_env()).unwrap(),
            PathBuf::from("C:/Users/me/AppData/Local")
                .join("Programs")
                .join("OpenAI")
                .join("Codex")
                .join("bin")
                .join("codex.exe")
        );
    }

    #[test]
    fn existing_backend_path_wins() {
        let mut host = MockHost {
            find_results: VecDeque::from([Some(PathBuf::from("/bin/claude"))]),
            ..Default::default()
        };
        let (path, prompt) =
            resolve_with(Backend::Claude, false, true, "", &mut host).expect("path");
        assert_eq!(path, "/bin/claude");
        assert!(prompt.is_empty());
        assert!(host.installer_runs.is_empty());
        assert!(host.verified.is_empty());
    }

    #[test]
    fn dry_run_uses_placeholder_without_installing() {
        for backend in Backend::ALL {
            let mut host = MockHost::default();
            let (path, prompt) = resolve_with(backend, true, true, "y\n", &mut host).expect("path");
            assert_eq!(path, backend.executable_name());
            assert!(prompt.is_empty());
            assert!(host.installer_runs.is_empty());
            assert!(host.verified.is_empty());
        }
    }

    #[test]
    fn claude_missing_noninteractive_prints_install_command() {
        let mut host = MockHost::default();
        let err = resolve_with(Backend::Claude, false, false, "", &mut host).unwrap_err();
        assert_eq!(
            err,
            BackendBootstrapError::BackendMissingNonInteractive {
                backend: Backend::Claude,
                product_name: "Claude Code",
                install_command: CLAUDE_POSIX_INSTALL_COMMAND
            }
        );
        assert!(err.to_string().contains("claude.ai/install"));
        assert!(host.installer_runs.is_empty());
    }

    #[test]
    fn codex_missing_noninteractive_prints_install_command() {
        let mut host = MockHost::default();
        let err = resolve_with(Backend::Codex, false, false, "", &mut host).unwrap_err();
        assert_eq!(
            err,
            BackendBootstrapError::BackendMissingNonInteractive {
                backend: Backend::Codex,
                product_name: "Codex CLI",
                install_command: CODEX_LINUX_MANAGED_INSTALL_COMMAND
            }
        );
        assert!(err.to_string().contains("clud codex-update"));
        assert!(host.installer_runs.is_empty());
    }

    #[test]
    fn deepseek_missing_interactively_prompts_and_installs_on_yes() {
        let mut host = MockHost::default();
        let _ = resolve_with(Backend::DeepSeek, false, true, "y\n", &mut host);
        assert_eq!(host.installer_runs, vec![Backend::DeepSeek]);
    }

    #[test]
    fn deepseek_missing_interactively_declined_installs_nothing() {
        for answer in ["n\n", "\n"] {
            let mut host = MockHost::default();
            let err = resolve_with(Backend::DeepSeek, false, true, answer, &mut host).unwrap_err();
            assert!(matches!(
                err,
                BackendBootstrapError::BackendInstallDeclined { .. }
            ));
            assert!(err.to_string().contains("clud dsh-update"));
            assert!(host.installer_runs.is_empty());
        }
    }

    #[test]
    fn deepseek_missing_noninteractively_names_the_managed_install() {
        let mut host = MockHost::default();
        let err = resolve_with(Backend::DeepSeek, false, false, "y\n", &mut host).unwrap_err();
        assert!(err.to_string().contains("clud dsh-update"));
        assert!(host.installer_runs.is_empty());
    }

    #[test]
    fn deepseek_dry_run_neither_prompts_nor_installs() {
        let mut host = MockHost::default();
        let (path, _) = resolve_with(Backend::DeepSeek, true, true, "y\n", &mut host).unwrap();
        assert_eq!(path, "dsh");
        assert!(host.installer_runs.is_empty());
    }

    #[test]
    fn deepseek_in_the_managed_prefix_only_is_used_without_a_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("dsh");
        std::fs::write(&managed, b"managed").unwrap();
        let mut host = MockHost {
            native_path: Some(managed.clone()),
            ..MockHost::default()
        };
        let (path, transcript) =
            resolve_with(Backend::DeepSeek, false, true, "", &mut host).unwrap();
        assert_eq!(PathBuf::from(path), managed);
        assert!(transcript.is_empty());
        assert!(host.installer_runs.is_empty());
    }

    #[test]
    fn deepseek_on_path_wins_over_the_managed_prefix() {
        let mut host = MockHost {
            find_results: VecDeque::from([Some(PathBuf::from("/usr/local/bin/dsh"))]),
            native_path: Some(PathBuf::from("/never/used")),
            ..MockHost::default()
        };
        let (path, _) = resolve_with(Backend::DeepSeek, false, true, "", &mut host).unwrap();
        assert_eq!(path, "/usr/local/bin/dsh");
    }

    #[test]
    fn interactive_decline_does_not_install() {
        for backend in [Backend::Claude, Backend::Codex] {
            let mut host = MockHost::default();
            let err = resolve_with(backend, false, true, "n\n", &mut host).unwrap_err();
            assert!(matches!(
                err,
                BackendBootstrapError::BackendInstallDeclined { .. }
            ));
            assert!(host.installer_runs.is_empty());
            assert!(host.verified.is_empty());
        }
    }

    #[test]
    fn interactive_accept_installs_verifies_and_returns_path_from_path() {
        for backend in [Backend::Claude, Backend::Codex] {
            let installed =
                PathBuf::from(format!("/home/me/.local/bin/{}", backend.executable_name()));
            let mut host = MockHost {
                find_results: VecDeque::from([None, Some(installed.clone())]),
                installer_result: Ok(()),
                verify_result: Ok(()),
                ..Default::default()
            };
            let (path, prompt) =
                resolve_with(backend, false, true, "yes\n", &mut host).expect("path");
            assert_eq!(path, installed.to_string_lossy());
            // #2002: one empty line separates diagnostics from the question.
            let expected_prompt = format!(
                "\n\n{}\n",
                BackendInstallSpec::for_backend(backend, InstallPlatform::Linux).prompt()
            );
            assert!(
                prompt.ends_with(&expected_prompt),
                "prompt must be the last thing written, got: {prompt:?}"
            );
            assert!(
                !prompt.contains("\n\n\n"),
                "prompt must have exactly one separating blank line, got: {prompt:?}"
            );
            assert!(
                prompt.contains("not found on PATH"),
                "diagnostic must precede the prompt, got: {prompt:?}"
            );
            assert_eq!(host.installer_runs, vec![backend]);
            assert_eq!(host.verified, vec![(backend, installed)]);
        }
    }

    #[test]
    fn selected_backend_isolation_installs_only_the_selected_backend() {
        let codex_path = PathBuf::from("/home/me/.local/bin/codex");
        let mut host = MockHost {
            find_results: VecDeque::from([None, Some(codex_path.clone())]),
            installer_result: Ok(()),
            verify_result: Ok(()),
            ..Default::default()
        };
        let (path, _) = resolve_with(Backend::Codex, false, true, "y\n", &mut host).expect("path");
        assert_eq!(path, codex_path.to_string_lossy());
        assert_eq!(host.find_calls, vec![Backend::Codex, Backend::Codex]);
        assert_eq!(host.installer_runs, vec![Backend::Codex]);
        assert_eq!(host.verified, vec![(Backend::Codex, codex_path)]);
    }

    #[test]
    fn installer_failure_reports_manual_command() {
        let mut host = MockHost {
            installer_result: Err("network down".to_string()),
            ..Default::default()
        };
        let err = resolve_with(Backend::Codex, false, true, "y\n", &mut host).unwrap_err();
        assert!(matches!(
            err,
            BackendBootstrapError::BackendInstallerFailed { .. }
        ));
        assert!(err.to_string().contains("network down"));
        assert!(err.to_string().contains("clud codex-update"));
    }

    #[test]
    fn verification_failure_rejects_install() {
        let installed = PathBuf::from("/home/me/.local/bin/claude");
        let mut host = MockHost {
            find_results: VecDeque::from([None, Some(installed.clone())]),
            installer_result: Ok(()),
            verify_result: Err("bad version".to_string()),
            ..Default::default()
        };
        let err = resolve_with(Backend::Claude, false, true, "y\n", &mut host).unwrap_err();
        assert!(matches!(
            err,
            BackendBootstrapError::BackendVerificationFailed { .. }
        ));
        assert!(err.to_string().contains("bad version"));
        assert_eq!(host.verified, vec![(Backend::Claude, installed)]);
    }

    #[test]
    fn path_fallback_uses_documented_native_install_location() {
        for backend in [Backend::Claude, Backend::Codex] {
            let temp = tempfile::tempdir().expect("tempdir");
            let native = temp.path().join(if crate::platform::host::WINDOWS {
                format!("{}.exe", backend.executable_name())
            } else {
                backend.executable_name().to_string()
            });
            // The binary does NOT exist yet — the installer materializes it.
            // (Pre-writing it here would now short-circuit at the pre-prompt
            // gate and never exercise the post-install fallback.)
            let mut host = MockHost {
                find_results: VecDeque::from([None, None]),
                installer_result: Ok(()),
                native_path: Some(native.clone()),
                installer_creates: Some(native.clone()),
                verify_result: Ok(()),
                ..Default::default()
            };
            let (path, _) = resolve_with(backend, false, true, "Y\n", &mut host).expect("path");
            assert_eq!(path, native.to_string_lossy());
            assert_eq!(host.verified, vec![(backend, native)]);
            assert_eq!(host.installer_runs, vec![backend]);
        }
    }

    /// Issue #934: the binary is already at the documented native install
    /// location but missing from `PATH` (stale shell env, an updater
    /// mid-swap). clud must use it as-is — no prompt, no reinstall.
    #[test]
    fn existing_native_install_is_used_when_path_lookup_misses() {
        for backend in [Backend::Claude, Backend::Codex] {
            let temp = tempfile::tempdir().expect("tempdir");
            let native = temp.path().join(if crate::platform::host::WINDOWS {
                format!("{}.exe", backend.executable_name())
            } else {
                backend.executable_name().to_string()
            });
            std::fs::write(&native, b"mock").expect("write native path");
            let mut host = MockHost {
                find_results: VecDeque::from([None]),
                native_path: Some(native.clone()),
                ..Default::default()
            };

            // Empty stdin: if this prompted, `read_yes` would see EOF and the
            // call would fail as declined rather than resolving.
            let (path, err) = resolve_with(backend, false, true, "", &mut host).expect("path");

            assert_eq!(path, native.to_string_lossy());
            assert!(
                host.installer_runs.is_empty(),
                "must not reinstall a backend that is already on disk"
            );
            assert!(
                !err.contains("is not installed"),
                "must not claim the backend is missing, got: {err:?}"
            );
        }
    }

    /// The diagnostic names the fallback location it checked, so a false
    /// "not installed" is debuggable from the transcript alone.
    #[test]
    fn missing_backend_prompt_reports_the_locations_checked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let absent = temp.path().join("claude-not-here");
        let mut host = MockHost {
            find_results: VecDeque::from([None]),
            native_path: Some(absent.clone()),
            ..Default::default()
        };

        let mut input = io::Cursor::new(b"n\n".to_vec());
        let mut buf = Vec::<u8>::new();
        let _ = resolve_backend_path(
            Backend::Claude,
            false,
            true,
            &mut input,
            &mut buf,
            &mut host,
        );

        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("not found on PATH"), "got: {text:?}");
        assert!(
            text.contains(&absent.display().to_string()),
            "diagnostic must name the fallback path checked, got: {text:?}"
        );
    }
}

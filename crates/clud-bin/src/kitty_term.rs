//! Explicit Windows launch through the packaged native WezTerm GUI.

use crate::args::Args;
use crate::subprocess::ManagedSubprocess;
use std::path::{Path, PathBuf};

const ENV_KITTY_TERM: &str = "CLUD_KITTY_TERM";
const ENV_BINARY_OVERRIDE: &str = "CLUD_KITTY_TERM_BINARY";
const ENV_CONFIG_OVERRIDE: &str = "CLUD_KITTY_TERM_CONFIG";

/// Handle only an explicit Kitty terminal request before ordinary startup.
pub fn handle(args: &Args) -> Option<i32> {
    clear_shell_wrapper_env();
    if args.shell.is_some() {
        return handle_shell(args);
    }
    if !args.kitty_term {
        return None;
    }
    if args.command.is_some() {
        eprintln!("[clud] --kitty-term only applies to a backend launch, not a clud subcommand");
        return Some(2);
    }
    if std::env::var_os(ENV_KITTY_TERM).is_some()
        || std::env::var_os(crate::webterm::ENV_WEBTERM).is_some()
    {
        eprintln!("[clud] --kitty-term cannot launch inside an existing desktop terminal");
        return Some(2);
    }
    if let Some(error) = platform_error(crate::platform::host::WINDOWS, std::env::consts::ARCH) {
        eprintln!("[clud] {error}");
        return Some(1);
    }
    Some(match launch(args) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("[clud] Kitty terminal unavailable: {error}");
            1
        }
    })
}

/// Experimental `--shell bash`. Windows runs the launch under Git Bash in the
/// bundled WezTerm fork; other hosts already have a native shell and PTY.
fn handle_shell(args: &Args) -> Option<i32> {
    if args.kitty_term || args.web_term {
        eprintln!("[clud] --shell cannot be combined with --kitty-term or --web-term");
        return Some(2);
    }
    if !crate::platform::host::WINDOWS {
        eprintln!("[clud] --shell bash only changes Windows launches; continuing normally");
        return None;
    }
    if !matches!(args.command, None | Some(crate::args::Command::Loop { .. })) {
        eprintln!("[clud] --shell only applies to a backend launch or `clud loop`");
        return Some(2);
    }
    if std::env::var_os(ENV_KITTY_TERM).is_some()
        || std::env::var_os(crate::webterm::ENV_WEBTERM).is_some()
    {
        eprintln!("[clud] --shell cannot launch inside an existing desktop terminal");
        return Some(2);
    }
    if let Some(error) = platform_error(true, std::env::consts::ARCH) {
        eprintln!("[clud] {}", error.replace("--kitty-term", "--shell bash"));
        return Some(1);
    }
    Some(match launch_shell(args) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("[clud] --shell bash unavailable: {error}");
            1
        }
    })
}

fn launch_shell(args: &Args) -> Result<i32, String> {
    let executable = std::env::current_exe().map_err(|error| format!("locating clud: {error}"))?;
    let cwd =
        std::env::current_dir().map_err(|error| format!("reading current directory: {error}"))?;
    let (binary, config) = companion_paths(&executable)?;
    let home = crate::home::user_home().ok_or("cannot resolve the home directory")?;
    let bash = crate::shell::git_bash_resolver::resolve_or_fetch_git_bash(&home)
        .map_err(|error| format!("resolving Git Bash: {error}"))?;
    let argv = shell_argv(args, &binary, &config, &bash, &executable, &cwd);
    let process = ManagedSubprocess::start_inheriting_env(argv, None, false, None)?;
    process.wait(None)
}

/// MSYS rewrites arguments that look like POSIX paths when it starts a native
/// program, which would corrupt a prompt such as `/fix this`.
const SHELL_EXEC: &str =
    "CLUD_SHELL_WRAPPED=1 MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1 exec \"$@\"";
const SHELL_WRAPPER_ENV: [&str; 3] = [
    "CLUD_SHELL_WRAPPED",
    "MSYS2_ARG_CONV_EXCL",
    "MSYS_NO_PATHCONV",
];

/// The wrapper's settings protect only its own exec of clud. Left set, they
/// would turn path conversion off for the agent and all its tool calls.
fn clear_shell_wrapper_env() {
    if std::env::var_os(SHELL_WRAPPER_ENV[0]).is_some() {
        for name in SHELL_WRAPPER_ENV {
            // SAFETY: called from startup dispatch, before clud starts threads
            // that read the environment, as `startup.rs` does.
            unsafe { std::env::remove_var(name) };
        }
    }
}

fn shell_argv(
    args: &Args,
    binary: &Path,
    config: &Path,
    bash: &Path,
    executable: &Path,
    cwd: &Path,
) -> Vec<String> {
    let mut argv = vec![
        binary.to_string_lossy().into_owned(),
        "--config-file".into(),
        config.to_string_lossy().into_owned(),
        "start".into(),
        "--no-auto-connect".into(),
        "--wait-exit".into(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--".into(),
        bash.to_string_lossy().into_owned(),
        "--noprofile".into(),
        "--norc".into(),
        "-c".into(),
        SHELL_EXEC.into(),
        "clud".into(),
        executable.to_string_lossy().into_owned(),
    ];
    let mut items = args.raw_argv.iter().skip(1);
    while let Some(item) = items.next() {
        if item == "--" {
            argv.push(item.clone());
            argv.extend(items.by_ref().cloned());
        } else if item == "--shell" {
            items.next();
        } else if !item.starts_with("--shell=") {
            argv.push(item.clone());
        }
    }
    argv
}

fn platform_error(is_windows: bool, architecture: &str) -> Option<&'static str> {
    if !is_windows {
        return Some("--kitty-term is currently supported on Windows only");
    }
    if architecture == "aarch64" {
        return Some(
            "--kitty-term does not support Windows ARM64; no ARM64 WezTerm GUI bundle is packaged",
        );
    }
    if architecture != "x86_64" {
        return Some("--kitty-term currently requires x86_64 Windows");
    }
    None
}

fn launch(args: &Args) -> Result<i32, String> {
    let executable = std::env::current_exe().map_err(|error| format!("locating clud: {error}"))?;
    let cwd =
        std::env::current_dir().map_err(|error| format!("reading current directory: {error}"))?;
    let (binary, config) = companion_paths(&executable)?;
    let argv = launch_argv(args, &binary, &config, &executable, &cwd);
    let process = ManagedSubprocess::start_inheriting_env(argv, None, false, None)?;
    process.wait(None)
}

fn companion_paths(executable: &Path) -> Result<(PathBuf, PathBuf), String> {
    let (default_binary, default_config) = default_paths(executable)?;
    let binary = std::env::var_os(ENV_BINARY_OVERRIDE)
        .map(PathBuf::from)
        .unwrap_or(default_binary);
    let config = std::env::var_os(ENV_CONFIG_OVERRIDE)
        .map(PathBuf::from)
        .unwrap_or(default_config);
    for (name, path) in [("WezTerm GUI", &binary), ("Kitty terminal config", &config)] {
        if !path.is_file() {
            return Err(format!(
                "{name} is missing at {}; install the Windows Kitty terminal bundle",
                path.display()
            ));
        }
    }
    Ok((binary, config))
}

fn default_paths(executable: &Path) -> Result<(PathBuf, PathBuf), String> {
    let directory = executable
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            format!(
                "clud executable has no parent directory: {}",
                executable.display()
            )
        })?
        .join("clud-kittyterm");
    Ok((
        directory.join("wezterm-gui.exe"),
        directory.join("clud-kittyterm.lua"),
    ))
}

fn launch_argv(
    args: &Args,
    binary: &Path,
    config: &Path,
    executable: &Path,
    cwd: &Path,
) -> Vec<String> {
    let mut argv = vec![
        binary.to_string_lossy().into_owned(),
        "--config-file".into(),
        config.to_string_lossy().into_owned(),
        "start".into(),
        "--no-auto-connect".into(),
        "--wait-exit".into(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--".into(),
        executable.to_string_lossy().into_owned(),
    ];
    let mut backend_arguments = false;
    for item in args.raw_argv.iter().skip(1) {
        if item == "--" {
            backend_arguments = true;
        }
        if !backend_arguments && item == "--kitty-term" {
            continue;
        }
        argv.push(item.clone());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(raw: &[&str]) -> Args {
        Args::parse_from_raw(raw.iter().map(|item| item.to_string()).collect())
    }

    #[test]
    fn forwards_original_argv_with_native_wezterm_launch_flags() {
        let args = parse(&[
            "clud",
            "--kitty-term",
            "--codex",
            "-p",
            "hello",
            "--",
            "--kitty-term",
        ]);
        let argv = launch_argv(
            &args,
            Path::new("C:/bin/clud-kittyterm/wezterm-gui.exe"),
            Path::new("C:/bin/clud-kittyterm/clud-kittyterm.lua"),
            Path::new("C:/bin/clud.exe"),
            Path::new("C:/work"),
        );
        assert_eq!(
            argv,
            [
                "C:/bin/clud-kittyterm/wezterm-gui.exe",
                "--config-file",
                "C:/bin/clud-kittyterm/clud-kittyterm.lua",
                "start",
                "--no-auto-connect",
                "--wait-exit",
                "--cwd",
                "C:/work",
                "--",
                "C:/bin/clud.exe",
                "--codex",
                "-p",
                "hello",
                "--",
                "--kitty-term",
            ]
        );
    }

    #[test]
    fn shell_flag_precedes_loop_and_is_stripped_from_the_forward() {
        for raw in [
            &[
                "clud", "--shell", "bash", "loop", "/fix it", "--", "--shell", "x",
            ][..],
            &[
                "clud",
                "--shell=bash",
                "loop",
                "/fix it",
                "--",
                "--shell",
                "x",
            ],
        ] {
            let args = parse(raw);
            assert_eq!(
                args.shell,
                Some(crate::args::ExperimentalShell::Bash),
                "{raw:?}"
            );
            assert!(
                matches!(args.command, Some(crate::args::Command::Loop { .. })),
                "{raw:?}"
            );
            let argv = shell_argv(
                &args,
                Path::new("W.exe"),
                Path::new("c.lua"),
                Path::new("bash.exe"),
                Path::new("clud.exe"),
                Path::new("C:/work"),
            );
            let forwarded = &argv[argv.iter().position(|a| a == "clud.exe").unwrap() + 1..];
            assert_eq!(forwarded, ["loop", "/fix it", "--", "--shell", "x"]);
            assert_eq!(argv[9], "bash.exe");
        }
    }

    #[test]
    fn companion_defaults_to_wheel_data_directory() {
        let (binary, config) = default_paths(Path::new("C:/venv/Scripts/clud.exe")).unwrap();
        assert_eq!(binary, Path::new("C:/venv/clud-kittyterm/wezterm-gui.exe"));
        assert_eq!(
            config,
            Path::new("C:/venv/clud-kittyterm/clud-kittyterm.lua")
        );
    }

    #[test]
    fn rejects_explicit_subcommand_before_launcher_resolution() {
        let args = parse(&["clud", "--kitty-term", "auth", "status"]);
        assert_eq!(handle(&args), Some(2));
    }

    #[test]
    fn windows_arm64_reports_unsupported_architecture_before_bundle_lookup() {
        assert_eq!(
            platform_error(true, "aarch64"),
            Some("--kitty-term does not support Windows ARM64; no ARM64 WezTerm GUI bundle is packaged")
        );
        assert_eq!(platform_error(true, "x86_64"), None);
        assert_eq!(
            platform_error(false, "x86_64"),
            Some("--kitty-term is currently supported on Windows only")
        );
    }
}

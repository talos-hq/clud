//! `clud optimize`: persistent, fast-machine setup recommendations.

use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::args::{Args, OptimizeTarget};
use crate::clud_settings::{self, RustOptimizeSettings};
use crate::loop_spec;
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};

/// Resolved write scope for `clud optimize rust`. Decoupled from the raw
/// `--global` / `--repo` booleans so the interactive prompt can produce the
/// same shape as the flag-driven path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteScope {
    Repo,
    Global,
}

const REPO_DIRECTIVE_DIR: &str = ".clud";
const REPO_DIRECTIVE_FILE: &str = "settings.json";

pub fn run(
    args: &Args,
    target: OptimizeTarget,
    global: bool,
    repo: bool,
    install_soldr: bool,
    use_soldr_shims: bool,
    soldr_version: &str,
) -> i32 {
    match target {
        OptimizeTarget::Rust => run_rust(
            args,
            global,
            repo,
            install_soldr,
            use_soldr_shims,
            soldr_version,
        ),
    }
}

/// soldr cannot be pinned: a requested version is ignored with a yellow
/// warning, nothing is persisted, and the install follows latest.
fn ignore_soldr_pin(requested: &str) -> &'static str {
    if let Some(pin) = crate::soldr_activate::pinned_version(Some(requested)) {
        eprintln!("{}", crate::soldr_activate::pin_ignored_warning(pin));
    }
    "latest"
}

fn run_rust(
    args: &Args,
    global: bool,
    repo: bool,
    install_soldr: bool,
    use_soldr_shims: bool,
    soldr_version: &str,
) -> i32 {
    let soldr_version = ignore_soldr_pin(soldr_version);
    let settings = RustOptimizeSettings {
        use_soldr_shims,
        install_soldr,
        soldr_version: soldr_version.to_string(),
    };

    // Resolve scope. Explicit flags win. Otherwise: if we have a TTY and
    // aren't in --dry-run, prompt — keeps automated callers stable (CI,
    // tests, scripted pipelines) while giving interactive users a choice.
    let scope = if global {
        WriteScope::Global
    } else if repo {
        WriteScope::Repo
    } else if !args.dry_run && io::stdin().is_terminal() && io::stderr().is_terminal() {
        match prompt_scope_from_stdin() {
            Ok(scope) => scope,
            Err(error) => {
                eprintln!("[clud] error: scope prompt failed: {error}");
                return 1;
            }
        }
    } else {
        // Preserves the pre-prompt default for non-TTY / dry-run callers:
        // bare `clud optimize rust` writes the repo directive.
        WriteScope::Repo
    };

    if args.dry_run {
        println!("[clud] dry-run: optimize rust");
        match scope {
            WriteScope::Global => {
                println!(
                    "[clud] dry-run: would write ~/.clud/settings.json optimize.rust use_soldr_shims={} install_soldr={} soldr_version=\"{}\"",
                    settings.use_soldr_shims, settings.install_soldr, settings.soldr_version
                );
            }
            WriteScope::Repo => match repo_directive_path() {
                Ok(path) => println!(
                    "[clud] dry-run: would write repo directive {}",
                    path.display()
                ),
                Err(error) => {
                    eprintln!("[clud] error: {error}");
                    return 1;
                }
            },
        }
        if install_soldr {
            // Surface the dry-run install destination so the user can audit
            // it the same way the live path now does.
            match planned_soldr_install_target() {
                Ok(target) => println!(
                    "[clud] dry-run: would install soldr {} to {} (only if missing from PATH)",
                    soldr_version,
                    target.display()
                ),
                Err(error) => {
                    eprintln!("[clud] error: {error}");
                    return 1;
                }
            }
        }
        return 0;
    }

    match scope {
        WriteScope::Global => {
            if let Err(error) = clud_settings::save_rust_optimize_settings(&settings) {
                eprintln!("[clud] error: failed to save optimize settings: {error}");
                return 1;
            }
            println!("[clud] wrote global Rust optimizer defaults to ~/.clud/settings.json");
        }
        WriteScope::Repo => match write_repo_directive(&settings) {
            Ok(path) => {
                println!(
                    "[clud] wrote repo Rust optimizer directive to {}",
                    path.display()
                );
                if let Err(error) = stage_repo_directive(&path) {
                    eprintln!("[clud] error: failed to stage repo directive: {error}");
                    return 1;
                }
                println!("[clud] staged {}", display_repo_path(&path));
            }
            Err(error) => {
                eprintln!("[clud] error: failed to write repo directive: {error}");
                return 1;
            }
        },
    }

    if install_soldr {
        match ensure_soldr_installed(soldr_version) {
            Ok(outcome) => println!("[clud] {outcome}"),
            Err(error) => {
                eprintln!("[clud] error: failed to install soldr: {error}");
                return 1;
            }
        }
    } else {
        println!("[clud] skipped soldr install");
    }

    if use_soldr_shims {
        println!("[clud] enabled soldr shim preference for future clud-managed Rust setup");
    } else {
        println!("[clud] disabled soldr shim preference");
    }
    0
}

/// Interactive scope prompt for `clud optimize rust`. Writes to stderr so
/// stdout stays clean for piped consumers. Enter (empty line) accepts the
/// `[L]ocal` default to preserve the pre-prompt behavior. EOF (closed
/// stdin) likewise falls back to Local.
pub(crate) fn prompt_scope_from_stdin() -> io::Result<WriteScope> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    prompt_scope(&mut reader, &mut io::stderr())
}

pub(crate) fn prompt_scope<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> io::Result<WriteScope> {
    write!(
        writer,
        "[clud] install scope — [L]ocal repo (.clud/settings.json) or [G]lobal (~/.clud/settings.json)? [L]: "
    )?;
    writer.flush()?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        // EOF: treat as default selection rather than erroring.
        return Ok(WriteScope::Repo);
    }
    Ok(parse_scope_answer(&line))
}

/// Pure mapping from a typed answer to a [`WriteScope`]. Unknown input
/// falls back to the default (Local/Repo) rather than reprompting — the
/// prompt is one-shot so automation that pipes "yes\n" still gets a sane
/// outcome.
pub(crate) fn parse_scope_answer(answer: &str) -> WriteScope {
    let trimmed = answer.trim();
    if trimmed.eq_ignore_ascii_case("g") || trimmed.eq_ignore_ascii_case("global") {
        WriteScope::Global
    } else {
        WriteScope::Repo
    }
}

/// Compute where `ensure_soldr_installed` would put the binary if it had
/// to install today. Used by the pre-install announcement and the dry-run
/// preview so the user knows the destination before any network I/O.
fn planned_soldr_install_target() -> Result<PathBuf, String> {
    let home = crate::home::user_home()
        .ok_or_else(|| "could not resolve user home directory".to_string())?;
    let target_dir = global_bin_dir(&home);
    let asset = soldr_asset_for_current_platform("0.0.0")?; // version unused for binary_name
    Ok(target_dir.join(asset.binary_name))
}

fn write_repo_directive(settings: &RustOptimizeSettings) -> io::Result<PathBuf> {
    let path = repo_directive_path()?;
    let repo_root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("could not resolve repo root"))?;
    ensure_rust_project(repo_root)?;
    write_repo_directive_at(&path, settings)?;
    let gitignore_updated = ensure_gitignore_tracks_repo_settings(repo_root)?;
    ensure_not_ignored(repo_root, &path)?;
    if gitignore_updated {
        stage_repo_path(repo_root, Path::new(".gitignore"))?;
    }
    Ok(path)
}

fn repo_directive_path() -> io::Result<PathBuf> {
    let cwd = env::current_dir()?;
    let root = loop_spec::git_root_from(&cwd);
    Ok(root.join(REPO_DIRECTIVE_DIR).join(REPO_DIRECTIVE_FILE))
}

fn write_repo_directive_at(path: &Path, settings: &RustOptimizeSettings) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut root = match fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        Ok(_) => serde_json::json!({}),
        Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error),
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    // No `soldr_version`: soldr cannot be pinned (it always follows latest).
    root["optimize"]["rust"] = serde_json::json!({
        "use_soldr_shims": settings.use_soldr_shims,
        "install_soldr": settings.install_soldr,
    });
    let mut body = serde_json::to_string_pretty(&root).map_err(io::Error::other)?;
    body.push('\n');
    fs::write(path, body)
}

fn ensure_rust_project(repo_root: &Path) -> io::Result<()> {
    if repo_root.join("Cargo.toml").is_file() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{} does not look like a Rust project: Cargo.toml was not found",
        repo_root.display()
    )))
}

fn ensure_gitignore_tracks_repo_settings(repo_root: &Path) -> io::Result<bool> {
    let gitignore = repo_root.join(".gitignore");
    let original = match fs::read_to_string(&gitignore) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let updated = rewrite_gitignore_for_repo_settings(&original);
    if updated != original {
        fs::write(gitignore, updated)?;
        return Ok(true);
    }
    Ok(false)
}

fn rewrite_gitignore_for_repo_settings(original: &str) -> String {
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let had_trailing_newline = original.ends_with('\n');
    let has_unignore_dir = lines
        .iter()
        .any(|line| matches!(line.trim(), "!.clud/" | "!/.clud/"));
    let has_unignore_settings = lines.iter().any(|line| {
        matches!(
            line.trim(),
            "!.clud/settings.json" | "!/.clud/settings.json"
        )
    });

    for line in &mut lines {
        if matches!(line.trim(), ".clud/" | "/.clud/" | ".clud" | "/.clud") {
            *line = ".clud/*".to_string();
        }
    }

    if !has_unignore_dir || !has_unignore_settings {
        if !lines.is_empty() && lines.last().is_some_and(|line| !line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("# clud project settings".to_string());
        if !has_unignore_dir {
            lines.push("!.clud/".to_string());
        }
        if !has_unignore_settings {
            lines.push("!.clud/settings.json".to_string());
        }
    }

    let mut out = lines.join("\n");
    if had_trailing_newline || !out.is_empty() {
        out.push('\n');
    }
    out
}

fn ensure_not_ignored(repo_root: &Path, path: &Path) -> io::Result<()> {
    let relative = path.strip_prefix(repo_root).unwrap_or(path);
    match run_status(
        vec![
            "git".to_string(),
            "check-ignore".to_string(),
            "--quiet".to_string(),
            relative.to_string_lossy().to_string(),
        ],
        Some(repo_root),
    )? {
        0 => Err(io::Error::other(format!(
            "{} is still ignored by git",
            relative.display()
        ))),
        1 => Ok(()),
        _ => Err(io::Error::other(format!(
            "git check-ignore failed for {}",
            relative.display()
        ))),
    }
}

fn stage_repo_directive(path: &Path) -> io::Result<()> {
    let repo_root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("could not resolve repo root"))?;
    let relative = path.strip_prefix(repo_root).unwrap_or(path);
    stage_repo_path(repo_root, relative)
}

fn stage_repo_path(repo_root: &Path, relative: &Path) -> io::Result<()> {
    let code = run_status(
        vec![
            "git".to_string(),
            "add".to_string(),
            relative.to_string_lossy().to_string(),
        ],
        Some(repo_root),
    )?;
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "git add failed with exit code {code}"
        )))
    }
}

fn display_repo_path(path: &Path) -> String {
    path.parent()
        .and_then(|parent| parent.file_name())
        .zip(path.file_name())
        .map(|(dir, file)| format!("{}/{}", dir.to_string_lossy(), file.to_string_lossy()))
        .unwrap_or_else(|| path.display().to_string())
}

fn ensure_soldr_installed(version: &str) -> Result<String, String> {
    if version.eq_ignore_ascii_case("latest") {
        let code = run_status(
            vec![
                "uv".to_string(),
                "tool".to_string(),
                "install".to_string(),
                "--force".to_string(),
                "soldr".to_string(),
            ],
            None,
        )
        .map_err(|error| format!("run rolling-latest soldr install: {error}"))?;
        return (code == 0)
            .then(|| "installed rolling-latest soldr via uv".to_string())
            .ok_or_else(|| format!("rolling-latest soldr install exited with code {code}"));
    }
    if let Ok(path) = which::which("soldr") {
        return Ok(format!("soldr already installed at {}", path.display()));
    }
    let home = crate::home::user_home()
        .ok_or_else(|| "could not resolve user home directory".to_string())?;
    let target_dir = global_bin_dir(&home);
    fs::create_dir_all(&target_dir)
        .map_err(|error| format!("create {}: {error}", target_dir.display()))?;

    let asset = soldr_asset_for_current_platform(version)?;
    // Pre-announce *before* network I/O. Users running into a slow GitHub
    // CDN otherwise stare at a silent terminal for tens of seconds; the
    // single line up front makes both the action and the destination
    // obvious before the spinner.
    eprintln!(
        "[clud] installing soldr {} to {}",
        version,
        target_dir.join(asset.binary_name).display()
    );
    let url = format!(
        "https://github.com/zackees/soldr/releases/download/v{version}/{}",
        asset.file_name
    );
    let temp_dir = env::temp_dir().join(format!("clud-soldr-{}", std::process::id()));
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)
            .map_err(|error| format!("clear {}: {error}", temp_dir.display()))?;
    }
    fs::create_dir_all(&temp_dir).map_err(|error| format!("create temp dir: {error}"))?;
    let cleanup_dir = temp_dir.clone();
    let result = install_soldr_from_url(&url, &asset, &temp_dir, &target_dir);
    let _ = fs::remove_dir_all(cleanup_dir);
    result
}

fn install_soldr_from_url(
    url: &str,
    asset: &SoldrAsset,
    temp_dir: &Path,
    target_dir: &Path,
) -> Result<String, String> {
    let archive = temp_dir.join(&asset.file_name);
    eprintln!("[clud] downloading {url}");
    let response = ureq::get(url)
        .call()
        .map_err(|error| format!("download {url}: {error}"))?;
    let mut reader = response.into_reader();
    let mut file = fs::File::create(&archive)
        .map_err(|error| format!("create {}: {error}", archive.display()))?;
    io::copy(&mut reader, &mut file).map_err(|error| format!("write download: {error}"))?;

    extract_archive(&archive, temp_dir, asset)?;
    let src = find_file_named(temp_dir, asset.binary_name)
        .ok_or_else(|| format!("{} not found in {}", asset.binary_name, asset.file_name))?;
    let target = target_dir.join(asset.binary_name);
    fs::copy(&src, &target)
        .map_err(|error| format!("copy {} to {}: {error}", src.display(), target.display()))?;
    make_executable(&target)?;
    Ok(format!(
        "installed soldr {version} to {path}",
        version = asset.version,
        path = target.display()
    ))
}

fn extract_archive(archive: &Path, temp_dir: &Path, asset: &SoldrAsset) -> Result<(), String> {
    let status = if asset.extension == "zip" {
        expand_zip(archive, temp_dir)?
    } else {
        run_status_string(
            vec![
                "tar".to_string(),
                "-xzf".to_string(),
                archive.to_string_lossy().to_string(),
                "-C".to_string(),
                temp_dir.to_string_lossy().to_string(),
            ],
            None,
        )?
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "extract {} failed with exit code {status}",
            archive.display()
        ))
    }
}

/// PEP 723 helper that extracts a `.zip` via the stdlib `zipfile` module.
/// Embedded at compile time so the binary is self-contained; written to a
/// temp path on demand and invoked via `uv run --script`.
const EXTRACT_ZIP_PY: &str = include_str!("../assets/scripts/extract_zip.py");

fn expand_zip(archive: &Path, temp_dir: &Path) -> Result<i32, String> {
    // Primary path: invoke the bundled Python extractor via `uv run --script`.
    // `--no-project` keeps the resolver from walking ancestors of CWD and
    // accidentally syncing whatever pyproject.toml happens to be there.
    let script_path = temp_dir.join("__clud_extract_zip.py");
    std::fs::write(&script_path, EXTRACT_ZIP_PY)
        .map_err(|error| format!("write extract_zip helper to temp: {error}"))?;
    let argv = vec![
        "uv".to_string(),
        "run".to_string(),
        "--no-project".to_string(),
        "--script".to_string(),
        script_path.to_string_lossy().to_string(),
        archive.to_string_lossy().to_string(),
        temp_dir.to_string_lossy().to_string(),
    ];
    match run_status_string(argv, None) {
        Ok(status) => {
            // Best-effort cleanup; if the rm fails the temp dir gets wiped
            // by the outer caller anyway.
            let _ = std::fs::remove_file(&script_path);
            Ok(status)
        }
        Err(uv_error) => {
            let _ = std::fs::remove_file(&script_path);
            expand_zip_via_powershell(archive, temp_dir, &uv_error)
        }
    }
}

/// PowerShell fallback used only when `uv run --script` could not be
/// spawned (uv missing from PATH). Honors the original two-step probe
/// of `powershell.exe` then `pwsh.exe`. Will be skipped entirely when
/// the upcoming `shell.disable_powershell` setting is true (forthcoming).
fn expand_zip_via_powershell(
    archive: &Path,
    temp_dir: &Path,
    uv_error: &str,
) -> Result<i32, String> {
    let args = vec![
        "-NoProfile".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-Command".to_string(),
        "Expand-Archive -LiteralPath $args[0] -DestinationPath $args[1] -Force".to_string(),
        archive.to_string_lossy().to_string(),
        temp_dir.to_string_lossy().to_string(),
    ];
    run_status_string(
        std::iter::once("powershell".to_string())
            .chain(args.clone())
            .collect(),
        None,
    )
    .or_else(|powershell_error| {
        run_status_string(
            std::iter::once("pwsh".to_string()).chain(args).collect(),
            None,
        )
        .map_err(|pwsh_error| {
            format!(
                "uv run --script unavailable ({uv_error}); PowerShell fallback also failed \
                 (powershell: {powershell_error}; pwsh: {pwsh_error})"
            )
        })
    })
}

fn run_status(argv: Vec<String>, cwd: Option<&Path>) -> io::Result<i32> {
    run_status_string(argv, cwd).map_err(io::Error::other)
}

fn run_status_string(argv: Vec<String>, cwd: Option<&Path>) -> Result<i32, String> {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: cwd.map(Path::to_path_buf),
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    process
        .start()
        .map_err(|error| format!("failed to start command: {error}"))?;

    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(_) => {}
            ReadStatus::Timeout => {
                if process.returncode().is_some() {
                    break;
                }
            }
            ReadStatus::Eof => break,
        }
    }

    process
        .wait(Some(Duration::from_secs(60)))
        .map_err(|error| format!("failed to wait for command: {error}"))
}

fn find_file_named(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().and_then(|file| file.to_str()) == Some(name) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_file_named(&path, name) {
                return Some(found);
            }
        }
    }
    None
}

use crate::platform::optimize::make_executable;

/// Where clud installs soldr: `$CARGO_HOME/bin` when present, else `~/.local/bin`.
pub(crate) fn global_bin_dir(home: &Path) -> PathBuf {
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    let cargo_bin = cargo_home.join("bin");
    if cargo_bin.exists() {
        cargo_bin
    } else {
        home.join(".local").join("bin")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SoldrAsset {
    version: String,
    file_name: String,
    extension: &'static str,
    binary_name: &'static str,
}

fn soldr_asset_for_current_platform(version: &str) -> Result<SoldrAsset, String> {
    let arch = match env::consts::ARCH {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => return Err(format!("unsupported architecture: {other}")),
    };
    let (os, extension, binary_name) = match env::consts::OS {
        "linux" => ("unknown-linux-gnu", "tar.gz", "soldr"),
        "macos" => ("apple-darwin", "tar.gz", "soldr"),
        "windows" => ("pc-windows-msvc", "zip", "soldr.exe"),
        other => return Err(format!("unsupported OS: {other}")),
    };
    Ok(SoldrAsset {
        version: version.to_string(),
        file_name: format!("soldr-v{version}-{arch}-{os}.{extension}"),
        extension,
        binary_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    #[test]
    fn writes_repo_directive() {
        let dir = TemporaryDirectory::new().unwrap();
        let path = dir.path().join(".clud").join("settings.json");
        let settings = RustOptimizeSettings {
            use_soldr_shims: false,
            install_soldr: true,
            soldr_version: "9.9.9".to_string(),
        };

        write_repo_directive_at(&path, &settings).unwrap();

        let text = fs::read_to_string(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["optimize"]["rust"]["use_soldr_shims"], false);
        assert_eq!(parsed["optimize"]["rust"]["install_soldr"], true);
        assert!(
            parsed["optimize"]["rust"].get("soldr_version").is_none(),
            "a soldr version is never written: soldr cannot be pinned"
        );
    }

    #[test]
    fn repo_directive_preserves_existing_json() {
        let dir = TemporaryDirectory::new().unwrap();
        let path = dir.path().join(".clud").join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\n  \"other\": true\n}\n").unwrap();

        write_repo_directive_at(&path, &RustOptimizeSettings::default()).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(parsed["other"], true);
        assert_eq!(parsed["optimize"]["rust"]["use_soldr_shims"], true);
        assert!(parsed["optimize"]["rust"].get("soldr_version").is_none());
    }

    #[test]
    fn gitignore_rewrite_unignores_settings_only() {
        let original = "target/\n.clud/\n";

        let updated = rewrite_gitignore_for_repo_settings(original);

        assert!(updated.contains(".clud/*"), "{updated}");
        assert!(updated.contains("!.clud/"), "{updated}");
        assert!(updated.contains("!.clud/settings.json"), "{updated}");
    }

    #[test]
    fn soldr_asset_matches_current_platform() {
        let asset = soldr_asset_for_current_platform("1.2.3").unwrap();
        assert!(asset.file_name.starts_with("soldr-v1.2.3-"), "{asset:?}");
        if crate::platform::host::WINDOWS {
            assert!(asset.file_name.ends_with(".zip"), "{asset:?}");
            assert_eq!(asset.binary_name, "soldr.exe");
        } else {
            assert!(asset.file_name.ends_with(".tar.gz"), "{asset:?}");
            assert_eq!(asset.binary_name, "soldr");
        }
    }

    #[test]
    fn finds_nested_binary() {
        let dir = TemporaryDirectory::new().unwrap();
        let nested = dir.path().join("pkg").join("bin");
        fs::create_dir_all(&nested).unwrap();
        let binary = nested.join("soldr");
        fs::write(&binary, "x").unwrap();

        assert_eq!(find_file_named(dir.path(), "soldr"), Some(binary));
    }

    #[test]
    fn parse_scope_answer_defaults_to_repo_on_empty_or_garbage() {
        assert_eq!(parse_scope_answer(""), WriteScope::Repo);
        assert_eq!(parse_scope_answer("\n"), WriteScope::Repo);
        assert_eq!(parse_scope_answer("   "), WriteScope::Repo);
        // Unrecognized input keeps the safe default rather than reprompting.
        assert_eq!(parse_scope_answer("yes"), WriteScope::Repo);
        assert_eq!(parse_scope_answer("local"), WriteScope::Repo);
        assert_eq!(parse_scope_answer("l"), WriteScope::Repo);
        assert_eq!(parse_scope_answer("L"), WriteScope::Repo);
    }

    #[test]
    fn parse_scope_answer_picks_global_on_g_variants() {
        assert_eq!(parse_scope_answer("g"), WriteScope::Global);
        assert_eq!(parse_scope_answer("G"), WriteScope::Global);
        assert_eq!(parse_scope_answer("global"), WriteScope::Global);
        assert_eq!(parse_scope_answer("Global"), WriteScope::Global);
        assert_eq!(parse_scope_answer("  global  "), WriteScope::Global);
        assert_eq!(parse_scope_answer("g\n"), WriteScope::Global);
    }

    #[test]
    fn prompt_scope_treats_eof_as_default_repo() {
        // Closed stdin (EOF on first read) must not error out — that would
        // make the bare `clud optimize rust` invocation flaky for users in
        // shells that lose their controlling TTY mid-command.
        let mut reader = io::Cursor::new(Vec::<u8>::new());
        let mut writer = Vec::<u8>::new();
        let scope = prompt_scope(&mut reader, &mut writer).unwrap();
        assert_eq!(scope, WriteScope::Repo);
        let rendered = String::from_utf8(writer).unwrap();
        assert!(
            rendered.contains("install scope"),
            "prompt was not rendered: {rendered:?}"
        );
    }

    #[test]
    fn prompt_scope_reads_user_response() {
        let mut reader = io::Cursor::new(b"g\n".to_vec());
        let mut writer = Vec::<u8>::new();
        assert_eq!(
            prompt_scope(&mut reader, &mut writer).unwrap(),
            WriteScope::Global
        );

        let mut reader = io::Cursor::new(b"\n".to_vec());
        let mut writer = Vec::<u8>::new();
        assert_eq!(
            prompt_scope(&mut reader, &mut writer).unwrap(),
            WriteScope::Repo
        );
    }

    #[test]
    fn planned_soldr_install_target_resolves() {
        // We only assert structural properties: a real home dir on the
        // test host exists, and the returned path ends in the platform
        // binary name. The exact directory varies (CARGO_HOME may or may
        // not be set), so we don't pin it.
        let target = planned_soldr_install_target().expect("home dir must resolve in test");
        let last = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if crate::platform::host::WINDOWS {
            assert_eq!(last, "soldr.exe");
        } else {
            assert_eq!(last, "soldr");
        }
    }
}

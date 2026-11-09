//! Unix half of `crate::self_install::activation`: owned Bash, zsh, or fish
//! startup-file edits and fresh login/interactive shell verification.

use std::ffi::{CStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use running_process::ReadStatus;

/// The Unix activation plan: the managed startup-file edits for the login
/// shell, or a manual instruction when the shell is not Bash, zsh, or fish.
#[derive(Debug, Clone)]
pub struct ActivationHost {
    shell: PathBuf,
    snippet: String,
    edits: Vec<ProfileEdit>,
    manual_instruction: Option<String>,
}

#[derive(Debug, Clone)]
struct ProfileEdit {
    path: PathBuf,
    prior: Option<Vec<u8>>,
    next: Vec<u8>,
    missing_parents: Vec<PathBuf>,
}

const START: &str = "# >>> clud installer PATH >>>";
const END: &str = "# <<< clud installer PATH <<<";
const MAX_PROFILE_BYTES: u64 = 1024 * 1024;

pub fn terminal_commands(destination: &Path) -> Result<Vec<(&'static str, String)>, String> {
    let bin = destination.parent().ok_or("missing install directory")?;
    let value = bin.to_str().ok_or("install directory is not UTF-8")?;
    if value.contains([':', '\0', '\n', '\r']) {
        return Err("install directory cannot be represented in a shell PATH".into());
    }
    let quoted = shell_words::quote(value);
    Ok(vec![
        (
            "Bash/zsh",
            format!("case \"$PATH\" in {quoted}:*) ;; *) export PATH={quoted}:\"$PATH\" ;; esac; hash -r"),
        ),
        (
            "fish",
            format!("fish_add_path --path --move -- {}", fish_quote(bin)?),
        ),
    ])
}

/// The environment `plan` reads, captured once so the startup-file choice is
/// a pure function of it (and unit-testable for any host layout).
#[derive(Debug, Clone)]
struct ShellEnv {
    home: PathBuf,
    shell: PathBuf,
    zdotdir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
}

impl ShellEnv {
    fn from_process() -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is unavailable")?;
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .or_else(account_shell)
            .ok_or("cannot determine the account login shell")?;
        Ok(Self {
            home,
            shell,
            zdotdir: std::env::var_os("ZDOTDIR").map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        })
    }
}

pub fn plan(destination: &Path) -> Result<ActivationHost, String> {
    plan_in(destination, &ShellEnv::from_process()?)
}

fn plan_in(destination: &Path, env: &ShellEnv) -> Result<ActivationHost, String> {
    let bin = destination.parent().ok_or("missing install directory")?;
    if bin
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .any(|byte| matches!(*byte, b':' | b'\n' | b'\r' | 0))
    {
        return Err("install directory contains a PATH separator or control character".into());
    }
    if !env.home.is_absolute() {
        return Err("HOME must be absolute".into());
    }
    if !env.shell.is_absolute() {
        return Err("login shell must be an absolute path".into());
    }
    let name = env
        .shell
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("login shell name is unavailable")?;
    let Some(paths) = startup_files(name, env)? else {
        let bin_text = bin.to_string_lossy();
        let quoted = shell_words::quote(&bin_text);
        return Ok(ActivationHost {
            shell: env.shell.clone(),
            snippet: String::new(),
            edits: Vec::new(),
            manual_instruction: Some(format!(
                "add `export PATH={quoted}:\"$PATH\"` to this shell's startup file, then run a new shell"
            )),
        });
    };
    let snippet = managed_snippet(name, bin)?;
    let edits = paths
        .into_iter()
        .map(|path| profile_edit(path, &snippet))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ActivationHost {
        shell: env.shell.clone(),
        snippet,
        edits,
        manual_instruction: None,
    })
}

/// The startup files a fresh terminal of this login shell reads, or `None`
/// when clud does not manage this shell. zsh gets `.zprofile` because macOS
/// Terminal opens login shells and `/etc/zprofile` (path_helper) rebuilds
/// PATH before it; bash reuses the login file that already exists so a new
/// `.bash_profile` never shadows the user's `.profile`.
fn startup_files(name: &str, env: &ShellEnv) -> Result<Option<Vec<PathBuf>>, String> {
    let home = &env.home;
    Ok(Some(match name {
        "bash" => {
            let login = [".bash_profile", ".bash_login", ".profile"]
                .iter()
                .map(|name| home.join(name))
                .find(|path| fs::symlink_metadata(path).is_ok())
                .unwrap_or_else(|| home.join(".bash_profile"));
            vec![login, home.join(".bashrc")]
        }
        "zsh" => {
            let zdotdir = env.zdotdir.clone().unwrap_or_else(|| home.clone());
            if !zdotdir.is_absolute() {
                return Err("ZDOTDIR must be absolute".into());
            }
            vec![zdotdir.join(".zprofile"), zdotdir.join(".zshrc")]
        }
        "fish" => {
            let config = env
                .xdg_config_home
                .clone()
                .unwrap_or_else(|| home.join(".config"));
            if !config.is_absolute() {
                return Err("XDG_CONFIG_HOME must be absolute".into());
            }
            vec![config.join("fish/conf.d/clud-path.fish")]
        }
        _ => return Ok(None),
    }))
}

fn managed_snippet(shell_name: &str, bin: &Path) -> Result<String, String> {
    if shell_name == "fish" {
        return Ok(format!(
            "{START}\nfish_add_path --path --move -- {}\n{END}\n",
            fish_quote(bin)?
        ));
    }
    let bin_text = bin.to_string_lossy();
    let quoted = shell_words::quote(&bin_text);
    Ok(format!(
        "{START}\ncase \"$PATH\" in\n  {quoted}:*) ;;\n  *) PATH={quoted}:\"$PATH\"; export PATH ;;\nesac\n{END}\n"
    ))
}

/// Replace the one managed block in `prior`, or append it. The user's own
/// text is never parsed or rewritten; only the marker-delimited block moves.
fn merge_managed_block(prior: &[u8], snippet: &str, path: &Path) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(prior)
        .map_err(|_| format!("profile is not UTF-8: {}", path.display()))?;
    if !text.contains(START) && !text.contains(END) {
        let mut result = prior.to_vec();
        if !result.is_empty() && !result.ends_with(b"\n") {
            result.push(b'\n');
        }
        result.extend_from_slice(snippet.as_bytes());
        return Ok(result);
    }
    if text.matches(START).count() != 1 || text.matches(END).count() != 1 {
        return Err(format!(
            "startup file has ambiguous installer markers: {}",
            path.display()
        ));
    }
    let start = text.find(START).ok_or("missing installer start marker")?;
    let end_start = text.find(END).ok_or("missing installer end marker")?;
    if end_start <= start {
        return Err(format!("invalid installer markers: {}", path.display()));
    }
    let end = end_start + END.len();
    let end = if prior.get(end) == Some(&b'\n') {
        end + 1
    } else {
        end
    };
    let mut result = prior[..start].to_vec();
    result.extend_from_slice(snippet.as_bytes());
    result.extend_from_slice(&prior[end..]);
    Ok(result)
}

fn profile_edit(path: PathBuf, snippet: &str) -> Result<ProfileEdit, String> {
    let prior = read_profile(&path)?;
    let next = match &prior {
        Some(bytes) => merge_managed_block(bytes, snippet, &path)?,
        None => snippet.as_bytes().to_vec(),
    };
    if next.len() as u64 > MAX_PROFILE_BYTES {
        return Err(format!(
            "startup file would be too large: {}",
            path.display()
        ));
    }
    let mut missing_parents = Vec::new();
    let mut ancestor = path.parent();
    while let Some(directory) = ancestor {
        if fs::symlink_metadata(directory).is_ok() {
            break;
        }
        missing_parents.push(directory.to_path_buf());
        ancestor = directory.parent();
    }
    Ok(ProfileEdit {
        path,
        prior,
        next,
        missing_parents,
    })
}

fn account_shell() -> Option<PathBuf> {
    let mut buffer = vec![0_u8; 4096];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: getpwuid_r receives a writable entry and buffer that live through this call.
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || found.is_null() {
            return None;
        }
        // SAFETY: found points to the entry written by successful getpwuid_r.
        let shell_ptr = unsafe { (*found).pw_shell };
        if shell_ptr.is_null() {
            return None;
        }
        // SAFETY: a successful getpwuid_r points pw_shell into the live buffer.
        let shell = unsafe { CStr::from_ptr(shell_ptr) };
        return Some(PathBuf::from(OsString::from_vec(shell.to_bytes().to_vec())));
    }
}

fn fish_quote(path: &Path) -> Result<String, String> {
    let value = path.to_str().ok_or("fish path is not UTF-8")?;
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        if matches!(ch, '\\' | '"' | '$') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    Ok(quoted)
}

fn read_profile(path: &Path) -> Result<Option<Vec<u8>>, String> {
    inspect_parent(path)?;
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.nlink() != 1
        || meta.mode() & 0o022 != 0
        || meta.len() > MAX_PROFILE_BYTES
    {
        return Err(format!("unsafe startup file: {}", path.display()));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    if opened.dev() != meta.dev() || opened.ino() != meta.ino() {
        return Err(format!(
            "startup file changed while opening: {}",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_PROFILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(format!("startup file is too large: {}", path.display()));
    }
    Ok(Some(bytes))
}

fn inspect_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("startup file has no parent")?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is unavailable")?;
    let mut current = PathBuf::new();
    for part in parent.components() {
        current.push(part);
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("inspect {}: {error}", current.display())),
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(format!("unsafe startup directory: {}", current.display()));
        }
        if (current.starts_with(&home) || current == parent)
            && meta.uid() != unsafe { libc::geteuid() }
        {
            return Err(format!(
                "startup directory is not user-owned: {}",
                current.display()
            ));
        }
        if meta.mode() & 0o002 != 0 && meta.mode() & 0o1000 == 0 {
            return Err(format!(
                "world-writable startup directory: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

pub fn description(_destination: &Path, plan: &ActivationHost) -> String {
    if let Some(instruction) = &plan.manual_instruction {
        return format!(
            "shell: {}; manual activation: {instruction}",
            plan.shell.display()
        );
    }
    let mut lines = vec![format!("shell: {}", plan.shell.display())];
    for edit in &plan.edits {
        lines.push(format!(
            "{} {}",
            if edit.prior.as_ref() == Some(&edit.next) {
                "keep"
            } else if edit.prior.is_some() {
                "update"
            } else {
                "create"
            },
            edit.path.display()
        ));
    }
    format!(
        "{}\nmanaged startup text:\n{}",
        lines.join("; "),
        plan.snippet
    )
}

/// What to tell the user once the startup files are written: the current
/// terminal still has the old PATH until it re-reads them.
pub fn reload_hint(plan: &ActivationHost) -> String {
    match plan.edits.first() {
        Some(edit) => format!(
            "Open a new terminal, or run `source {}`, to use clud in this one.",
            shell_words::quote(&edit.path.to_string_lossy())
        ),
        None => "Open a new terminal to use clud.".to_owned(),
    }
}

pub fn apply_and_verify(
    destination: &Path,
    plan: &ActivationHost,
    version: &str,
) -> Result<(), String> {
    if let Some(instruction) = &plan.manual_instruction {
        return Err(format!("manual activation required: {instruction}"));
    }
    let mut changed = Vec::new();
    for edit in &plan.edits {
        let observed = match read_profile(&edit.path) {
            Ok(observed) => observed,
            Err(error) => {
                rollback(&changed)?;
                cleanup_created_dirs(&plan.edits);
                return Err(error);
            }
        };
        if observed != edit.prior {
            rollback(&changed)?;
            cleanup_created_dirs(&plan.edits);
            return Err(format!(
                "startup file changed after consent: {}",
                edit.path.display()
            ));
        }
        if edit.prior.as_ref() == Some(&edit.next) {
            continue;
        }
        if let Err(error) = write_profile(&edit.path, &edit.next, edit.prior.as_deref()) {
            rollback(&changed)?;
            cleanup_created_dirs(&plan.edits);
            return Err(error);
        }
        changed.push(edit);
    }
    if let Err(error) = verify(destination, plan, version) {
        rollback(&changed)?;
        cleanup_created_dirs(&plan.edits);
        return Err(error);
    }
    Ok(())
}

fn write_profile(path: &Path, bytes: &[u8], expected: Option<&[u8]>) -> Result<(), String> {
    inspect_parent(path)?;
    let parent = path.parent().ok_or("startup file has no parent")?;
    if !parent.exists() {
        super::self_install::create_private_dirs(parent)?;
    }
    inspect_parent(path)?;
    let mode = if expected.is_some() {
        fs::symlink_metadata(path)
            .map_err(|error| error.to_string())?
            .permissions()
            .mode()
    } else {
        0o600
    };
    let mut stage = tempfile::Builder::new()
        .prefix(".clud-profile-")
        .tempfile_in(parent)
        .map_err(|error| error.to_string())?;
    stage.write_all(bytes).map_err(|error| error.to_string())?;
    stage
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|error| error.to_string())?;
    stage
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    if read_profile(path)?.as_deref() != expected {
        return Err(format!(
            "startup file changed during update: {}",
            path.display()
        ));
    }
    stage
        .persist(path)
        .map_err(|error| error.error.to_string())?;
    Ok(())
}

fn rollback(changed: &[&ProfileEdit]) -> Result<(), String> {
    for edit in changed.iter().rev() {
        if read_profile(&edit.path)?.as_deref() != Some(&edit.next) {
            return Err(format!(
                "cannot safely restore changed startup file: {}",
                edit.path.display()
            ));
        }
        match &edit.prior {
            Some(bytes) => write_profile(&edit.path, bytes, Some(edit.next.as_slice()))?,
            None => fs::remove_file(&edit.path).map_err(|error| error.to_string())?,
        }
    }
    Ok(())
}

fn cleanup_created_dirs(edits: &[ProfileEdit]) {
    for edit in edits {
        for directory in &edit.missing_parents {
            let _ = fs::remove_dir(directory);
        }
    }
}

fn verify(destination: &Path, plan: &ActivationHost, version: &str) -> Result<(), String> {
    let result = verify_fresh_shells(destination, plan, version);
    reclaim_terminal_foreground();
    result
}

/// An interactive probe shell (`-ic`/`-lic`) that inherits the terminal
/// enables job control and moves its own process group to the terminal's
/// foreground; it does not always hand the terminal back on exit. clud is
/// then a background group, and its next terminal read (the provider menu
/// that follows a verified install, #2219) fails with EIO. Take the
/// foreground back for clud's own group. SIGTTOU is ignored around the
/// call, as POSIX requires for a background group to change the foreground.
fn reclaim_terminal_foreground() {
    // SAFETY: plain libc calls on fd 0 and this process's own group; the
    // previous SIGTTOU disposition is restored before returning.
    unsafe {
        if libc::isatty(libc::STDIN_FILENO) != 1 {
            return;
        }
        let own = libc::getpgrp();
        if libc::tcgetpgrp(libc::STDIN_FILENO) == own {
            return;
        }
        let previous = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        let _ = libc::tcsetpgrp(libc::STDIN_FILENO, own);
        libc::signal(libc::SIGTTOU, previous);
    }
}

fn verify_fresh_shells(
    destination: &Path,
    plan: &ActivationHost,
    version: &str,
) -> Result<(), String> {
    let expected = format!("clud {version}");
    for mode in ["-lc", "-ic", "-lic"] {
        let mut env: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(key, _)| !matches!(key.as_str(), "PATH" | "BASH_ENV" | "ENV"))
            .collect();
        env.push(("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()));
        let command = vec![
            plan.shell.to_string_lossy().to_string(),
            mode.into(),
            "printf '__CLUD_INSTALL_PATH__'; command -v clud; printf '__CLUD_INSTALL_VERSION__'; clud --version".into(),
        ];
        let process = crate::subprocess::ManagedSubprocess::start(command, None, env, true, None)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut output = Vec::new();
        loop {
            if Instant::now() >= deadline {
                let _ = process.kill();
                return Err(format!("fresh shell {mode} lookup timed out"));
            }
            match process.read_stdout(Some(Duration::from_millis(100))) {
                ReadStatus::Line(line) => {
                    output.extend_from_slice(&line);
                    output.push(b'\n');
                }
                ReadStatus::Timeout => {
                    let _ = process.poll();
                }
                ReadStatus::Eof => break,
            }
            if output.len() > 32 * 1024 {
                let _ = process.kill();
                return Err("fresh shell output is too large".into());
            }
        }
        let code = process
            .wait(Some(Duration::from_secs(1)))
            .map_err(|error| error.to_string())?;
        let (actual, found_version) = parse_shell_probe(&output);
        if code != 0 || actual != destination.to_string_lossy() || found_version != expected {
            return Err(format!("fresh shell {mode} resolved {actual:?} with {found_version:?}; expected {} and {expected}", destination.display()));
        }
    }
    Ok(())
}

fn parse_shell_probe(output: &[u8]) -> (String, String) {
    let mut actual = String::new();
    let mut version = String::new();
    for line in String::from_utf8_lossy(output).lines() {
        if let Some(value) = line.strip_prefix("__CLUD_INSTALL_PATH__") {
            actual = value.to_owned();
        }
        if let Some(value) = line.strip_prefix("__CLUD_INSTALL_VERSION__") {
            version = value.to_owned();
        }
    }
    (actual, version)
}

#[cfg(test)]
mod tests {
    use super::{
        merge_managed_block, parse_shell_probe, plan_in, reload_hint, terminal_commands, ShellEnv,
        END, START,
    };
    use std::path::{Path, PathBuf};

    /// A fresh macOS-style account: `/Users/<name>`-like home, the default
    /// zsh login shell, and `~/.local/bin` (which macOS never puts on PATH).
    fn mac_env(home: &Path, shell: &str) -> ShellEnv {
        ShellEnv {
            home: home.to_path_buf(),
            shell: PathBuf::from(shell),
            zdotdir: None,
            xdg_config_home: None,
        }
    }

    fn dest(home: &Path) -> PathBuf {
        home.join(".local/bin/clud")
    }

    fn edit_paths(plan: &super::ActivationHost) -> Vec<PathBuf> {
        plan.edits.iter().map(|edit| edit.path.clone()).collect()
    }

    #[test]
    fn macos_default_zsh_activates_zprofile_and_zshrc() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(&dest(home.path()), &mac_env(home.path(), "/bin/zsh")).unwrap();
        assert_eq!(
            edit_paths(&plan),
            [home.path().join(".zprofile"), home.path().join(".zshrc")]
        );
        assert!(plan.manual_instruction.is_none());
        let bin = home.path().join(".local/bin");
        assert!(plan
            .snippet
            .contains(&format!("PATH={}:\"$PATH\"", bin.display())));
        for edit in &plan.edits {
            assert!(edit.prior.is_none());
            assert_eq!(edit.next, plan.snippet.as_bytes());
        }
    }

    #[test]
    fn macos_reload_hint_names_the_login_file_to_source() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(&dest(home.path()), &mac_env(home.path(), "/bin/zsh")).unwrap();
        let hint = reload_hint(&plan);
        assert!(hint.contains("Open a new terminal"), "{hint}");
        assert!(
            hint.contains(&format!(
                "source {}",
                home.path().join(".zprofile").display()
            )),
            "{hint}"
        );
        let manual = plan_in(&dest(home.path()), &mac_env(home.path(), "/bin/tcsh")).unwrap();
        assert_eq!(reload_hint(&manual), "Open a new terminal to use clud.");
    }

    #[test]
    fn macos_zsh_honors_zdotdir() {
        let home = tempfile::tempdir().unwrap();
        let zdot = home.path().join(".config/zsh");
        let mut env = mac_env(home.path(), "/bin/zsh");
        env.zdotdir = Some(zdot.clone());
        let plan = plan_in(&dest(home.path()), &env).unwrap();
        assert_eq!(
            edit_paths(&plan),
            [zdot.join(".zprofile"), zdot.join(".zshrc")]
        );
        env.zdotdir = Some(PathBuf::from("relative"));
        assert!(plan_in(&dest(home.path()), &env).is_err());
    }

    #[test]
    fn macos_bash_reuses_existing_profile_instead_of_shadowing_it() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".profile"), "export EDITOR=vim\n").unwrap();
        let plan = plan_in(&dest(home.path()), &mac_env(home.path(), "/bin/bash")).unwrap();
        assert_eq!(
            edit_paths(&plan),
            [home.path().join(".profile"), home.path().join(".bashrc")]
        );
        assert!(!home.path().join(".bash_profile").exists());
        let profile = String::from_utf8(plan.edits[0].next.clone()).unwrap();
        assert!(profile.starts_with("export EDITOR=vim\n"));
    }

    #[test]
    fn macos_bash_fresh_home_uses_bash_profile() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(&dest(home.path()), &mac_env(home.path(), "/bin/bash")).unwrap();
        assert_eq!(
            edit_paths(&plan),
            [
                home.path().join(".bash_profile"),
                home.path().join(".bashrc")
            ]
        );
    }

    #[test]
    fn macos_fish_writes_its_own_conf_d_file() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(
            &dest(home.path()),
            &mac_env(home.path(), "/opt/homebrew/bin/fish"),
        )
        .unwrap();
        assert_eq!(
            edit_paths(&plan),
            [home.path().join(".config/fish/conf.d/clud-path.fish")]
        );
        assert!(plan.snippet.contains("fish_add_path --path --move --"));
    }

    #[test]
    fn unmanaged_shell_gets_manual_instruction_and_no_edits() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(&dest(home.path()), &mac_env(home.path(), "/bin/tcsh")).unwrap();
        assert!(plan.edits.is_empty());
        assert!(plan.manual_instruction.unwrap().contains("export PATH="));
    }

    #[test]
    fn relative_home_or_shell_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let mut env = mac_env(Path::new("relative"), "/bin/zsh");
        assert!(plan_in(&dest(home.path()), &env).is_err());
        env = mac_env(home.path(), "zsh");
        assert!(plan_in(&dest(home.path()), &env).is_err());
    }

    #[test]
    fn rerun_keeps_one_block_and_user_text_byte_exact() {
        let path = Path::new("/x/.zprofile");
        let snippet = format!("{START}\nnew\n{END}\n");
        // No trailing newline in the user's file: append on its own line.
        let once = merge_managed_block(b"eval \"$(brew shellenv)\"", &snippet, path).unwrap();
        assert_eq!(
            once,
            format!("eval \"$(brew shellenv)\"\n{snippet}").into_bytes()
        );
        let twice = merge_managed_block(&once, &snippet, path).unwrap();
        assert_eq!(twice, once);
        // An older block in the middle is replaced in place.
        let old = format!("a\n{START}\nold\n{END}\nb\n");
        let merged = merge_managed_block(old.as_bytes(), &snippet, path).unwrap();
        assert_eq!(merged, format!("a\n{snippet}b\n").into_bytes());
    }

    #[test]
    fn ambiguous_or_reversed_markers_are_refused() {
        let path = Path::new("/x/.zshrc");
        let snippet = format!("{START}\nnew\n{END}\n");
        for text in [
            format!("{START}\n{END}\n{START}\n{END}\n"),
            format!("{END}\n{START}\n"),
            format!("{START}\nunterminated\n"),
        ] {
            assert!(
                merge_managed_block(text.as_bytes(), &snippet, path).is_err(),
                "{text}"
            );
        }
        assert!(merge_managed_block(&[0xff, 0xfe], &snippet, path).is_err());
    }

    #[test]
    fn terminal_commands_refuse_unrepresentable_path_entries() {
        for destination in ["/tmp/colon:home/bin/clud", "/tmp/line\nhome/bin/clud"] {
            assert!(terminal_commands(Path::new(destination)).is_err());
        }
    }

    #[test]
    fn fresh_shell_probe_ignores_interactive_warnings() {
        let output = b"bash: no job control in this shell\n__CLUD_INSTALL_PATH__/tmp/bin/clud\nbash: warning after path\n__CLUD_INSTALL_VERSION__clud 2.8.16\n";
        let (path, version) = parse_shell_probe(output);
        assert_eq!(path, "/tmp/bin/clud");
        assert_eq!(version, "clud 2.8.16");
    }
}

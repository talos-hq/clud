//! Rollout helpers for the native `cmd-scan` hook (formerly `block-bad-cmd`).
//! Since #1551 `clud-cmd-scan` is not a binary of its own but an argv[0] alias
//! of `clud` ([`crate::multicall`]), materialized in a helper-only directory.
//!
//! #489/#490 moved the command-guard policy into a native helper, but older
//! installs and hook configs can still route through
//! `clud tool run hooks/block-bad-cmd.py`. #532 renamed the native helper
//! itself from `clud-block-bad-cmd` to `clud-cmd-scan` (it now also does
//! eager GC tracking of `git clone`/`git worktree add`, not just command
//! blocking), so there are now two legacy command shapes to migrate away
//! from. This module owns the narrow first-run repair path: detect an
//! installed clud missing the helper and rewrite only the exact old managed
//! hook commands once the sibling helper is available.
//!
//! Project-scoped configs (`<repo>/.claude/settings*.json`,
//! `<repo>/.codex/hooks.json`) are usually committed and shared, so they are
//! never pinned to the launching executable's absolute path: that path exists
//! on one machine only, and writing it dirtied tracked files every time a dev
//! build or the test suite launched clud inside a checkout (#1333, #1426).
//! Only the legacy command shapes are rewritten, to the portable bare
//! `clud-cmd-scan`, and #1334 extended that to user configs too: an absolute
//! helper path pinned there dangles as soon as that build (a worktree, a
//! `uvx` copy) disappears. A user config still carrying such a dangling pin
//! returns to the bare name. The bare name resolves at run time through PATH;
//! `ensure_helper_on_session_path` makes that hold for a clud session whose
//! install directory is not on PATH.

use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};

const LEGACY_PYTHON_SHIM_COMMAND: &str = "clud tool run hooks/block-bad-cmd.py";
const LEGACY_PYTHON_SHIM_COMMAND_EXIT: &str =
    "clud tool run hooks/block-bad-cmd.py; exit $LASTEXITCODE";
const LEGACY_NATIVE_COMMAND: &str = "clud-block-bad-cmd";
const LEGACY_NATIVE_COMMAND_EXIT: &str = "clud-block-bad-cmd; exit $LASTEXITCODE";
const NEW_COMMAND: &str = "clud-cmd-scan";
const NEW_COMMAND_EXIT: &str = "clud-cmd-scan; exit $LASTEXITCODE";
const PINNED_PYTHON_SHIM_COMMAND: &str = "\"$CLUD_EXE\" tool run hooks/block-bad-cmd.py";
const PINNED_PYTHON_SHIM_COMMAND_EXIT: &str =
    "& $env:CLUD_EXE tool run hooks/block-bad-cmd.py; exit $LASTEXITCODE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallProbe {
    /// The running `clud`, which serves the `clud-cmd-scan` name.
    HelperPresent { path: PathBuf },
    /// The running program is not `clud` (a test harness), so nothing is
    /// exposed or migrated.
    NotInstalledLayout,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MigrationReport {
    pub files_changed: usize,
    pub commands_changed: usize,
    pub stale_commands_blocked: usize,
}

/// Where a hook config lives, which decides whether it may be pinned to the
/// launching executable (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigScope {
    /// Inside the repo; usually committed, so only portable rewrites.
    Project,
    /// Under the hook home; machine-local, so pinning is safe.
    User,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FileMigration {
    commands_changed: usize,
    stale_commands_blocked: usize,
}

pub fn run_startup_checks(auto_fix_hooks: bool) {
    ensure_helper_on_session_path();
    crate::hook_handshake::export_for_session();
    let sibling_helper_present =
        matches!(probe_current_install(), InstallProbe::HelperPresent { .. });

    if !auto_fix_hooks {
        return;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo_root = crate::loop_spec::git_root_from(&cwd);
    let home = hook_home_dir();
    match migrate_hook_configs_at(&repo_root, home.as_deref(), sibling_helper_present) {
        Ok(report) => {
            if let Err(error) = crate::scanner_hook_migration::migrate(&repo_root, home.as_deref())
            {
                eprintln!("[clud] warning: failed to consolidate scanner hooks: {error}");
            }
            if report.commands_changed > 0 {
                if !sibling_helper_present {
                    eprintln!("[clud] migrated legacy hook command to the CLUD_EXE shim");
                } else {
                    eprintln!(
                    "\x1b[32m[clud] migrated {count} block-bad-cmd hook command{plural} to native `{helper}`\x1b[0m",
                    count = report.commands_changed,
                    plural = if report.commands_changed == 1 { "" } else { "s" },
                    helper = NEW_COMMAND,
                );
                }
            }
            if report.stale_commands_blocked > 0 {
                eprintln!(
                    "[clud] warning: found {count} stale block-bad-cmd hook command{plural}, but `{helper}` is not on PATH; leaving compatibility shim wiring in place",
                    count = report.stale_commands_blocked,
                    plural = if report.stale_commands_blocked == 1 { "" } else { "s" },
                    helper = NEW_COMMAND,
                );
            }
        }
        Err(error) => {
            eprintln!("[clud] warning: failed to migrate block-bad-cmd hook config: {error}");
        }
    }
}

pub fn probe_current_install() -> InstallProbe {
    match std::env::current_exe() {
        Ok(path) => probe_install_at(&path),
        Err(_) => InstallProbe::NotInstalledLayout,
    }
}

pub fn probe_install_at(current_exe: &Path) -> InstallProbe {
    let is_clud = crate::shim_registry::invoked_name(current_exe.as_os_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("clud"));
    if is_clud && current_exe.is_file() {
        InstallProbe::HelperPresent {
            path: current_exe.to_path_buf(),
        }
    } else {
        InstallProbe::NotInstalledLayout
    }
}

pub fn native_helper_name() -> String {
    crate::shim_registry::file_name(crate::multicall::CMD_SCAN)
}

pub fn migrate_hook_configs_at(
    repo_root: &Path,
    home: Option<&Path>,
    helper_available: bool,
) -> io::Result<MigrationReport> {
    let mut report = MigrationReport::default();
    for (path, scope) in hook_config_paths(repo_root, home) {
        if !path.is_file() {
            continue;
        }
        let outcome = migrate_file(&path, helper_available, scope)?;
        if outcome.commands_changed > 0 {
            report.files_changed += 1;
            report.commands_changed += outcome.commands_changed;
        }
        report.stale_commands_blocked += outcome.stale_commands_blocked;
    }
    Ok(report)
}

const PROJECT_CONFIGS: [(&str, &str); 3] = [
    (".claude", "settings.json"),
    (".claude", "settings.local.json"),
    (".codex", "hooks.json"),
];
const USER_CONFIGS: [(&str, &str); 2] = [(".claude", "settings.json"), (".codex", "hooks.json")];

pub(crate) fn hook_config_paths(
    repo_root: &Path,
    home: Option<&Path>,
) -> Vec<(PathBuf, ConfigScope)> {
    let mut paths = Vec::new();
    for (dir, file) in PROJECT_CONFIGS {
        paths.push((repo_root.join(dir).join(file), ConfigScope::Project));
    }
    if let Some(home) = home {
        for (dir, file) in USER_CONFIGS {
            paths.push((home.join(dir).join(file), ConfigScope::User));
        }
    }
    paths
}

fn migrate_file(
    path: &Path,
    helper_available: bool,
    scope: ConfigScope,
) -> io::Result<FileMigration> {
    let text = std::fs::read_to_string(path)?;
    let mut json: Value = match serde_json::from_str(&text) {
        Ok(json) => json,
        Err(_) => return Ok(FileMigration::default()),
    };

    let stale = count_stale_commands(&json, scope);
    if stale == 0 {
        return Ok(FileMigration::default());
    }
    if !helper_available && !has_legacy_python_shim(&json) {
        return Ok(FileMigration {
            commands_changed: 0,
            stale_commands_blocked: stale,
        });
    }

    let mut changed = 0usize;
    migrate_value(&mut json, &mut changed, helper_available, scope);
    if changed == 0 {
        return Ok(FileMigration::default());
    }

    let mut body = serde_json::to_string_pretty(&json).map_err(io::Error::other)?;
    body.push('\n');
    std::fs::write(path, body)?;
    Ok(FileMigration {
        commands_changed: changed,
        stale_commands_blocked: count_stale_commands(&json, scope),
    })
}

fn count_stale_commands(value: &Value, scope: ConfigScope) -> usize {
    match value {
        Value::Object(map) => {
            let here = map
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| command_is_stale(command, scope))
                .map(|_| 1)
                .unwrap_or(0);
            let nested: usize = map.values().map(|v| count_stale_commands(v, scope)).sum();
            here + nested
        }
        Value::Array(values) => values.iter().map(|v| count_stale_commands(v, scope)).sum(),
        _ => 0,
    }
}

fn migrate_value(
    value: &mut Value,
    changed: &mut usize,
    helper_available: bool,
    scope: ConfigScope,
) {
    match value {
        Value::Object(map) => {
            let replacement = map
                .get("command")
                .and_then(Value::as_str)
                .and_then(|command| replacement_command_for(command, helper_available, scope));
            if let Some(replacement) = replacement {
                map.insert("command".to_string(), Value::String(replacement));
                *changed += 1;
            }
            for value in map.values_mut() {
                migrate_value(value, changed, helper_available, scope);
            }
        }
        Value::Array(values) => {
            for value in values {
                migrate_value(value, changed, helper_available, scope);
            }
        }
        _ => {}
    }
}

fn command_is_stale(command: &str, scope: ConfigScope) -> bool {
    replacement_command(command, scope).is_some()
}

fn has_legacy_python_shim(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| {
                    matches!(
                        command,
                        LEGACY_PYTHON_SHIM_COMMAND | LEGACY_PYTHON_SHIM_COMMAND_EXIT
                    )
                })
                || map.values().any(has_legacy_python_shim)
        }
        Value::Array(values) => values.iter().any(has_legacy_python_shim),
        _ => false,
    }
}

fn replacement_command_for(
    command: &str,
    helper_available: bool,
    scope: ConfigScope,
) -> Option<String> {
    if helper_available {
        // Portable only, in every scope: an absolute helper path is
        // machine-specific and short-lived (dev builds, worktrees, `uvx`
        // copies), and dangles the moment that binary goes away (#1334).
        // The session resolves the bare name through PATH instead
        // (`ensure_helper_on_session_path`).
        return replacement_command(command, scope).map(str::to_string);
    }
    match command {
        LEGACY_PYTHON_SHIM_COMMAND => Some(PINNED_PYTHON_SHIM_COMMAND.to_string()),
        LEGACY_PYTHON_SHIM_COMMAND_EXIT => Some(PINNED_PYTHON_SHIM_COMMAND_EXIT.to_string()),
        _ => None,
    }
}

/// The portable command a stale one migrates to, or `None` when `command` is
/// current. The bare helper is always current. In a user file, a helper
/// pinned to an absolute path that no longer exists (the old #1279 pinning)
/// is stale too and returns to the bare name (#1334).
fn replacement_command(command: &str, scope: ConfigScope) -> Option<&'static str> {
    match command {
        LEGACY_PYTHON_SHIM_COMMAND | LEGACY_NATIVE_COMMAND => Some(NEW_COMMAND),
        LEGACY_PYTHON_SHIM_COMMAND_EXIT | LEGACY_NATIVE_COMMAND_EXIT => Some(NEW_COMMAND_EXIT),
        _ if scope == ConfigScope::User => dangling_pinned_helper_replacement(command),
        _ => None,
    }
}

/// The bare replacement for a command that is exactly a pinned helper
/// (`'<abs>/clud-cmd-scan'` or `& '<abs>\clud-cmd-scan.exe'`, optionally
/// followed by `; exit $LASTEXITCODE`) whose program no longer exists.
fn dangling_pinned_helper_replacement(command: &str) -> Option<&'static str> {
    const EXIT_SUFFIX: &str = "; exit $LASTEXITCODE";
    let trimmed = command.trim();
    let (body, replacement) = match trimmed.strip_suffix(EXIT_SUFFIX) {
        Some(body) => (body.trim_end(), NEW_COMMAND_EXIT),
        None => (trimmed, NEW_COMMAND),
    };
    let quoted = body.strip_prefix("& ").unwrap_or(body);
    let program = hook_program(quoted)?;
    // Only a command that is the quoted program and nothing else; any other
    // shape is a user variant we must not touch.
    let exact =
        quoted.len() == program.len() + 2 && (quoted.starts_with('\'') || quoted.starts_with('"'));
    let path = Path::new(&program);
    let is_helper = path
        .file_stem()
        .is_some_and(|stem| stem == NEW_COMMAND || stem == LEGACY_NATIVE_COMMAND);
    (exact && is_helper && path.is_absolute() && !path.exists()).then_some(replacement)
}

/// The program a hook command runs: its first word as written, quotes
/// removed and variables left unexpanded. A leading `& ` is PowerShell's call
/// operator, so such a command is read as PowerShell.
pub fn hook_program(command: &str) -> Option<String> {
    let script = parse_hook_command(command);
    let invocation = script.invocations.first()?;
    Some(invocation.words.first()?.text().to_string())
}

/// Parse a hook command line from a settings file. Nothing is inherited, so
/// `$CLAUDE_PROJECT_DIR` and friends read as written.
pub fn parse_hook_command(command: &str) -> crate::shell_ast::Script {
    use crate::shell_ast::{BaseEnv, Dialect};
    let dialect = if command.trim_start().starts_with('&') {
        Dialect::PowerShell
    } else {
        Dialect::Posix
    };
    crate::shell_ast::parse(command, dialect, &BaseEnv::new(""))
}

/// Warnings for hook commands whose program is an absolute path that does
/// not exist. Such a hook fails open on every tool call with only a
/// non-blocking error line, so the command guard silently stops running
/// (#1334).
pub fn dangling_hook_program_warnings(repo_root: &Path, home: Option<&Path>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    for hook in crate::block_bad_cmd::frontend_hook_commands(repo_root, home) {
        let Some(program) = hook_program(&hook.command) else {
            continue;
        };
        let path = Path::new(&program);
        if path.is_absolute() && !path.exists() {
            seen.insert((hook.source.display().to_string(), program));
        }
    }
    seen.into_iter()
        .map(|(source, program)| {
            format!(
                "Hook command in {source} runs `{program}`, which does not exist, so the hook \
                 fails open on every tool call. Replace it with the portable `{NEW_COMMAND}` \
                 (resolved from PATH) or a program that exists on this machine."
            )
        })
        .collect()
}

/// Make the bare `clud-cmd-scan` hook command resolve for this session: when
/// it is not already on PATH but sits next to the running clud, expose it
/// through a helper-only directory appended to PATH, so every backend child
/// inherits it. Appended, not prepended, so it never shadows an installed
/// helper or the session shims.
///
/// The install directory itself is never put on PATH: it holds every other
/// program installed beside clud (a venv's `python`/`pip`/`soldr`, a `uv
/// tool` dir's companions), and exposing them changes what the session and
/// every descendant resolve by name (a venv's `soldr`, for instance, turns on
/// `.clud/settings.json` soldr routing in daemon workers). Appending the
/// install directory broke the Windows daemon integration tests after #1560.
/// The helper-only directory contains exactly one program.
pub fn ensure_helper_on_session_path() {
    if let InstallProbe::HelperPresent { path } = probe_current_install() {
        let path_env = std::env::var("PATH").unwrap_or_default();
        if let Some(launcher) = crate::shim_resolve::which("clud", &path_env) {
            if let Err(error) = expose_helper_beside_launcher(&launcher, &path) {
                eprintln!(
                    "[clud] warning: cannot expose scanner beside {}: {error}",
                    launcher.display()
                );
            }
        }
    }
    let path_env = std::env::var("PATH").unwrap_or_default();
    if let Some(found) = crate::shim_resolve::which(&native_helper_name(), &path_env) {
        if let InstallProbe::HelperPresent { path } = probe_current_install() {
            let home = crate::home::user_home();
            if let Some(warning) = refresh_found_helper(&found, &path, home.as_deref()) {
                eprintln!("{warning}");
            }
        }
        return;
    }
    let InstallProbe::HelperPresent { path } = probe_current_install() else {
        return;
    };
    let Some(home) = crate::home::user_home() else {
        return;
    };
    let Ok(dir) = expose_helper_at(&home, &path) else {
        return;
    };
    if let Some(updated) = path_with_appended(&path_env, &dir) {
        // SAFETY: startup-only write, before the backend is spawned and
        // before any thread that reads the environment runs.
        unsafe { std::env::set_var("PATH", updated) };
    }
}

/// Login shells can rebuild PATH and discard helper-bin. Keep the scanner
/// beside the public launcher too, including a uv tool's symlink in local/bin.
/// Existing aliases beside another installation remain untouched.
fn expose_helper_beside_launcher(launcher: &Path, source: &Path) -> io::Result<()> {
    let Some(dir) = launcher.parent() else {
        return Ok(());
    };
    let scanner = dir.join(native_helper_name());
    if scanner.exists() && !same_file::is_same_file(launcher, source).unwrap_or(false) {
        return Ok(());
    }
    crate::alias_link::install_alias(source, &scanner)?;
    Ok(())
}

/// Bring a `clud-cmd-scan` already on PATH up to the running `clud` when clud
/// owns its location: beside the running binary, or the helper-only
/// directory. An upgrade replaces `clud` but not these aliases, and the hook
/// then runs the old build's rules (#1949: the `/loop` stop guard never ran).
/// A helper anywhere else belongs to someone else and is left alone. A
/// `clud-shim` left beside `clud` by a pre-#1551 install is refreshed too;
/// the interpreter and tool names (`python`, `rm`, ...) never are, because
/// beside `clud` in a virtualenv they are the real programs.
/// Returns whether `found` was in a clud-owned location.
fn refresh_owned_helper(found: &Path, clud: &Path, home: Option<&Path>) -> bool {
    let Some(dir) = found.parent() else {
        return false;
    };
    let beside_clud = clud
        .parent()
        .is_some_and(|clud_dir| same_dir(dir, clud_dir));
    let helper_bin = home.is_some_and(|home| same_dir(dir, &home.join(HELPER_BIN_SUBDIR)));
    if !beside_clud && !helper_bin {
        return false;
    }
    for name in [
        crate::multicall::CMD_SCAN,
        crate::multicall::LEGACY_CMD_SCAN,
        crate::multicall::SHIM,
    ] {
        let target = dir.join(crate::shim_registry::file_name(name));
        if name == crate::multicall::CMD_SCAN || target.is_file() {
            // Best effort: on Windows a running helper cannot be replaced,
            // and the existing one still works.
            let _ = crate::alias_link::install_alias(clud, &target);
        }
    }
    true
}

/// Whether `a` and `b` hold the same build. Windows std has no file identity,
/// so a hardlinked or copied alias is recognized by its bytes; only this
/// once-per-launch warning pays for the read.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(meta_a), Ok(meta_b)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    meta_a.len() == meta_b.len()
        && matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// The launch warning for a `clud-cmd-scan` that PATH finds first but that is
/// not the running `clud` and sits where clud may not replace it: every hook
/// in the session would run that other build's rules, silently (#1954).
fn foreign_helper_warning(found: &Path, clud: &Path, owned: bool) -> Option<String> {
    let current = clud.canonicalize().ok()?;
    if owned || crate::alias_link::is_alias_of(&current, found) || same_bytes(&current, found) {
        return None;
    }
    Some(format!(
        "clud: warning: {} is first on PATH and is not this clud ({}); \
         its hooks will enforce that build's rules. Remove it or put {} first.",
        found.display(),
        clud.display(),
        clud.parent()
            .map_or_else(String::new, |dir| dir.display().to_string())
    ))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Under the user's home: the helper-only PATH directory.
const HELPER_BIN_SUBDIR: &str = ".clud/state/helper-bin";

/// Place the scanner names alone in the helper-only directory under `home`
/// as aliases of `clud` (hardlink, then symlink, then copy; see
/// [`crate::alias_link`]) and return that directory. An up-to-date entry is
/// left untouched; a stale one (after an upgrade) is replaced through a temp
/// file and rename. If the replacement fails (on Windows a running helper
/// cannot be replaced) an existing entry is still used.
fn expose_helper_at(home: &Path, clud: &Path) -> io::Result<PathBuf> {
    let dir = home.join(HELPER_BIN_SUBDIR);
    std::fs::create_dir_all(&dir)?;
    for name in [
        crate::multicall::CMD_SCAN,
        crate::multicall::LEGACY_CMD_SCAN,
    ] {
        let target = dir.join(crate::shim_registry::file_name(name));
        if let Err(error) = crate::alias_link::install_alias(clud, &target) {
            if !target.is_file() {
                return Err(error);
            }
        }
    }
    Ok(dir)
}

/// The launch step when PATH already finds a `clud-cmd-scan`: refresh it if
/// clud owns it, and refresh the helper-only directory too. That directory
/// is not what PATH found, so without the second step its aliases outlive
/// every upgrade while another `clud-cmd-scan` is first on PATH. Returns the
/// foreign-helper warning, if any.
fn refresh_found_helper(found: &Path, clud: &Path, home: Option<&Path>) -> Option<String> {
    let owned = refresh_owned_helper(found, clud, home);
    if let Some(home) = home {
        refresh_helper_bin(home, clud);
    }
    foreign_helper_warning(found, clud, owned)
}

/// Bring an existing helper-only directory up to `clud`. Nothing is created:
/// a directory clud never populated stays absent. The per-entry check is the
/// same metadata freshness test [`expose_helper_at`] pays on every launch.
fn refresh_helper_bin(home: &Path, clud: &Path) {
    if home.join(HELPER_BIN_SUBDIR).is_dir() {
        let _ = expose_helper_at(home, clud);
    }
}

fn path_with_appended(path_env: &str, dir: &Path) -> Option<String> {
    let mut entries: Vec<PathBuf> = std::env::split_paths(path_env)
        .filter(|entry| !entry.as_os_str().is_empty())
        .collect();
    if entries.iter().any(|entry| entry == dir) {
        return None;
    }
    entries.push(dir.to_path_buf());
    std::env::join_paths(entries)
        .ok()
        .and_then(|joined| joined.into_string().ok())
}

fn hook_home_dir() -> Option<PathBuf> {
    std::env::var_os("CLUD_HOOK_HOME")
        .map(PathBuf::from)
        .or_else(crate::home::user_home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn the_running_clud_is_the_helper() {
        let tmp = tempdir().unwrap();
        let clud = tmp.path().join(crate::shim_registry::file_name("clud"));
        write(&clud, "");

        assert_eq!(
            probe_install_at(&clud),
            InstallProbe::HelperPresent { path: clud }
        );
    }

    #[test]
    fn a_test_harness_is_not_the_installed_layout() {
        let tmp = tempdir().unwrap();
        let harness = tmp
            .path()
            .join(crate::shim_registry::file_name("clud-3f9a1c"));
        write(&harness, "");

        assert_eq!(probe_install_at(&harness), InstallProbe::NotInstalledLayout);
        assert_eq!(
            probe_install_at(&tmp.path().join("clud")),
            InstallProbe::NotInstalledLayout,
            "a missing file is not an install"
        );
    }

    #[test]
    fn public_launcher_gets_a_scanner_without_exposing_other_tools() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("tool-env/clud");
        write(&source, "current-build");
        let public = tmp.path().join("public/clud");
        std::fs::create_dir_all(public.parent().unwrap()).unwrap();
        crate::alias_link::install_alias(&source, &public).unwrap();
        expose_helper_beside_launcher(&public, &source).unwrap();
        let scanner = public.parent().unwrap().join(native_helper_name());
        assert!(crate::alias_link::is_fresh(&source, &scanner));
        assert!(!public.parent().unwrap().join("python").exists());

        let foreign = tmp.path().join("foreign/clud");
        write(&foreign, "other-build");
        expose_helper_beside_launcher(&foreign, &source).unwrap();
        assert!(foreign
            .parent()
            .unwrap()
            .join(native_helper_name())
            .exists());
    }

    #[test]
    fn migrates_exact_claude_and_codex_commands_when_helper_available() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        write(
            &home.join(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"clud tool run hooks/block-bad-cmd.py"}]}]}}"#,
        );
        write(
            &home.join(".codex/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"clud tool run hooks/block-bad-cmd.py; exit $LASTEXITCODE"}]}]}}"#,
        );

        let report = migrate_hook_configs_at(&repo, Some(&home), true).unwrap();

        assert_eq!(report.files_changed, 2);
        assert_eq!(report.commands_changed, 2);
        assert_eq!(report.stale_commands_blocked, 0);
        let claude: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
                .unwrap();
        let codex: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".codex/hooks.json")).unwrap())
                .unwrap();
        assert_eq!(
            claude["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            NEW_COMMAND
        );
        assert_eq!(
            codex["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            NEW_COMMAND_EXIT
        );

        let second = migrate_hook_configs_at(&repo, Some(&home), true).unwrap();
        assert_eq!(second, MigrationReport::default());
    }

    #[test]
    fn migrates_legacy_native_block_bad_cmd_command_to_cmd_scan() {
        // #532: a hook config already migrated once (python shim ->
        // `clud-block-bad-cmd`) must also be carried forward to the
        // renamed `clud-cmd-scan` binary.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        write(
            &home.join(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"clud-block-bad-cmd"}]}]}}"#,
        );
        write(
            &home.join(".codex/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"clud-block-bad-cmd; exit $LASTEXITCODE"}]}]}}"#,
        );

        let report = migrate_hook_configs_at(&repo, Some(&home), true).unwrap();

        assert_eq!(report.files_changed, 2);
        assert_eq!(report.commands_changed, 2);
        assert_eq!(report.stale_commands_blocked, 0);

        let claude: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
                .unwrap();
        let codex: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".codex/hooks.json")).unwrap())
                .unwrap();
        assert_eq!(
            claude["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            NEW_COMMAND
        );
        assert_eq!(
            codex["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            NEW_COMMAND_EXIT
        );
    }

    #[test]
    fn missing_helper_blocks_rewrite_of_legacy_native_command() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let path = home.join(".claude/settings.json");
        write(
            &path,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"command":"clud-block-bad-cmd"}]}]}}"#,
        );

        let report = migrate_hook_configs_at(&repo, Some(&home), false).unwrap();

        assert_eq!(report.files_changed, 0);
        assert_eq!(report.commands_changed, 0);
        assert_eq!(report.stale_commands_blocked, 1);
        assert!(fs::read_to_string(path)
            .unwrap()
            .contains("clud-block-bad-cmd"));
    }

    #[test]
    fn user_bare_helper_is_left_byte_for_byte_unchanged() {
        // #1334: the bare helper resolves from PATH at run time; pinning it
        // to the launching binary is what left dangling hooks behind.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let body = r#"{"hooks":[{"command":"clud-cmd-scan"},{"command":"clud-cmd-scan; exit $LASTEXITCODE"}]}"#;
        let claude = home.join(".claude/settings.json");
        let codex = home.join(".codex/hooks.json");
        write(&claude, body);
        write(&codex, body);

        for helper_available in [true, false] {
            let report = migrate_hook_configs_at(&repo, Some(&home), helper_available).unwrap();
            assert_eq!(report, MigrationReport::default());
        }
        assert_eq!(fs::read_to_string(claude).unwrap(), body);
        assert_eq!(fs::read_to_string(codex).unwrap(), body);
    }

    #[test]
    fn migration_never_writes_an_absolute_path_in_any_scope() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let legacy = r#"{"hooks":[{"command":"clud tool run hooks/block-bad-cmd.py"},{"command":"clud tool run hooks/block-bad-cmd.py; exit $LASTEXITCODE"},{"command":"clud-block-bad-cmd"}]}"#;
        for path in [
            repo.join(".claude/settings.json"),
            repo.join(".codex/hooks.json"),
            home.join(".claude/settings.json"),
            home.join(".codex/hooks.json"),
        ] {
            write(&path, legacy);
        }

        migrate_hook_configs_at(&repo, Some(&home), true).unwrap();

        let exe_dir = std::env::current_exe().unwrap();
        let exe_dir = exe_dir.parent().unwrap().to_string_lossy().into_owned();
        for path in [
            repo.join(".claude/settings.json"),
            repo.join(".codex/hooks.json"),
            home.join(".claude/settings.json"),
            home.join(".codex/hooks.json"),
        ] {
            let text = fs::read_to_string(&path).unwrap();
            assert!(!text.contains(&exe_dir), "{}: {text}", path.display());
            let config: Value = serde_json::from_str(&text).unwrap();
            let mut commands = Vec::new();
            collect_commands(&config, &mut commands);
            assert_eq!(
                commands,
                vec![NEW_COMMAND, NEW_COMMAND_EXIT, NEW_COMMAND],
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn user_dangling_pinned_helper_returns_to_the_bare_name() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let gone = tmp.path().join("gone/target/debug/clud-cmd-scan");
        let gone = gone.to_string_lossy();
        let pinned = serde_json::json!({"hooks": [
            {"command": format!("'{gone}'")},
            {"command": format!("& '{gone}'; exit $LASTEXITCODE")},
        ]});
        let path = home.join(".claude/settings.json");
        write(&path, &pinned.to_string());

        let report = migrate_hook_configs_at(&repo, Some(&home), true).unwrap();

        assert_eq!(report.commands_changed, 2);
        let config: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(config["hooks"][0]["command"], NEW_COMMAND);
        assert_eq!(config["hooks"][1]["command"], NEW_COMMAND_EXIT);
    }

    #[test]
    fn existing_pins_and_project_pins_are_not_rewritten() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let live = tmp.path().join("bin").join(native_helper_name());
        write(&live, "");
        let gone = tmp.path().join("gone/clud-cmd-scan");
        let user = serde_json::json!({"hooks": [{"command": format!("'{}'", live.display())}]})
            .to_string();
        let project = serde_json::json!({"hooks": [{"command": format!("'{}'", gone.display())}]})
            .to_string();
        write(&home.join(".claude/settings.json"), &user);
        write(&repo.join(".claude/settings.json"), &project);

        let report = migrate_hook_configs_at(&repo, Some(&home), true).unwrap();

        assert_eq!(report, MigrationReport::default());
        assert_eq!(
            fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
            user
        );
        assert_eq!(
            fs::read_to_string(repo.join(".claude/settings.json")).unwrap(),
            project
        );
    }

    #[test]
    fn hook_program_extracts_the_first_word() {
        assert_eq!(
            hook_program("clud-cmd-scan").as_deref(),
            Some("clud-cmd-scan")
        );
        assert_eq!(
            hook_program("clud-cmd-scan; exit $LASTEXITCODE").as_deref(),
            Some("clud-cmd-scan")
        );
        assert_eq!(
            hook_program("'/a b/clud-cmd-scan'").as_deref(),
            Some("/a b/clud-cmd-scan")
        );
        assert_eq!(
            hook_program("& 'C:\\x\\clud-cmd-scan.exe'; exit $LASTEXITCODE").as_deref(),
            Some("C:\\x\\clud-cmd-scan.exe")
        );
        assert_eq!(
            hook_program("python .codex/hooks/check-soldr.py").as_deref(),
            Some("python")
        );
        assert_eq!(hook_program("   "), None);
    }

    /// #1949: an upgrade replaced `clud` but left the `clud-cmd-scan` beside
    /// it from the old build, which PATH found first, so every hook ran the
    /// old rules. A helper clud owns is refreshed; anyone else's is not.
    #[test]
    fn a_stale_helper_clud_owns_is_refreshed_and_a_foreign_one_is_not() {
        let tmp = tempdir().unwrap();
        let install = tmp.path().join("install");
        let clud = install.join(crate::shim_registry::file_name("clud"));
        write(&clud, "clud-v2-longer");
        let beside = install.join(native_helper_name());
        write(&beside, "clud-v1");
        let home = tmp.path().join("home");
        let helper_bin = home.join(HELPER_BIN_SUBDIR).join(native_helper_name());
        write(&helper_bin, "clud-v1");
        let foreign = tmp.path().join("elsewhere").join(native_helper_name());
        write(&foreign, "someone-else");

        assert!(refresh_owned_helper(&beside, &clud, Some(&home)));
        assert!(refresh_owned_helper(&helper_bin, &clud, Some(&home)));
        assert!(!refresh_owned_helper(&foreign, &clud, Some(&home)));

        assert_eq!(std::fs::read_to_string(&beside).unwrap(), "clud-v2-longer");
        assert_eq!(
            std::fs::read_to_string(&helper_bin).unwrap(),
            "clud-v2-longer"
        );
        assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "someone-else");
    }

    /// #1954: a pre-#1551 `clud-shim` sat stale beside `clud`; it is
    /// refreshed with the scanner, but the real `python` there is not.
    #[test]
    fn refresh_covers_a_stale_clud_shim_and_never_an_interpreter() {
        let tmp = tempdir().unwrap();
        let install = tmp.path().join("install");
        let clud = install.join(crate::shim_registry::file_name("clud"));
        write(&clud, "clud-v2-longer");
        let scan = install.join(native_helper_name());
        write(&scan, "clud-v1");
        let shim = install.join(crate::shim_registry::file_name(crate::multicall::SHIM));
        write(&shim, "clud-v1");
        let python = install.join(crate::shim_registry::file_name("python"));
        write(&python, "real-python");

        assert!(refresh_owned_helper(&scan, &clud, None));

        assert_eq!(std::fs::read_to_string(&shim).unwrap(), "clud-v2-longer");
        assert_eq!(std::fs::read_to_string(&python).unwrap(), "real-python");
    }

    #[test]
    fn a_foreign_helper_first_on_path_is_warned_about() {
        let tmp = tempdir().unwrap();
        let clud = tmp
            .path()
            .join("install")
            .join(crate::shim_registry::file_name("clud"));
        write(&clud, "clud-v2");
        let foreign = tmp.path().join("elsewhere").join(native_helper_name());
        write(&foreign, "clud-v1");
        let warning = foreign_helper_warning(&foreign, &clud, false).unwrap();
        assert!(
            warning.contains(&foreign.display().to_string()),
            "{warning}"
        );
        assert_eq!(foreign_helper_warning(&foreign, &clud, true), None);
        let alias = tmp.path().join("install").join(native_helper_name());
        crate::alias_link::install_alias(&clud, &alias).unwrap();
        assert_eq!(foreign_helper_warning(&alias, &clud, false), None);
    }

    /// #2514-style regression: a foreign `clud-cmd-scan` first on PATH made
    /// the launch skip helper-bin, so its aliases stayed on the old build.
    #[test]
    fn a_stale_helper_bin_is_refreshed_while_a_foreign_helper_is_first_on_path() {
        let tmp = tempdir().unwrap();
        let clud = tmp
            .path()
            .join("install")
            .join(crate::shim_registry::file_name("clud"));
        write(&clud, "clud-v2-current");
        let home = tmp.path().join("home");
        let decoy = home.join(HELPER_BIN_SUBDIR).join(native_helper_name());
        write(&decoy, "clud-v1");
        let foreign = tmp.path().join("venv").join(native_helper_name());
        write(&foreign, "clud-v0");

        let warning = refresh_found_helper(&foreign, &clud, Some(&home));

        assert!(
            warning.is_some(),
            "the foreign helper is still warned about"
        );
        assert!(
            crate::alias_link::is_fresh(&clud.canonicalize().unwrap(), &decoy),
            "helper-bin entry was not refreshed"
        );
        assert_eq!(fs::read_to_string(&decoy).unwrap(), "clud-v2-current");
        assert_eq!(fs::read_to_string(&foreign).unwrap(), "clud-v0");
    }

    #[test]
    fn an_up_to_date_helper_bin_entry_is_left_untouched() {
        let tmp = tempdir().unwrap();
        let clud = tmp.path().join("install").join("clud");
        write(&clud, "clud-v2");
        let home = tmp.path().join("home");
        let dir = expose_helper_at(&home, &clud).unwrap();
        let entry = dir.join(native_helper_name());
        let before = fs::symlink_metadata(&entry).unwrap();
        let foreign = tmp.path().join("venv").join(native_helper_name());
        write(&foreign, "clud-v0");

        refresh_found_helper(&foreign, &clud, Some(&home));

        let after = fs::symlink_metadata(&entry).unwrap();
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert!(same_file::is_same_file(&clud, &entry).unwrap() || before.len() == after.len());
    }

    #[test]
    fn a_missing_helper_bin_is_not_created_by_the_refresh() {
        let tmp = tempdir().unwrap();
        let clud = tmp.path().join("install").join("clud");
        write(&clud, "clud-v2");
        let home = tmp.path().join("home");
        let foreign = tmp.path().join("venv").join(native_helper_name());
        write(&foreign, "clud-v0");
        refresh_found_helper(&foreign, &clud, Some(&home));
        assert!(!home.join(HELPER_BIN_SUBDIR).exists());
    }

    #[test]
    fn helper_is_exposed_alone_not_its_install_directory() {
        let tmp = tempdir().unwrap();
        let install = tmp.path().join("install");
        let clud = install.join(crate::shim_registry::file_name("clud"));
        write(&clud, "clud-v1");
        write(&install.join("soldr"), "must not be exposed");
        let home = tmp.path().join("home");

        let dir = expose_helper_at(&home, &clud).unwrap();

        assert_ne!(dir, install);
        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        let mut expected = vec![
            crate::shim_registry::file_name(crate::multicall::CMD_SCAN),
            crate::shim_registry::file_name(crate::multicall::LEGACY_CMD_SCAN),
        ];
        expected.sort();
        assert_eq!(entries, expected);
        let exposed = dir.join(native_helper_name());
        assert_eq!(std::fs::read_to_string(&exposed).unwrap(), "clud-v1");

        // An upgraded clud (new file, as an installer writes it) replaces the stale entry.
        std::fs::remove_file(&clud).unwrap();
        write(&clud, "clud-v2-longer");
        assert_eq!(expose_helper_at(&home, &clud).unwrap(), dir);
        assert_eq!(std::fs::read_to_string(&exposed).unwrap(), "clud-v2-longer");
    }

    #[test]
    fn dangling_hook_program_produces_a_warning_naming_file_and_fix() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        // Build paths component-wise so they display with native separators,
        // matching the settings path the scanner reports on Windows.
        let gone = tmp
            .path()
            .join("gone")
            .join("target")
            .join("debug")
            .join("clud-cmd-scan");
        let body = serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
            {"type": "command", "command": format!("'{}'", gone.display())},
            {"type": "command", "command": "clud-cmd-scan"},
        ]}]}})
        .to_string();
        let settings = repo.join(".claude").join("settings.json");
        write(&settings, &body);

        let warnings = dangling_hook_program_warnings(&repo, None);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = &warnings[0];
        assert!(
            warning.contains(&settings.display().to_string()),
            "{warning}"
        );
        assert!(warning.contains(&gone.display().to_string()), "{warning}");
        assert!(warning.contains("does not exist"), "{warning}");
        assert!(warning.contains(NEW_COMMAND), "{warning}");
        // The launch hook-health report carries it.
        let report = crate::hook_health::inspect_paths(&repo, None);
        assert!(
            report.warnings.iter().any(|w| w == warning),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn helper_dir_is_appended_to_path_once() {
        let tmp = tempdir().unwrap();
        let a = tmp.path().join("a");
        let dir = tmp.path().join("helper");
        let base = std::env::join_paths([&a]).unwrap().into_string().unwrap();
        let updated = path_with_appended(&base, &dir).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&updated).collect();
        assert_eq!(entries, vec![a, dir.clone()]);
        assert_eq!(path_with_appended(&updated, &dir), None);
        assert_eq!(path_with_appended("", &dir).unwrap(), dir.to_string_lossy());
    }

    #[test]
    fn missing_helper_blocks_rewrite_and_preserves_compatibility_command() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let path = home.join(".claude/settings.json");
        write(
            &path,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"command":"clud tool run hooks/block-bad-cmd.py"}]}]}}"#,
        );

        let report = migrate_hook_configs_at(&repo, Some(&home), false).unwrap();

        assert_eq!(report.files_changed, 1);
        assert_eq!(report.commands_changed, 1);
        assert_eq!(report.stale_commands_blocked, 0);
        let config: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            config["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            PINNED_PYTHON_SHIM_COMMAND
        );
    }

    #[test]
    fn non_exact_user_variants_are_untouched() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let body = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"command":"python wrapper.py && clud tool run hooks/block-bad-cmd.py"},{"command":" clud tool run hooks/block-bad-cmd.py "}]}]}}"#;
        let path = home.join(".claude/settings.json");
        write(&path, body);

        let report = migrate_hook_configs_at(&repo, Some(&home), true).unwrap();

        assert_eq!(report, MigrationReport::default());
        assert_eq!(fs::read_to_string(path).unwrap(), body);
    }

    /// This repo's own committed hook configs, as a launch inside the
    /// checkout sees them (#1333, #1426).
    const COMMITTED_CLAUDE_SETTINGS: &str = include_str!("../../../.claude/settings.json");
    const COMMITTED_CODEX_HOOKS: &str = include_str!("../../../.codex/hooks.json");

    const PROJECT_CONFIG_RELS: [&str; 3] = [
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".codex/hooks.json",
    ];

    #[test]
    fn project_bare_helper_is_never_pinned_to_the_launching_install() {
        // #1426: a dev build (or the test suite) launched inside a checkout
        // rewrote the committed `clud-cmd-scan` to its own absolute path.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let body = r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"command":"clud-cmd-scan"},{"command":"clud-cmd-scan; exit $LASTEXITCODE"}]}]}}"#;
        for rel in PROJECT_CONFIG_RELS {
            write(&repo.join(rel), body);
        }

        for helper_available in [true, false] {
            let report = migrate_hook_configs_at(&repo, Some(&home), helper_available).unwrap();
            assert_eq!(report, MigrationReport::default());
        }
        for rel in PROJECT_CONFIG_RELS {
            assert_eq!(fs::read_to_string(repo.join(rel)).unwrap(), body, "{rel}");
        }
    }

    #[test]
    fn project_legacy_commands_migrate_to_the_portable_bare_helper() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let path = repo.join(".claude/settings.json");
        write(
            &path,
            r#"{"hooks":[{"command":"clud tool run hooks/block-bad-cmd.py"},{"command":"clud-block-bad-cmd; exit $LASTEXITCODE"}]}"#,
        );

        let report = migrate_hook_configs_at(&repo, None, true).unwrap();

        assert_eq!(report.files_changed, 1);
        assert_eq!(report.commands_changed, 2);
        assert_eq!(report.stale_commands_blocked, 0);
        let config: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(config["hooks"][0]["command"], NEW_COMMAND);
        assert_eq!(config["hooks"][1]["command"], NEW_COMMAND_EXIT);
    }

    #[test]
    fn committed_repo_hook_configs_use_the_portable_bare_helper() {
        // #1333 and #1428 both committed a machine-local helper path. Since
        // #1954 the repo carries no scanner line at all, so it sees the
        // coverage a user's launch delivers; any line that returns must
        // still be the bare helper.
        for (name, body) in [
            (".claude/settings.json", COMMITTED_CLAUDE_SETTINGS),
            (".codex/hooks.json", COMMITTED_CODEX_HOOKS),
        ] {
            let json: Value = serde_json::from_str(body).unwrap();
            let mut commands = Vec::new();
            collect_commands(&json, &mut commands);
            let scans: Vec<_> = commands
                .iter()
                .filter(|command| command.contains("clud-cmd-scan"))
                .collect();
            assert!(
                scans.is_empty(),
                "{name}: a repo scanner line hides matcher gaps from clud developers (#1954)"
            );
            for command in scans {
                assert_eq!(command, NEW_COMMAND, "{name} must not pin clud-cmd-scan");
            }
        }
    }

    #[test]
    fn committed_repo_hook_configs_survive_a_launch_unchanged() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let claude = repo.join(".claude/settings.json");
        let codex = repo.join(".codex/hooks.json");
        write(&claude, COMMITTED_CLAUDE_SETTINGS);
        write(&codex, COMMITTED_CODEX_HOOKS);

        for helper_available in [true, false] {
            let report = migrate_hook_configs_at(&repo, None, helper_available).unwrap();
            assert_eq!(report, MigrationReport::default());
        }
        let claude_after = fs::read_to_string(claude).unwrap();
        let codex_after = fs::read_to_string(codex).unwrap();
        assert_eq!(claude_after, COMMITTED_CLAUDE_SETTINGS);
        assert_eq!(codex_after, COMMITTED_CODEX_HOOKS);
    }

    fn collect_commands(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(command) = map.get("command").and_then(Value::as_str) {
                    out.push(command.to_string());
                }
                for value in map.values() {
                    collect_commands(value, out);
                }
            }
            Value::Array(values) => {
                for value in values {
                    collect_commands(value, out);
                }
            }
            _ => {}
        }
    }
}

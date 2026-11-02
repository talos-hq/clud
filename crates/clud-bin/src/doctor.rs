//! `clud doctor` (#1954): report hook and alias health that a launch cannot
//! see. Every alias in clud's own alias dirs must be the running build and
//! present, no retired alias may remain, and the first `clud-cmd-scan` on
//! PATH must be this build too. Files in those dirs that are not clud
//! aliases are listed, never deleted. Matcher coverage is computed from the
//! hook configs actually on disk, beside the fragment a clud launch passes,
//! so the compiled list is never mistaken for proof of delivery.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{json, Value};

use crate::clud_hooks_compile::GUARDED_TOOLS;
use crate::hook_handshake::fingerprint;

/// One problem `clud doctor` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// An alias whose build differs from the running `clud`.
    Stale { path: PathBuf, found: String },
    /// A retired alias name still on disk.
    Retired { path: PathBuf },
    /// An alias missing from a dir clud has populated.
    Missing { path: PathBuf },
    /// The first `clud-cmd-scan` on PATH is another build.
    ForeignScanner { path: PathBuf, found: String },
    /// A clud-owned name resolves to a toolstore build (or none) other than
    /// the launcher's (#2558).
    WrongBuild {
        path: PathBuf,
        found: String,
        launcher: String,
    },
}

impl Finding {
    /// The file the finding is about.
    pub fn path(&self) -> &Path {
        match self {
            Self::Stale { path, .. } | Self::Retired { path } | Self::Missing { path } => path,
            Self::ForeignScanner { path, .. } | Self::WrongBuild { path, .. } => path,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Stale { .. } => "stale",
            Self::Retired { .. } => "retired",
            Self::Missing { .. } => "missing",
            Self::ForeignScanner { .. } => "foreign_scanner",
            Self::WrongBuild { .. } => "wrong_build",
        }
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.path().display();
        match self {
            Self::Stale { found, .. } => {
                let note = format!("build {found}");
                write!(f, "stale alias {path} ({note})")
            }
            Self::Retired { .. } => {
                let note = "remains";
                write!(f, "retired alias {path} {note}")
            }
            Self::Missing { .. } => write!(f, "alias {path} is missing"),
            Self::WrongBuild {
                found, launcher, ..
            } => write!(f, "alias {path} runs build {found}; launcher is {launcher}"),
            Self::ForeignScanner { found, .. } => {
                let note = "hooks relay to the launcher";
                write!(
                    f,
                    "first clud-cmd-scan on PATH is {path} (build {found}); {note}"
                )
            }
        }
    }
}

/// What one alias dir holds against the names clud installs there.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DirReport {
    pub findings: Vec<Finding>,
    /// Entries that are neither a current nor a retired alias. Reported only:
    /// clud does not own them, so it never deletes them.
    pub unknown: Vec<PathBuf>,
}

/// Check the on-disk `files` and `retired` names in `dir` against the
/// running build `own`. With `owned`, the dir is clud's alone: a missing
/// alias is a finding and other entries are listed as unknown.
pub fn inspect_dir(
    dir: &Path,
    files: &[String],
    retired: &[String],
    own: &str,
    owned: bool,
) -> DirReport {
    let mut report = DirReport::default();
    let populated = owned && dir.is_dir();
    for file in files {
        let path = dir.join(file);
        match fingerprint(&path) {
            Some(found) if found != own => report.findings.push(Finding::Stale { path, found }),
            Some(_) => {}
            None if populated && std::fs::symlink_metadata(&path).is_err() => {
                report.findings.push(Finding::Missing { path });
            }
            None => {}
        }
    }
    for file in retired {
        let path = dir.join(file);
        if std::fs::symlink_metadata(&path).is_ok() {
            report.findings.push(Finding::Retired { path });
        }
    }
    if populated {
        report.unknown = unknown_entries(dir, files, retired);
    }
    report
}

/// Entries of `dir` that clud did not put there. A dot-file is an
/// installer's staging copy (`.<name>.<pid>.<n>.tmp`), possibly another
/// process's in flight, so it is not listed.
fn unknown_entries(dir: &Path, files: &[String], retired: &[String]) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut unknown: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            !name.starts_with('.') && !files.contains(&name) && !retired.contains(&name)
        })
        .map(|entry| entry.path())
        .collect();
    unknown.sort();
    unknown
}

fn file_names(names: &[&str]) -> Vec<String> {
    names
        .iter()
        .map(|name| crate::shim_registry::file_name(name))
        .collect()
}

/// Every finding for this machine.
pub fn inspect(exe: &Path, own: &str, home: Option<&Path>, path_env: &str) -> DirReport {
    use crate::shim_registry::{file_names_in, ShimDir};
    let scanners = file_names(&[
        crate::multicall::CMD_SCAN,
        crate::multicall::LEGACY_CMD_SCAN,
    ]);
    let retired = file_names(crate::shim_install::RETIRED_ALIASES);
    let mut report = DirReport::default();
    let mut add = |part: DirReport| {
        report.findings.extend(part.findings);
        report.unknown.extend(part.unknown);
    };
    if let Some(home) = home {
        let dirs = [
            (
                crate::shim_registry::INTERPRETER_SUBDIR,
                file_names_in(ShimDir::Interpreter),
            ),
            (
                crate::shim_registry::SESSION_SUBDIR,
                file_names_in(ShimDir::Session),
            ),
            (".clud/state/helper-bin", scanners.clone()),
        ];
        for (subdir, files) in dirs {
            let dir = home.join(subdir);
            let part = inspect_dir(&dir, &files, &retired, own, true);
            let flagged: Vec<PathBuf> = part
                .findings
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect();
            add(part);
            add(DirReport {
                findings: wrong_builds(home, exe, &dir, &files, &flagged),
                unknown: Vec::new(),
            });
        }
    }
    if let Some(beside) = exe.parent() {
        // A bin dir shared with real programs: never missing, never unknown.
        let mut files = scanners;
        files.push(crate::shim_registry::file_name(crate::multicall::SHIM));
        add(inspect_dir(beside, &files, &[], own, false));
    }
    let scanner = crate::shim_registry::file_name(crate::multicall::CMD_SCAN);
    if let Some(first) = crate::shim_resolve::which(&scanner, path_env) {
        if let Some(found) = fingerprint(&first).filter(|found| found != own) {
            if !report.findings.iter().any(|f| f.path() == first) {
                report
                    .findings
                    .push(Finding::ForeignScanner { path: first, found });
            }
        }
    }
    report
}

/// One [`Finding::WrongBuild`] per present name in `dir` whose toolstore
/// build is not the launcher's. Names already flagged are skipped. This is
/// the exact test size/version fingerprints cannot make (#2558).
fn wrong_builds(
    home: &Path,
    exe: &Path,
    dir: &Path,
    files: &[String],
    flagged: &[PathBuf],
) -> Vec<Finding> {
    let Ok(launcher) = crate::toolstore::build_id(home, exe) else {
        return Vec::new();
    };
    files
        .iter()
        .map(|file| dir.join(file))
        .filter(|path| !flagged.contains(path) && std::fs::symlink_metadata(path).is_ok())
        .filter_map(|path| {
            let found = crate::toolstore::resolved_build(home, &path)
                .unwrap_or_else(|| "unversioned (not in the toolstore)".to_string());
            (found != launcher).then(|| Finding::WrongBuild {
                path,
                found,
                launcher: launcher.clone(),
            })
        })
        .collect()
}

/// The tool names a hook harness may report for a guarded tool besides its
/// own: Codex reaches `apply_patch` through `Edit`/`Write`, `spawn_agent`
/// through `Agent`, and its shells through `Bash`.
fn matcher_aliases(tool: &str) -> &'static [&'static str] {
    match tool {
        "apply_patch" => &["Edit", "Write"],
        "spawn_agent" => &["Agent"],
        "exec_command" | "shell_command" => &["Bash"],
        _ => &[],
    }
}

/// Whether a PreToolUse `matcher` reaches `tool`: empty or `*` reaches every
/// tool, anything else is an anchored regex over the name or an alias.
pub fn matcher_reaches(matcher: &str, tool: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let Ok(regex) = Regex::new(&format!("^(?:{matcher})$")) else {
        return false;
    };
    regex.is_match(tool) || matcher_aliases(tool).iter().any(|a| regex.is_match(a))
}

/// Whether a hook command runs clud's scanner.
fn runs_scanner(command: &str) -> bool {
    ["clud-cmd-scan", "clud-block-bad-cmd", "__cmd-scan"]
        .iter()
        .any(|name| command.contains(name))
}

/// The PreToolUse matchers in a hook document whose entry runs the scanner.
pub fn scanner_matchers(document: &Value) -> Vec<String> {
    let Some(entries) = document["hooks"]["PreToolUse"].as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|hook| hook["command"].as_str().is_some_and(runs_scanner))
            })
        })
        .map(|entry| entry["matcher"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Guarded tools none of `matchers` reaches.
pub fn uncovered(matchers: &[String]) -> Vec<&'static str> {
    GUARDED_TOOLS
        .iter()
        .copied()
        .filter(|tool| !matchers.iter().any(|m| matcher_reaches(m, tool)))
        .collect()
}

/// One hook config's scanner coverage.
#[derive(Debug, PartialEq, Eq)]
pub struct Coverage {
    pub source: String,
    pub matchers: Vec<String>,
    pub uncovered: Vec<&'static str>,
}

fn coverage(source: String, matchers: Vec<String>) -> Coverage {
    let uncovered = uncovered(&matchers);
    Coverage {
        source,
        matchers,
        uncovered,
    }
}

/// Scanner coverage of the launch fragment and of every hook config on
/// disk that registers the scanner. A config without a scanner entry is
/// left out: it neither adds nor removes coverage.
pub fn coverage_report(home: Option<&Path>, repo: Option<&Path>) -> Vec<Coverage> {
    let fragment = crate::clud_hooks_compile::deletion_safety_fragment();
    let mut report = vec![coverage(
        "clud launch --settings fragment".to_string(),
        scanner_matchers(&fragment),
    )];
    let mut files = Vec::new();
    for root in [home, repo].into_iter().flatten() {
        files.push(root.join(".claude/settings.json"));
        files.push(root.join(".codex/hooks.json"));
    }
    for file in files {
        let Some(document) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            continue;
        };
        let matchers = scanner_matchers(&document);
        if !matchers.is_empty() {
            report.push(coverage(file.display().to_string(), matchers));
        }
    }
    report
}

fn json_report(own: &str, exe: &Path, aliases: &DirReport, coverage: &[Coverage]) -> Value {
    json!({
        "exe": exe.display().to_string(),
        "build": own,
        "guarded_tools": GUARDED_TOOLS,
        "findings": aliases.findings.iter().map(|f| json!({
            "kind": f.kind(),
            "path": f.path().display().to_string(),
            "message": f.to_string(),
        })).collect::<Vec<_>>(),
        "unknown": aliases.unknown.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "coverage": coverage.iter().map(|c| json!({
            "source": c.source,
            "matchers": c.matchers,
            "uncovered": c.uncovered,
        })).collect::<Vec<_>>(),
    })
}

fn print_report(own: &str, exe: &Path, aliases: &DirReport, coverage: &[Coverage]) {
    println!("clud {} (build {own})", exe.display());
    for entry in coverage {
        let gaps = if entry.uncovered.is_empty() {
            "every guarded tool".to_string()
        } else {
            format!("misses {}", entry.uncovered.join(", "))
        };
        println!("hook coverage: {}: {gaps}", entry.source);
    }
    for path in &aliases.unknown {
        println!(
            "note: {} is not a clud alias; left in place",
            path.display()
        );
    }
    if aliases.findings.is_empty() {
        println!("aliases: all current");
        return;
    }
    for finding in &aliases.findings {
        println!("problem: {finding}");
    }
    println!("a normal `clud` launch refreshes clud's own aliases and removes retired ones");
}

/// The dirs under `home` that hold only clud's aliases.
fn owned_alias_dirs(home: &Path) -> [PathBuf; 3] {
    [
        home.join(crate::shim_registry::INTERPRETER_SUBDIR),
        home.join(crate::shim_registry::SESSION_SUBDIR),
        home.join(".clud/state/helper-bin"),
    ]
}

/// `clud doctor --fix`: replace each stale or missing alias with the running
/// `exe`. A foreign scanner is replaced only inside one of clud's own alias
/// dirs; anywhere else it belongs to the user and is left alone. Retired
/// aliases are left to the launch, which already removes them. Returns one
/// line per action.
pub fn repair(exe: &Path, report: &DirReport, home: Option<&Path>) -> Vec<String> {
    let owned = home.map(owned_alias_dirs).unwrap_or_default();
    let in_owned_dir = |path: &Path| {
        path.parent()
            .is_some_and(|dir| owned.iter().any(|own| same_dir(dir, own)))
    };
    let mut actions = Vec::new();
    for finding in &report.findings {
        let path = finding.path();
        let replace = match finding {
            Finding::Stale { .. } | Finding::Missing { .. } | Finding::WrongBuild { .. } => true,
            Finding::ForeignScanner { .. } => in_owned_dir(path),
            Finding::Retired { .. } => continue,
        };
        if !replace {
            actions.push(format!(
                "left alone: {} is not in a clud alias dir; remove it or reorder PATH yourself",
                path.display()
            ));
            continue;
        }
        let placed = match home.filter(|_| in_owned_dir(path)) {
            Some(home) => crate::toolstore::install_owned(home, exe, path),
            None => crate::alias_link::install_alias(exe, path),
        };
        actions.push(match placed {
            Ok(_) => format!("fixed: {} now runs {}", path.display(), exe.display()),
            Err(error) => format!("could not fix {}: {error}", path.display()),
        });
    }
    actions
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// `clud doctor`: print the report; exit 1 when an alias is wrong. With
/// `fix`, repair first and report what remains.
pub fn run(as_json: bool, fix: bool) -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("clud doctor: cannot resolve the running executable");
        return 2;
    };
    let Some(own) = fingerprint(&exe) else {
        eprintln!("clud doctor: cannot stat {}", exe.display());
        return 2;
    };
    let home = crate::home::user_home();
    let path_env = std::env::var("PATH").unwrap_or_default();
    let mut aliases = inspect(&exe, &own, home.as_deref(), &path_env);
    if fix && !aliases.findings.is_empty() {
        for action in repair(&exe, &aliases, home.as_deref()) {
            eprintln!("{action}");
        }
        aliases = inspect(&exe, &own, home.as_deref(), &path_env);
    }
    let repo = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::block_bad_cmd::nearest_repo_root_public(&cwd));
    let coverage = coverage_report(home.as_deref(), repo.as_deref());
    if as_json {
        println!("{}", json_report(&own, &exe, &aliases, &coverage));
    } else {
        print_report(&own, &exe, &aliases, &coverage);
    }
    i32::from(!aliases.findings.is_empty())
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;

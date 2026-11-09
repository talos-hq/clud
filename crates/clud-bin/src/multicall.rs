//! argv[0] dispatch: `clud` is the only executable clud ships (#1551).
//!
//! Every helper name (`clud-cmd-scan`, `clud-shim`, `python`, `rm`, `gh`, ...)
//! is a hardlink, symlink or copy of `clud` (see [`crate::alias_link`]), and
//! [`maybe_run`] picks the function its name selects. `main` calls it before
//! clap, tracing, the runtime or any other startup work, so a hook or shim
//! invocation costs no more than the old dedicated binaries did.
//!
//! Dispatch keys on argv[0], never `current_exe()`: on Linux `current_exe()`
//! resolves a symlink to `clud`, and on Windows the CRT hands argv[0] over as
//! the caller typed it (any case, with or without `.exe`, either separator).
//!
//! Two hidden argv[1] forms reach the same entry points without an alias, for
//! hook configs and smoke tests that must not depend on argv[0]:
//! `clud __cmd-scan [args]` and `clud __shim <name> [args]`. A third,
//! `clud __link-aliases <dir>`, materializes every alias beside a build
//! output (test harnesses, packaging smoke tests).

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::shim_registry;

/// The scanner personality (`block_bad_cmd::run`).
pub const CMD_SCAN: &str = "clud-cmd-scan";
/// The pre-#532 spelling of [`CMD_SCAN`]; still answered so a hook config the
/// rollout has not rewritten yet keeps working.
pub const LEGACY_CMD_SCAN: &str = "clud-block-bad-cmd";
/// The `--registry` / passthrough personality's own name.
pub const SHIM: &str = "clud-shim";

/// `clud __cmd-scan`.
pub const CMD_SCAN_SUBCOMMAND: &str = "__cmd-scan";
/// `clud __shim <name>`.
pub const SHIM_SUBCOMMAND: &str = "__shim";
/// `clud __link-aliases <dir>`.
pub const LINK_ALIASES_SUBCOMMAND: &str = "__link-aliases";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Personality {
    CmdScan,
    Shim,
    Secret,
    Askpass,
    Installer,
}

/// Every name `clud` answers to besides its own, in materialization order.
pub fn alias_names() -> Vec<String> {
    let mut names = vec![
        CMD_SCAN.to_string(),
        LEGACY_CMD_SCAN.to_string(),
        SHIM.to_string(),
        "clud-secret".to_string(),
        "clud-askpass".to_string(),
    ];
    for spec in shim_registry::SHIMS {
        if !names.iter().any(|name| name == spec.name) {
            names.push(spec.name.to_string());
        }
    }
    names
}

/// The personality argv[0] selects, or `None` for the normal `clud` CLI.
/// Case-insensitive, `.exe`-insensitive and separator-agnostic.
pub fn personality_for(argv0: &OsStr) -> Option<Personality> {
    let name = shim_registry::invoked_name(argv0)?.to_ascii_lowercase();
    personality_for_name(&name)
}

fn personality_for_name(name: &str) -> Option<Personality> {
    let stem = name
        .strip_suffix(')')
        .and_then(|name| {
            let (stem, number) = name.rsplit_once(" (")?;
            (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())).then_some(stem)
        })
        .unwrap_or(name);
    if matches!(stem, "clud-installer" | "clud-install" | "clud-setup") {
        Some(Personality::Installer)
    } else if name == "clud-secret" {
        Some(Personality::Secret)
    } else if name == "clud-askpass" {
        Some(Personality::Askpass)
    } else if name == CMD_SCAN || name == LEGACY_CMD_SCAN {
        Some(Personality::CmdScan)
    } else if name == SHIM || shim_registry::SHIMS.iter().any(|spec| spec.name == name) {
        Some(Personality::Shim)
    } else {
        None
    }
}

/// Run the personality argv selects and return its exit code; `None` means
/// this is a normal `clud` invocation.
pub fn maybe_run(argv: &[OsString]) -> Option<i32> {
    let argv0 = argv.first()?;
    if let Some(personality) = personality_for(argv0) {
        return Some(run_personality(personality, argv));
    }
    match argv.get(1).and_then(|arg| arg.to_str()) {
        Some(CMD_SCAN_SUBCOMMAND) => Some(run_cmd_scan(&argv[1..])),
        Some(SHIM_SUBCOMMAND) => {
            let Some(name) = argv.get(2) else {
                eprintln!("usage: clud {SHIM_SUBCOMMAND} <alias> [args...]");
                return Some(2);
            };
            // The alias name stands in for argv[0], exactly as if invoked so.
            let mut shim_argv = vec![name.clone()];
            shim_argv.extend(argv[3..].iter().cloned());
            Some(crate::shim_main::run(&shim_argv))
        }
        Some(LINK_ALIASES_SUBCOMMAND) => Some(link_aliases(argv.get(2))),
        _ => None,
    }
}

fn run_personality(personality: Personality, argv: &[OsString]) -> i32 {
    match personality {
        Personality::Installer => {
            let mut installer_argv = argv.to_vec();
            if !installer_argv
                .iter()
                .skip(1)
                .any(|arg| arg == "--installer")
            {
                installer_argv.insert(1, "--installer".into());
            }
            let args = crate::args::Args::parse_from_raw(
                installer_argv
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
            );
            crate::self_install::entry::run_explicit(&args).unwrap_or(2)
        }
        Personality::CmdScan => run_cmd_scan(argv),
        Personality::Secret => crate::secret_prompt::run_alias(argv),
        Personality::Askpass => crate::secret_prompt::run_askpass(argv),
        Personality::Shim => crate::shim_main::run(argv),
    }
}

/// `args[0]` is skipped exactly like argv[0] is. A run whose build differs
/// from the session's launcher relays to it first (#1954).
fn run_cmd_scan(args: &[OsString]) -> i32 {
    use crate::hook_handshake as handshake;
    let decision = handshake::decide(&handshake::probe());
    if let handshake::Decision::Relay {
        launcher,
        own,
        expected,
    } = decision
    {
        crate::block_bad_cmd::log_hook_skew(&launcher, &own, &expected);
        if let Some(code) = handshake::relay(&launcher, &args[1..]) {
            return code;
        }
    }
    crate::block_bad_cmd::run_with_args(
        args.iter()
            .skip(1)
            .map(|arg| arg.to_string_lossy().into_owned()),
    )
}

fn link_aliases(dir: Option<&OsString>) -> i32 {
    let Some(dir) = dir else {
        eprintln!("usage: clud {LINK_ALIASES_SUBCOMMAND} <dir>");
        return 2;
    };
    let source = match std::env::current_exe() {
        Ok(source) => source,
        Err(error) => {
            eprintln!("clud: cannot resolve the running executable: {error}");
            return 1;
        }
    };
    let dir = Path::new(dir);
    if let Err(error) = std::fs::create_dir_all(dir) {
        eprintln!("clud: cannot create {}: {error}", dir.display());
        return 1;
    }
    for name in alias_names() {
        let target = dir.join(shim_registry::file_name(&name));
        if let Err(error) = crate::alias_link::install_alias(&source, &target) {
            eprintln!("clud: cannot link {}: {error}", target.display());
            return 1;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_download_names_dispatch() {
        for name in ["clud-installer", "clud-install", "clud-setup"] {
            for case in [name.to_string(), name.to_ascii_uppercase()] {
                for suffix in ["", ".exe", " (1)", " (12).EXE"] {
                    for prefix in ["", "/downloads/", "C:\\Downloads\\"] {
                        let argv0 = format!("{prefix}{case}{suffix}");
                        assert_eq!(personality(&argv0), Some(Personality::Installer), "{argv0}");
                    }
                }
            }
        }
        for name in [
            "clud",
            "clud.exe",
            "install",
            "setup",
            "clud-setup-old",
            "clud-installer.txt",
            "clud-setup ()",
            "clud-setup (x).exe",
            "clud-setup (1).exe.bak",
            "/clud-setup/clud",
            "reinstall-clud",
        ] {
            assert_eq!(personality(name), None, "{name}");
        }
    }

    fn personality(argv0: &str) -> Option<Personality> {
        personality_for(OsStr::new(argv0))
    }

    /// Every alias name, in every spelling a caller can produce.
    #[test]
    fn every_alias_dispatches_in_every_spelling() {
        for name in alias_names() {
            let expected = if name == CMD_SCAN || name == LEGACY_CMD_SCAN {
                Personality::CmdScan
            } else if name == "clud-secret" {
                Personality::Secret
            } else if name == "clud-askpass" {
                Personality::Askpass
            } else {
                Personality::Shim
            };
            let upper = name.to_ascii_uppercase();
            let mixed: String = name
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect();
            for spelling in [&name, &upper, &mixed] {
                for full in [
                    spelling.to_string(),
                    format!("{spelling}.exe"),
                    format!("{spelling}.EXE"),
                    format!("/home/u/.clud/state/bin/{spelling}"),
                    format!(r"C:\Users\u\.clud\state\bin\{spelling}.exe"),
                    format!(r"C:/Users/u/.clud/state/bin/{spelling}.Exe"),
                ] {
                    assert_eq!(personality(&full), Some(expected), "{full}");
                }
            }
        }
    }

    #[test]
    fn clud_itself_and_lookalikes_are_not_aliases() {
        for name in [
            "clud",
            "clud.exe",
            "CLUD.EXE",
            "clud-cmd-scan-x",
            "xrm",
            "rm2",
            "pip",
            "",
        ] {
            assert_eq!(personality(name), None, "{name:?}");
        }
    }

    #[test]
    fn a_normal_invocation_is_not_dispatched() {
        let argv: Vec<OsString> = ["clud", "--version"].map(OsString::from).to_vec();
        assert_eq!(maybe_run(&argv), None);
        assert_eq!(maybe_run(&[]), None);
    }

    #[test]
    fn hidden_subcommands_need_their_arguments() {
        let shim: Vec<OsString> = ["clud", SHIM_SUBCOMMAND].map(OsString::from).to_vec();
        assert_eq!(maybe_run(&shim), Some(2));
        let link: Vec<OsString> = ["clud", LINK_ALIASES_SUBCOMMAND]
            .map(OsString::from)
            .to_vec();
        assert_eq!(maybe_run(&link), Some(2));
    }

    #[test]
    fn alias_names_have_no_duplicates() {
        let names = alias_names();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len());
        assert!(names.iter().any(|name| name == CMD_SCAN));
        assert!(names.iter().any(|name| name == "rm"));
    }
}

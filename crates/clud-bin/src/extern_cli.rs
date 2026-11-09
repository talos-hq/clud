//! `clud extern trust` — the trust allowlist for foreign checkouts' hooks
//! (zackees/clud#967 Phase 4, #966 D9).
//!
//! An `extern` checkout's own hooks stay off until this command records the
//! allow in the parent's gitignored `.clud/settings.local.json`, keyed by the
//! checkout's name **and** its origin URL. Everything here is self-contained:
//! no backend, no daemon, no settings beyond the one gitignored file.

use std::path::Path;

use crate::hook_trust::{self, TrustStore};

/// Run `clud extern trust <name>` / `--list` / `--revoke`. Returns the
/// process exit code.
pub fn run(subcommand: Option<&crate::args::ExternSubcommand>) -> i32 {
    match subcommand {
        None => {
            eprintln!("clud extern: missing subcommand. Try `clud extern trust --help`.");
            2
        }
        Some(crate::args::ExternSubcommand::Trust(options)) => {
            let crate::args::ExternTrustArgs { name, list, revoke } = options.as_ref();
            run_trust(name.as_deref(), *list, *revoke)
        }
    }
}

fn run_trust(name: Option<&str>, list: bool, revoke: bool) -> i32 {
    let Some(parent) = parent_root() else {
        eprintln!("[clud] error: not inside a git repo; run `clud extern trust` from the repo whose checkouts you mean");
        return 1;
    };
    let store = hook_trust::load(&parent);

    if list {
        print_list(&store, name);
        return 0;
    }

    let Some(name) = name else {
        eprintln!(
            "clud extern trust: a checkout name is required (or use `--list`); try `clud extern trust --help`"
        );
        return 2;
    };
    if !hook_trust::valid_name(name) {
        eprintln!(
            "[clud] error: {name:?} is not a usable checkout name (must be a bare directory name)"
        );
        return 1;
    }

    if revoke {
        return match hook_trust::revoke(&parent, name) {
            Ok(true) => {
                println!("[clud] removed trust for extern checkout {name:?}");
                0
            }
            Ok(false) => {
                eprintln!("[clud] no trust entry for extern checkout {name:?} was recorded");
                1
            }
            Err(error) => {
                eprintln!("[clud] error: {error}");
                1
            }
        };
    }

    // Trusting means naming a checkout that exists: the entry is the
    // name+origin pair, and the origin is read from the checkout itself.
    let Some(dir) = hook_trust::extern_dir_for(&parent, name) else {
        eprintln!(
            "[clud] error: no extern checkout named {name:?} under {}",
            extern_roots_display(&parent)
        );
        return 1;
    };
    let Some(origin) = hook_trust::origin_of(&dir) else {
        eprintln!(
            "[clud] error: {} has no git `origin` remote; trust is keyed to the checkout's \
             name and origin URL, so a checkout without one cannot be trusted",
            dir.display()
        );
        return 1;
    };
    match hook_trust::record(&parent, name, &origin) {
        Ok(()) => {
            println!(
                "[clud] trusted extern checkout {name:?} (origin {origin}); its hooks now run \
                 for its files, rooted at the checkout. Recorded in {}",
                hook_trust::trust_file(&parent).display()
            );
            0
        }
        Err(error) => {
            eprintln!("[clud] error: {error}");
            1
        }
    }
}

fn parent_root() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    crate::block_bad_cmd::nearest_repo_root_public(&cwd)
}

fn extern_roots_display(parent: &Path) -> String {
    crate::extern_root::known_roots(parent)
        .into_iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_list(store: &TrustStore, filter: Option<&str>) {
    let entries = store
        .extern_entries
        .iter()
        .filter(|entry| filter.is_none_or(|name| entry.name == name));
    let mut any = false;
    for entry in entries {
        println!("{}\t{}", entry.name, entry.origin);
        any = true;
    }
    if !any {
        match filter {
            Some(name) => println!("(no trust entries recorded for {name:?})"),
            None => println!("(no trust entries recorded; `clud extern trust <name>` adds one)"),
        }
    }
}

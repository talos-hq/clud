//! Working on clud inside clud's own repo.
//!
//! `bash build` replaces `target/debug/clud` (the source of every shim alias), after which the command
//! hook's rm identity check stops matching the installed rm shim and denies
//! every shell call, including the ones that would repair it. Inside this
//! repo clud therefore sets [`SKIP_RM_IDENTITY_ENV`] for the session (the
//! hook inherits it) and says so in yellow. Every other hook check stays on.

use std::path::Path;

/// Set to `1` in the hook's environment to skip only the rm identity check.
/// Read from the environment, never from command text, so a capped agent
/// cannot set it for itself.
pub const SKIP_RM_IDENTITY_ENV: &str = "CLUD_SKIP_RM_IDENTITY";

/// Whether `root` is a checkout of clud itself.
pub fn is_clud_repo(root: &Path) -> bool {
    let manifest = root.join("crates/clud-bin/Cargo.toml");
    std::fs::read_to_string(manifest)
        .is_ok_and(|text| text.lines().any(|line| line.trim() == "name = \"clud\""))
}

pub fn skip_rm_identity_enabled() -> bool {
    std::env::var(SKIP_RM_IDENTITY_ENV).as_deref() == Ok("1")
}

/// The startup warning, painted yellow.
pub fn warning() -> String {
    format!(
        "\x1b[33m[clud] {SKIP_RM_IDENTITY_ENV}=1: rm shim identity check disabled \
         automatically for doing work in the clud repo\x1b[0m"
    )
}

/// Turn the skip on for this process and its children when `root` is the
/// clud repo, and warn. An explicit value already in the environment wins.
pub fn apply(root: &Path) {
    if std::env::var_os(SKIP_RM_IDENTITY_ENV).is_some() || !is_clud_repo(root) {
        return;
    }
    std::env::set_var(SKIP_RM_IDENTITY_ENV, "1");
    eprintln!("{}", warning());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_clud_repo_by_its_crate_manifest() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        assert!(!is_clud_repo(tmp.path()));
        let dir = tmp.path().join("crates/clud-bin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"other\"\n").unwrap();
        assert!(!is_clud_repo(tmp.path()));
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"clud\"\n").unwrap();
        assert!(is_clud_repo(tmp.path()));
    }

    /// Searches up from the working directory, not from the compile-time
    /// `CARGO_MANIFEST_DIR`: CI builds on Linux and runs the Windows bundle
    /// from a checkout at a different path.
    #[test]
    fn this_checkout_is_detected() {
        let cwd = std::env::current_dir().unwrap();
        assert!(
            cwd.ancestors().any(is_clud_repo),
            "no clud checkout above {}",
            cwd.display()
        );
    }

    #[test]
    fn warning_is_yellow_and_names_the_switch() {
        let w = warning();
        assert!(w.starts_with("\x1b[33m") && w.ends_with("\x1b[0m"));
        assert!(w.contains("CLUD_SKIP_RM_IDENTITY=1"));
        assert!(w.contains("clud repo"));
    }
}

//! What the installer's consent screen says about a plan: a short summary in
//! plain words, and the exact plan behind **Show details**.
//!
//! The summary names the install location, the one directory added to PATH
//! and what it replaces; the title carries the version. Digests, sources, the exact startup-file
//! or User Path edit and the prompt engine stay one keystroke away, and an
//! unattended `--yes` install still prints the full plan
//! ([`InstallPlan::description`]).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::selector::{Block, Tone};

use super::activation::PathChange;
use super::catalog::compare_versions;
use super::transaction::InstallPlan;

/// Where a path is shown relative to: the home directory on POSIX, where `~`
/// is the familiar spelling. Windows paths stay absolute; `%USERPROFILE%`
/// is no clearer to a reader than the path itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathStyle {
    pub home: Option<PathBuf>,
}

impl PathStyle {
    pub fn detect() -> Self {
        Self {
            home: if crate::platform::host::WINDOWS {
                None
            } else {
                crate::home::user_home()
            },
        }
    }

    pub fn show(&self, path: &Path) -> String {
        match self
            .home
            .as_ref()
            .and_then(|home| path.strip_prefix(home).ok())
        {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        }
    }
}

/// The plan facts a consent screen shows, separated from [`InstallPlan`]'s
/// open source file so screens can be rendered in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub version: String,
    pub destination: PathBuf,
    /// The replaced executable's version, when one is installed there.
    pub existing_version: Option<String>,
    pub replaces: bool,
    /// `None` for `--no-modify-path`.
    pub path: Option<PathChange>,
    pub source: String,
    pub sha256: String,
    pub path_edit: String,
    pub prompt_engine: String,
}

impl Facts {
    pub fn of(plan: &InstallPlan) -> Self {
        Self {
            version: plan.version.clone(),
            destination: plan.destination.clone(),
            existing_version: plan.existing_version.clone(),
            replaces: plan.replacement.is_some(),
            path: plan.activation.as_ref().map(|plan| plan.path_change()),
            source: plan.source_label.clone(),
            sha256: plan.source_sha256.clone(),
            path_edit: plan.path_proposal.clone(),
            prompt_engine: crate::managed_lokr::preview(),
        }
    }

    pub fn title(&self) -> String {
        format!("Install clud {}", self.version)
    }

    /// The sentence under the title: what the install means for the user.
    pub fn subtitle(&self) -> &'static str {
        match self.path {
            Some(PathChange::Add { .. } | PathChange::Unchanged { .. }) => {
                "After installation, open a new terminal and type clud."
            }
            Some(PathChange::Manual { .. }) => "One manual step remains afterwards; see PATH.",
            None => "PATH is left unchanged (--no-modify-path).",
        }
    }

    /// The fields every consent screen shows. The version is in the title.
    pub fn summary(&self, paths: &PathStyle) -> Vec<Block> {
        vec![
            Block::path("Install to", paths.show(&self.destination)),
            path_field(self.path.as_ref(), paths),
            Block::field("Replaces", self.replaces_text()),
        ]
    }

    fn replaces_text(&self) -> String {
        let Some(existing) = &self.existing_version else {
            return if self.replaces {
                "the clud already installed there".into()
            } else {
                "nothing (new install)".into()
            };
        };
        let relation = match compare_versions(existing, &self.version) {
            Some(Ordering::Less) => "upgrade",
            Some(Ordering::Equal) => "same version",
            Some(Ordering::Greater) => "newer than this one",
            None => "existing install",
        };
        format!("clud {existing} ({relation})")
    }

    /// The exact plan, printed above the screen on request.
    pub fn details(&self) -> Vec<Block> {
        vec![
            Block::Text(format!("Details for clud {}", self.version), Tone::Strong),
            Block::below("Source", self.source.as_str()),
            Block::below("SHA-256", self.sha256.as_str()),
            Block::below("Install path", self.destination.display().to_string()),
            Block::below("PATH change", self.path_edit.as_str()),
            Block::below("Also installs", self.prompt_engine.as_str()),
        ]
    }
}

/// The PATH field, in plain words: the one directory a new terminal gains.
pub fn path_field(change: Option<&PathChange>, paths: &PathStyle) -> Block {
    match change {
        None => Block::field("PATH", "not changed (--no-modify-path)"),
        Some(PathChange::Unchanged { directory }) => Block::field(
            "PATH",
            format!("already includes {}", paths.show(directory)),
        ),
        Some(PathChange::Add { directory, files }) if files.is_empty() => {
            Block::path("Adds to PATH", paths.show(directory))
        }
        Some(PathChange::Add { directory, files }) => {
            let files: Vec<String> = files.iter().map(|file| paths.show(file)).collect();
            Block::field(
                "Adds to PATH",
                format!("{}, in {}", paths.show(directory), files.join(" and ")),
            )
        }
        Some(PathChange::Manual { directory }) => Block::field(
            "PATH",
            format!(
                "add {} yourself; your shell is not set up automatically",
                paths.show(directory)
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(path: &str) -> PathStyle {
        PathStyle {
            home: Some(PathBuf::from(path)),
        }
    }

    #[test]
    fn paths_under_home_use_tilde() {
        let paths = home("/home/me");
        assert_eq!(
            paths.show(Path::new("/home/me/.local/bin/clud")),
            "~/.local/bin/clud"
        );
        assert_eq!(paths.show(Path::new("/opt/clud")), "/opt/clud");
        assert_eq!(paths.show(Path::new("/home/me")), "~");
        assert_eq!(
            PathStyle::default().show(Path::new("/home/me/x")),
            "/home/me/x"
        );
    }

    fn value(block: &Block) -> (&str, &str) {
        match block {
            Block::Field { key, value, .. } => (key, value),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn path_field_names_the_directory_and_the_files_in_plain_words() {
        let paths = home("/home/me");
        let directory = PathBuf::from("/home/me/.local/bin");
        let add = |files: Vec<PathBuf>| PathChange::Add {
            directory: directory.clone(),
            files,
        };
        let both = add(vec!["/home/me/.bashrc".into(), "/home/me/.profile".into()]);
        assert_eq!(
            value(&path_field(Some(&both), &paths)),
            ("Adds to PATH", "~/.local/bin, in ~/.bashrc and ~/.profile")
        );
        assert_eq!(
            value(&path_field(Some(&add(Vec::new())), &paths)),
            ("Adds to PATH", "~/.local/bin")
        );
        let unchanged = PathChange::Unchanged {
            directory: directory.clone(),
        };
        assert_eq!(
            value(&path_field(Some(&unchanged), &paths)),
            ("PATH", "already includes ~/.local/bin")
        );
        let manual = PathChange::Manual { directory };
        assert!(value(&path_field(Some(&manual), &paths))
            .1
            .contains("yourself"));
        assert!(value(&path_field(None, &paths))
            .1
            .contains("--no-modify-path"));
    }

    #[test]
    fn replaces_says_upgrade_reinstall_or_downgrade() {
        let mut facts = Facts {
            version: "2.8.65".into(),
            destination: "/home/me/.local/bin/clud".into(),
            existing_version: None,
            replaces: false,
            path: None,
            source: String::new(),
            sha256: String::new(),
            path_edit: String::new(),
            prompt_engine: String::new(),
        };
        assert_eq!(facts.replaces_text(), "nothing (new install)");
        for (existing, expected) in [
            ("2.8.40", "clud 2.8.40 (upgrade)"),
            ("2.8.65", "clud 2.8.65 (same version)"),
            ("2.10.0", "clud 2.10.0 (newer than this one)"),
        ] {
            facts.existing_version = Some(existing.into());
            assert_eq!(facts.replaces_text(), expected);
        }
    }
}

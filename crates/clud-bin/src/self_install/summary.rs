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

    /// `--installer-plan-json`: the plan the consent screen shows, as one
    /// read-only JSON object (`clud-installer-plan/v1`).
    pub fn report(&self) -> serde_json::Value {
        let path = match &self.path {
            None => serde_json::json!({ "change": "not_modified" }),
            Some(PathChange::Unchanged { directory }) => {
                serde_json::json!({ "change": "unchanged", "directory": directory })
            }
            Some(PathChange::Add { directory, files }) => {
                serde_json::json!({ "change": "add", "directory": directory, "files": files })
            }
            Some(PathChange::Manual { directory }) => {
                serde_json::json!({ "change": "manual", "directory": directory })
            }
        };
        serde_json::json!({
            "schema": PLAN_SCHEMA,
            "version": self.version,
            "destination": self.destination,
            "existing_version": self.existing_version,
            "replaces": self.replaces,
            "replaces_text": self.replaces_text(),
            "path": path,
            "source": self.source,
            "sha256": self.sha256,
            "path_edit": self.path_edit,
            "prompt_engine": self.prompt_engine,
        })
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

/// Schema id of the `--installer-plan-json` object.
pub const PLAN_SCHEMA: &str = "clud-installer-plan/v1";

/// Screen blocks as plain lines (`key: value`), for a native dialog that
/// cannot draw the terminal layout.
pub fn plain_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            Block::Blank => String::new(),
            Block::Text(text, _) => text.clone(),
            Block::Field { key, value, .. } => format!("{key}: {value}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    fn plan_report_is_a_stable_read_only_schema() {
        let mut facts = Facts {
            version: "2.8.70".into(),
            destination: "/home/me/.local/bin/clud".into(),
            existing_version: Some("2.8.65".into()),
            replaces: true,
            path: Some(PathChange::Add {
                directory: "/home/me/.local/bin".into(),
                files: vec!["/home/me/.zprofile".into()],
            }),
            source: "running executable".into(),
            sha256: "ab".repeat(32),
            path_edit: "append to ~/.zprofile".into(),
            prompt_engine: "lokr 0.1.0".into(),
        };
        let report = facts.report();
        let keys: Vec<&str> = report
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "schema",
                "version",
                "destination",
                "existing_version",
                "replaces",
                "replaces_text",
                "path",
                "source",
                "sha256",
                "path_edit",
                "prompt_engine"
            ]
        );
        assert_eq!(report["schema"], PLAN_SCHEMA);
        assert_eq!(report["replaces_text"], "clud 2.8.65 (upgrade)");
        assert_eq!(report["path"]["change"], "add");
        assert_eq!(report["path"]["files"][0], "/home/me/.zprofile");
        facts.path = None;
        assert_eq!(facts.report()["path"]["change"], "not_modified");
        assert_eq!(
            plain_text(&facts.summary(&home("/home/me"))),
            "Install to: ~/.local/bin/clud\nPATH: not changed (--no-modify-path)\n\
             Replaces: clud 2.8.65 (upgrade)"
        );
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

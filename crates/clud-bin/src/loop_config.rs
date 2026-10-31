//! `loop.json`: the single typed schema for a `clud loop` run (#2471).
//!
//! A config comes from `--config <path>`, or implicitly from
//! `<git-root>/loop.json`. CLI flags override file values. The resolved
//! config is written to `<git-root>/.clud/loop/loop.json` so every run
//! leaves a record of exactly what it ran with. The committed JSON Schema
//! (`assets/loop.schema.json`) is kept in lockstep with [`LoopConfig`] by
//! `schema_properties_match_struct_fields`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::loop_spec::loop_dir;

/// The only schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Name of the implicit config file at the git root.
pub const FILE_NAME: &str = "loop.json";

/// JSON Schema for [`LoopConfig`], shipped for editors and validation.
pub const JSON_SCHEMA: &str = include_str!("../assets/loop.schema.json");

/// Typed `loop.json`. Every field except `schema_version` is optional so a
/// file can set only what it cares about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopConfig {
    pub schema_version: u32,
    /// Prompt text, path to a local file, or a GitHub issue/PR URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Iteration budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_count: Option<u32>,
    /// Custom DONE marker path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<String>,
    /// Disable the DONE/BLOCKED marker contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_done: Option<bool>,
    /// Re-run interval such as `30s`, `5m`, `1h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
    /// Force re-fetch of a cached GitHub issue/PR body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<bool>,
    /// Message queued for the first iteration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Why a `loop.json` could not be used. Invalid configs fail fast.
#[derive(Debug, thiserror::Error)]
pub enum LoopConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path}: unsupported schema_version {found} (this clud supports {SCHEMA_VERSION})")]
    Version { path: PathBuf, found: u32 },
    #[error("{path}: `done` and `no_done: true` conflict")]
    Conflict { path: PathBuf },
}

impl LoopConfig {
    /// Parse and validate `text` read from `path`.
    pub fn parse(path: &Path, text: &str) -> Result<Self, LoopConfigError> {
        let cfg: Self = serde_json::from_str(text).map_err(|source| LoopConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if cfg.schema_version != SCHEMA_VERSION {
            return Err(LoopConfigError::Version {
                path: path.to_path_buf(),
                found: cfg.schema_version,
            });
        }
        if cfg.done.is_some() && cfg.no_done == Some(true) {
            return Err(LoopConfigError::Conflict {
                path: path.to_path_buf(),
            });
        }
        Ok(cfg)
    }

    /// Load the explicit `--config` path, or `<git-root>/loop.json` when it
    /// exists. `Ok(None)` means no config is in play.
    pub fn discover(
        git_root: &Path,
        explicit: Option<&Path>,
    ) -> Result<Option<(PathBuf, Self)>, LoopConfigError> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => {
                let implicit = git_root.join(FILE_NAME);
                if !implicit.is_file() {
                    return Ok(None);
                }
                implicit
            }
        };
        let text = std::fs::read_to_string(&path).map_err(|source| LoopConfigError::Read {
            path: path.clone(),
            source,
        })?;
        Self::parse(&path, &text).map(|cfg| Some((path, cfg)))
    }

    /// Overlay `cli` on top of `self`: any value the CLI set wins.
    #[must_use]
    pub fn overridden_by(&self, cli: &Self) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            task: cli.task.clone().or_else(|| self.task.clone()),
            loop_count: cli.loop_count.or(self.loop_count),
            done: cli.done.clone().or_else(|| self.done.clone()),
            no_done: cli.no_done.or(self.no_done),
            repeat: cli.repeat.clone().or_else(|| self.repeat.clone()),
            refresh: cli.refresh.or(self.refresh),
            message: cli.message.clone().or_else(|| self.message.clone()),
        }
    }

    /// Where the resolved config for the current run is recorded.
    pub fn resolved_path(git_root: &Path) -> PathBuf {
        loop_dir(git_root).join(FILE_NAME)
    }

    /// Write this (resolved) config to `.clud/loop/loop.json`.
    pub fn write_resolved(&self, git_root: &Path) -> std::io::Result<PathBuf> {
        let path = Self::resolved_path(git_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        text.push('\n');
        std::fs::write(&path, text)?;
        Ok(path)
    }
}

#[cfg(test)]
#[path = "loop_config_tests.rs"]
mod tests;

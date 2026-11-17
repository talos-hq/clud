//! User-approved shell activation and fresh name-based install verification.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

use crate::platform::host;
use crate::platform::self_install as fs_host;
use crate::platform::self_install_activation as host_activation;

#[derive(Debug, Clone)]
pub struct ActivationPlan {
    pub destination: PathBuf,
    host: host_activation::ActivationHost,
}

/// What activation does to PATH, in the terms the consent screen shows. The
/// exact edit stays in [`ActivationPlan::description`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathChange {
    /// `directory` already leads PATH; nothing is written.
    Unchanged { directory: PathBuf },
    /// `directory` is added for new terminals. `files` are the startup files
    /// edited; empty when the change is the Windows user PATH.
    Add {
        directory: PathBuf,
        files: Vec<PathBuf>,
    },
    /// The login shell is not one the installer edits: the user adds
    /// `directory` themselves.
    Manual { directory: PathBuf },
}

pub fn plan(destination: &Path) -> Result<ActivationPlan, String> {
    Ok(ActivationPlan {
        destination: destination.to_path_buf(),
        host: host_activation::plan(destination)?,
    })
}

/// Commands are labeled because the account login shell need not be the caller.
pub fn terminal_commands(destination: &Path) -> Result<Vec<(&'static str, String)>, String> {
    host_activation::terminal_commands(destination)
}

impl ActivationPlan {
    pub fn description(&self) -> String {
        host_activation::description(&self.destination, &self.host)
    }

    pub fn path_change(&self) -> PathChange {
        host_activation::path_change(&self.destination, &self.host)
    }

    /// How the user picks up the new PATH in an already-open terminal.
    pub fn reload_hint(&self) -> String {
        host_activation::reload_hint(&self.host)
    }

    pub fn apply_and_verify(&self, version: &str) -> Result<(), String> {
        super::transaction::inspect_destination(&self.destination, false)?;
        let parent = self
            .destination
            .parent()
            .ok_or("missing install directory")?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        fs_host::restrict_lock_options(&mut options);
        let lock_path = parent.join(".clud-install.lock");
        if host::WINDOWS {
            if let Ok(meta) = std::fs::symlink_metadata(&lock_path) {
                if fs_host::is_reparse_point(&meta) {
                    return Err("installer lock is a reparse point".into());
                }
            }
        }
        let lock = options
            .open(&lock_path)
            .map_err(|error| format!("open installer lock: {error}"))?;
        let lock_meta = lock.metadata().map_err(|error| error.to_string())?;
        if !lock_meta.is_file() {
            return Err("installer lock is not a regular file".into());
        }
        if !fs_host::owned_exclusively(&lock_meta) {
            return Err("installer lock is not exclusively user-owned".into());
        }
        if fs_host::is_reparse_point(&lock_meta) {
            return Err("installer lock is a reparse point".into());
        }
        lock.lock_exclusive().map_err(|error| error.to_string())?;
        super::transaction::inspect_destination(&self.destination, false)?;
        host_activation::apply_and_verify(&self.destination, &self.host, version)
    }
}

//! Windows half of `crate::self_install::transaction`'s filesystem checks.
//! Windows has no POSIX owner/mode bits here; the installer instead rejects
//! reparse points and installs under the per-user `LOCALAPPDATA`.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::self_install::catalog::{Arch, GnuEligibility};

/// No-op: Windows lock files take the default share/create flags.
pub fn restrict_lock_options(_options: &mut OpenOptions) {}

/// True when the entry is a reparse point (`FILE_ATTRIBUTE_REPARSE_POINT`).
pub fn is_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_attributes() & 0x400 != 0
}

/// Always true: Windows has no uid/link-count check here.
pub fn owned_exclusively(_meta: &fs::Metadata) -> bool {
    true
}

/// Always true: Windows has no uid/mode check here.
pub fn user_controlled_dir(_meta: &fs::Metadata) -> bool {
    true
}

/// Windows fingerprints a file by length and mtime only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity;

pub fn file_identity(_meta: &fs::Metadata) -> FileIdentity {
    FileIdentity
}

pub fn create_install_dir(parent: &Path) -> Result<(), String> {
    fs::create_dir_all(parent).map_err(|error| format!("create user bin: {error}"))
}

pub fn approved_destination() -> Result<PathBuf, String> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| crate::home::user_home().map(|home| home.join("AppData/Local")))
        .ok_or("cannot locate per-user local app data")?;
    if !local.is_absolute() {
        return Err("per-user local app data must be absolute".into());
    }
    Ok(local.join("Programs/clud/bin/clud.exe"))
}

/// No-op: executability on Windows comes from the `.exe` extension.
pub fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Never verified: the GNU build is a Linux asset.
pub fn gnu_eligibility(_arch: Arch) -> GnuEligibility {
    GnuEligibility::Unverified
}

/// Windows launcher reparse points remain unsupported.
pub fn replaceable_launcher_link(_path: &Path, _meta: &fs::Metadata) -> bool {
    false
}

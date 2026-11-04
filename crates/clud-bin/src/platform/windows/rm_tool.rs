//! Windows half of `crate::rm_tool`: no uid and no device/inode identity,
//! so ownership checks and the creation ledger's identity match do not
//! apply; read-only attributes stand in for mode bits, and a junction or
//! other reparse point stands in for a mount.

use std::fs::{Metadata, OpenOptions, Permissions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Windows has no uid.
pub fn effective_uid() -> Option<u32> {
    None
}

/// Windows has no uid.
pub fn file_owner(_meta: &Metadata) -> Option<u32> {
    None
}

/// No device/inode identity is read on Windows.
pub fn file_identity(_meta: &Metadata) -> Option<(u64, u64)> {
    None
}

/// Windows has no mode bits: this only clears FILE_ATTRIBUTE_READONLY.
pub fn make_dir_permissions_writable(permissions: &mut Permissions) {
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
}

/// Clear a read-only file's FILE_ATTRIBUTE_READONLY.
pub fn make_file_writable(path: &Path, mut permissions: Permissions) {
    if permissions.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

/// Recreate the symlink `source` (pointing at `link`) at `dest`, as a
/// directory or file symlink to match what it points at.
pub fn copy_symlink(source: &Path, link: PathBuf, dest: &Path) -> std::io::Result<()> {
    if std::fs::metadata(source).is_ok_and(|m| m.is_dir()) {
        std::os::windows::fs::symlink_dir(link, dest)
    } else {
        std::os::windows::fs::symlink_file(link, dest)
    }
}

pub fn reject_mounts(target: &Path) -> Result<(), String> {
    if super::rm_guard::windows_reparse_tree(target)? {
        return Err("contains a junction or other reparse point".into());
    }
    Ok(())
}

/// No temp root counts as RAM-backed off Linux.
pub fn is_ram_backed(_: &Path) -> bool {
    false
}

/// A session grant carries no checkout identity on Windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantIdentity {}

/// No ownership check on Windows.
pub fn owned_by_caller(_root: &Path) -> bool {
    true
}

/// Nothing to record on Windows.
pub fn grant_identity(_root: &Path) -> Result<GrantIdentity, String> {
    Ok(GrantIdentity {})
}

/// Without an identity, a grant holds while its checkout is a directory.
pub fn same_grant_identity(root: &Path, _identity: &GrantIdentity) -> bool {
    root.is_dir()
}

/// Windows has no creation mode bits.
pub fn grant_record_mode(_options: &mut OpenOptions) {}

/// Windows has no mode bits; the test tree stays as created.
#[cfg(test)]
pub(crate) fn seal_dir_for_test(_dir: &Path) {}
/// Unix directory-mode repair has no Windows equivalent. Preserve the
/// original failed unlink instead of changing a file's shared attributes.
pub fn remove_owned_leaf_writable(
    _root: &std::path::Path,
    _path: &std::path::Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "directory-mode repair is Unix-only",
    ))
}

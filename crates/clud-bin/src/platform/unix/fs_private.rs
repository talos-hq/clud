//! Unix half of `crate::fs_private`: owner-only files are created with mode
//! `0600`; there is no separate ACL step.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;

/// Create the file readable and writable by its owner only.
pub fn owner_only_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

/// The creation mode already made `_path` owner-only.
pub fn protect_owner_only(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::fs_private::write_private_atomic;
    use std::fs;

    #[test]
    fn files_are_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("secret.json");
        write_private_atomic(&path, b"{}").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

//! Read credentials only from an owned, private, regular opened file.

use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

fn insecure() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "credential file is not owner-only",
    )
}

pub fn read(path: &Path) -> io::Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                insecure()
            } else {
                error
            }
        })?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(insecure());
    }
    let mut secret = String::new();
    file.read_to_string(&mut secret)?;
    Ok(secret)
}

pub fn create_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(insecure());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_file_and_dir_are_owner_only() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let folder = dir.path().join("credentials");
        create_dir(&folder).unwrap();
        let file = folder.join("key");
        crate::fs_private::write_private_atomic(&file, b"fixture-key").unwrap();
        assert_eq!(fs::metadata(&folder).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read(&file).unwrap(), "fixture-key");
    }

    #[test]
    fn a_group_readable_file_is_refused() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let file = dir.path().join("key");
        crate::fs_private::write_private_atomic(&file, b"fixture-key").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read(&file).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn a_symlinked_credential_file_is_refused() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let target = dir.path().join("target");
        let file = dir.path().join("key");
        crate::fs_private::write_private_atomic(&target, b"fixture-key").unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert_eq!(
            read(&file).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}

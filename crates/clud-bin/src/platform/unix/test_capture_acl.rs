//! macOS extended ACL checks on an already-open artifact.
//!
//! The caller must also verify directory ancestry and retain stable directory
//! handles; the macOS root path remains unsupported until that exists.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;

// Darwin's public filesec_property_t value from <sys/fcntl.h>.
const FILESEC_ACL: c_int = 5;

unsafe extern "C" {
    fn filesec_init() -> *mut c_void;
    fn filesec_free(filesec: *mut c_void);
    fn filesec_query_property(filesec: *mut c_void, property: c_int, valid: *mut c_int) -> c_int;
    // x86_64 Darwin headers select the modern inode layout with this alias;
    // arm64 uses the unsuffixed symbol. See <sys/stat.h> __DARWIN_INODE64.
    #[cfg_attr(target_arch = "x86_64", link_name = "fstatx_np$INODE64")]
    fn fstatx_np(fd: c_int, stat: *mut libc::stat, filesec: *mut c_void) -> c_int;
}

struct FileSecurity(*mut c_void);

impl Drop for FileSecurity {
    fn drop(&mut self) {
        // SAFETY: the pointer came from filesec_init and is owned by this wrapper.
        unsafe { filesec_free(self.0) };
    }
}

/// Reject every present ACL and every failed security metadata query.
pub(super) fn verify_empty_acl(file: &File) -> io::Result<()> {
    // SAFETY: filesec_init takes no arguments and returns an owned object or null.
    let security = unsafe { filesec_init() };
    if security.is_null() {
        return Err(io::Error::last_os_error());
    }
    let security = FileSecurity(security);
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor and filesec are live; stat points to valid writable storage.
    if unsafe { fstatx_np(file.as_raw_fd(), stat.as_mut_ptr(), security.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut valid = 0;
    // SAFETY: filesec is live and valid points to writable c_int storage.
    if unsafe { filesec_query_property(security.0, FILESEC_ACL, &mut valid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Apple's filesec implementation exposes the populated ACL-valid flag;
    // copyfile treats the corresponding missing property after fstatx as no ACL.
    // Reject any present ACL rather than guessing which principals are safe.
    if valid != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "artifact has an extended ACL",
        ));
    }
    Ok(())
}

/// Open every component without following links and return the verified root.
/// The returned handle is intended to be held through relative file creation.
#[cfg(test)]
pub(super) fn open_private_root(path: &std::path::Path) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let mut components = path.components().peekable();
    if components.next() != Some(Component::RootDir) || components.peek().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact root must be absolute",
        ));
    }
    let slash = CString::new("/").expect("slash contains no NUL");
    // SAFETY: the string is terminated, flags request a directory and no link following.
    let fd = unsafe {
        libc::open(
            slash.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open returned a new owned descriptor.
    let mut directory = unsafe { File::from_raw_fd(fd) };
    verify_directory(&directory, false)?;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid artifact path component",
            ));
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: directory is live, name is terminated, and flags forbid symlinks.
        let child = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if child < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned descriptor.
        directory = unsafe { File::from_raw_fd(child) };
        verify_directory(&directory, components.peek().is_none())?;
    }
    Ok(directory)
}

#[cfg(test)]
fn verify_directory(directory: &File, root: bool) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = directory.metadata()?;
    // SAFETY: geteuid takes no arguments and has no side effects.
    let uid = unsafe { libc::geteuid() };
    if !meta.is_dir() || (meta.uid() != uid && meta.uid() != 0) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "untrusted artifact directory",
        ));
    }
    if root {
        if meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "artifact root is not private",
            ));
        }
    } else if meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "writable artifact ancestor",
        ));
    }
    verify_empty_acl(directory)
}

/// Create a private artifact relative to the held, verified directory handle.
#[cfg(test)]
pub(super) fn create_private_artifact(
    root: &std::path::Path,
) -> io::Result<(File, std::path::PathBuf)> {
    create_private_artifact_with(root, super::verify_file)
}

#[cfg(test)]
fn create_private_artifact_with(
    root: &std::path::Path,
    validate: impl Fn(&File) -> io::Result<()>,
) -> io::Result<(File, std::path::PathBuf)> {
    use std::ffi::CString;
    use std::fmt::Write as _;
    use std::os::fd::FromRawFd;

    let directory = open_private_root(root)?;
    for _ in 0..16 {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(io::Error::other)?;
        let mut name = String::from("local-test-");
        for byte in nonce {
            write!(&mut name, "{byte:02x}").map_err(io::Error::other)?;
        }
        name.push_str(".log");
        let c_name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: directory is held, name is terminated, and EXCL prevents replacement.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(error);
        }
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if let Err(error) = validate(&file) {
            // The file is empty; only remove the same object we just created.
            if let Err(cleanup) = remove_created_artifact(&directory, &c_name, &file) {
                return Err(io::Error::other(format!(
                    "artifact validation failed ({error}); cleanup failed ({cleanup})"
                )));
            }
            return Err(error);
        }
        return Ok((file, root.join(name)));
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique artifact",
    ))
}

#[cfg(test)]
pub(super) fn create_rejected_artifact_for_test(
    root: &std::path::Path,
) -> io::Result<(File, std::path::PathBuf)> {
    create_private_artifact_with(root, |_| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "fixture"))
    })
}

/// Remove a freshly created, still-identical empty artifact after validation fails.
/// The root/current-UID trust boundary excludes concurrent same-UID replacement.
#[cfg(test)]
pub(super) fn remove_created_artifact(
    directory: &File,
    name: &std::ffi::CStr,
    original: &File,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let original_meta = original.metadata()?;
    let mut current = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: directory and name are live; storage is writable and links are not followed.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            current.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful fstatat initialized the stat object.
    let current = unsafe { current.assume_init() };
    if current.st_mode & libc::S_IFMT != libc::S_IFREG
        || (original_meta.dev(), original_meta.ino())
            != (current.st_dev as u64, current.st_ino as u64)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "artifact identity changed during cleanup",
        ));
    }
    // SAFETY: the name is relative to the held directory and was just checked.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reopen only the original artifact object, relative to a verified root handle.
#[cfg(test)]
pub(super) fn reopen_private_artifact(
    root: &std::path::Path,
    path: &std::path::Path,
    identity: (u64, u64),
) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;

    if path.parent() != Some(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "artifact is outside its root",
        ));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let nonce = name
        .strip_prefix("local-test-")
        .and_then(|value| value.strip_suffix(".log"));
    if !nonce.is_some_and(|value| {
        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid artifact name",
        ));
    }
    let directory = open_private_root(root)?;
    let c_name =
        CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: directory is held, name is terminated, and flags forbid links and FIFO blocking.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    super::verify_file(&file)?;
    let meta = file.metadata()?;
    if (meta.dev(), meta.ino()) != identity {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "artifact identity changed",
        ));
    }
    Ok(file)
}

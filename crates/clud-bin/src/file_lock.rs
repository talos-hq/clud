//! Advisory file locks over kernal-api `fs`, with the call shape clud used
//! from `fs4` 0.13: `try_lock_*` returns `Ok(false)` when another holder has
//! the lock, and `unlock` is explicit (dropping the `File` also releases it).

use std::fs::File;
use std::io;

/// Lock methods on [`File`]; import the trait to call them.
pub trait FileExt {
    fn lock_exclusive(&self) -> io::Result<()>;
    fn lock_shared(&self) -> io::Result<()>;
    fn try_lock_exclusive(&self) -> io::Result<bool>;
    fn try_lock_shared(&self) -> io::Result<bool>;
    fn unlock(&self) -> io::Result<()>;
}

fn contended(result: io::Result<()>) -> io::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if kernal_api::fs_is_lock_conflict(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

impl FileExt for File {
    fn lock_exclusive(&self) -> io::Result<()> {
        kernal_api::fs_lock_exclusive(self)
    }

    fn lock_shared(&self) -> io::Result<()> {
        kernal_api::fs_lock_shared(self)
    }

    fn try_lock_exclusive(&self) -> io::Result<bool> {
        contended(kernal_api::fs_try_lock_exclusive(self))
    }

    fn try_lock_shared(&self) -> io::Result<bool> {
        contended(kernal_api::fs_try_lock_shared(self))
    }

    fn unlock(&self) -> io::Result<()> {
        kernal_api::fs_unlock(self)
    }
}

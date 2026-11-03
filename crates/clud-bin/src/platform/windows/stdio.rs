//! Process stdio redirection (#2386), Windows side. Rust's `eprintln!` calls
//! `GetStdHandle(STD_ERROR_HANDLE)` on every write, so replacing the process
//! handle is enough; no CRT fd is involved.

use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_HANDLE};

/// Point this process's stderr at `file`. A duplicate of the handle is
/// installed as `STD_ERROR_HANDLE`. Handles from earlier redirects are
/// intentionally leaked (one per 256 KiB rotation): another thread's
/// `eprintln!` may still be writing to the old handle, and closing it would
/// risk a write to a closed or reused handle.
pub fn redirect_stderr_to(file: &File) -> io::Result<()> {
    install_std(STD_ERROR_HANDLE, file).map(|_| ())
}

/// Duplicate `file`'s handle (leaked on purpose) and install it as `id`.
fn install_std(id: STD_HANDLE, file: &File) -> io::Result<HANDLE> {
    let handle = leak_duplicate(file)?;
    // SAFETY: `handle` is a live file handle that is never closed.
    unsafe { SetStdHandle(id, handle) }.map_err(io::Error::other)?;
    Ok(handle)
}

/// Duplicate `file`'s handle and leak the duplicate, returning it.
fn leak_duplicate(file: &File) -> io::Result<HANDLE> {
    let dup = file.try_clone()?;
    let handle = HANDLE(dup.as_raw_handle());
    std::mem::forget(dup);
    Ok(handle)
}

/// Modern Windows Terminal supports Rust's Unicode console writes. Legacy
/// console code pages use ASCII replacement; redirected streams default UTF-8.
pub fn output_is_utf8() -> bool {
    use std::io::IsTerminal;
    use windows::Win32::System::Console::GetConsoleOutputCP;
    if !std::io::stdout().is_terminal() && !std::io::stderr().is_terminal() {
        return true;
    }
    std::env::var_os("WT_SESSION").is_some()
        // SAFETY: this read-only API takes no pointers or handles.
        || unsafe { GetConsoleOutputCP() } == 65001
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::WriteFile;

    /// Writes through the duplicated handle land in the file. The process
    /// std handles are shared with parallel tests, so none is swapped here;
    /// `install_std` is `leak_duplicate` plus one `SetStdHandle` call.
    #[test]
    fn leaked_duplicate_writes_into_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("err.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let dup = leak_duplicate(&file).unwrap();
        assert_ne!(dup.0, file.as_raw_handle());
        let mut written = 0u32;
        // SAFETY: `dup` is a live handle owned by this test (leaked).
        unsafe { WriteFile(dup, Some(b"hello"), Some(&mut written), None) }.unwrap();
        assert_eq!(written, 5);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }
}

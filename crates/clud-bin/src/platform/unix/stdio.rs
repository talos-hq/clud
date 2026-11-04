//! Process stdio redirection (#2386), Unix side: `dup2` onto the target fd.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// Point this process's stderr (fd 2) at `file`. The file may be dropped
/// afterwards: fd 2 holds its own duplicate.
pub fn redirect_stderr_to(file: &File) -> io::Result<()> {
    redirect_fd(file, 2)
}

/// `dup2(file, target)`; split out so tests can aim at a spare fd instead of
/// the test process's own stderr.
fn redirect_fd(file: &File, target: i32) -> io::Result<()> {
    // SAFETY: both descriptors are valid for the call; dup2 does not take
    // ownership of `file`'s fd.
    if unsafe { libc::dup2(file.as_raw_fd(), target) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Unix streams carry bytes; locale policy is applied by the caller. Missing
/// locale information uses the safe ASCII common subset.
pub fn output_is_utf8() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;

    #[test]
    fn redirect_fd_sends_writes_to_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        // A scratch fd standing in for fd 2.
        let scratch = File::open("/dev/null").unwrap();
        let target = scratch.as_raw_fd();
        redirect_fd(&file, target).unwrap();
        drop(file);
        // SAFETY: `scratch` owns `target`; borrow it via a dup'd handle.
        let dup = unsafe { libc::dup(target) };
        assert!(dup >= 0, "dup failed: {}", io::Error::last_os_error());
        let mut out = unsafe { File::from_raw_fd(dup) };
        writeln!(out, "stray-2386").unwrap();
        drop(out);
        let mut text = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "stray-2386\n");
    }

    #[test]
    fn redirect_fd_rejects_a_bad_target() {
        let file = File::open("/dev/null").unwrap();
        assert!(redirect_fd(&file, -1).is_err());
    }
}

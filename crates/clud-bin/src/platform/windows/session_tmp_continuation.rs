//! Windows half of `crate::gc::session_tmp_continuation`: no mode bits, and
//! a path's raw form is its UTF-16 code units (little-endian bytes).

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Always false: Windows has no owner write/search mode bits to restore.
pub fn make_owner_writable(_dir: &Path) -> bool {
    false
}

pub fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

pub fn bytes_path(bytes: &[u8]) -> Result<PathBuf, &'static str> {
    if bytes.len() & 1 != 0 {
        return Err("odd number of UTF-16 path bytes");
    }
    let words = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(OsString::from_wide(&words).into())
}

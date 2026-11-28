//! Owner-only files, written atomically.
//!
//! For local data that may carry secrets: session previews, compact
//! summaries, recovery payloads (#922). On Unix the file is created with mode
//! `0600`; on Windows it gets the owner-only protected DACL that
//! `codex_auth` uses for credentials. The write goes to a unique temp file in
//! the same directory and is renamed into place, so a reader never sees a
//! torn file and an interrupted write leaves the old one intact.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Atomically replace `path` with `bytes`, readable by its owner only.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = unique_temp(path);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        crate::platform::fs_private::owner_only_mode(&mut options);
        let mut file = options.open(&temp)?;
        crate::platform::fs_private::protect_owner_only(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        crate::platform::fs_private::protect_owner_only(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn unique_temp(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_replace_atomically_and_leave_no_temp_files() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("nested").join("index.json");
        write_private_atomic(&path, b"one").unwrap();
        write_private_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }
}

//! Windows-only tests of `crate::grind_facts`: delete-pending and inaccessible
//! lock files.

#[cfg(test)]
mod tests {
    use crate::grind_facts::tests::{write, A};
    use crate::grind_facts::{lookup_in, record_task};
    use std::time::{Duration, SystemTime};

    #[test]
    fn delete_pending_lock_is_retried_without_losing_facts() {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FileDispositionInfo, SetFileInformationByHandle, DELETE,
            FILE_ATTRIBUTE_NORMAL, FILE_DISPOSITION_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        };

        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let dir = root.path().join("grind");
        std::fs::create_dir(&dir).unwrap();
        let path = write(&dir, A, r#"{"mode":"parallel"}"#);
        let lock = path.with_extension("lock");
        // Keep a handle open while deleting the name. Windows marks the file
        // delete-pending until this handle closes; a new create then fails
        // with ERROR_ACCESS_DENIED (5), not AlreadyExists.
        let handle = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0x7) // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
            .open(&lock)
            .unwrap();
        // Mark the file for classic (non-POSIX) deletion while another handle
        // remains open. DeleteFileW and Rust's remove_file both freed the name
        // immediately on the CI runner, so neither reproduces error 5 there.
        let wide: Vec<u16> = lock.as_os_str().encode_wide().chain(Some(0)).collect();
        let delete_handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                DELETE | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            delete_handle,
            INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );
        let delete_handle = unsafe { std::fs::File::from_raw_handle(delete_handle) };
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
        assert_ne!(
            unsafe {
                SetFileInformationByHandle(
                    delete_handle.as_raw_handle(),
                    FileDispositionInfo,
                    &disposition as *const _ as *const _,
                    std::mem::size_of_val(&disposition) as u32,
                )
            },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        drop(delete_handle);
        let observed = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .unwrap_err();
        assert_eq!(observed.raw_os_error(), Some(5), "{observed}");

        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(handle);
        });
        let checkout = root.path().join("repo-wt-1");
        let result = record_task(&path, &checkout, &["src/a.rs".into()]);
        release.join().unwrap();
        assert_eq!(result.unwrap(), 1);
        let facts = lookup_in(&dir, Some(A), SystemTime::now());
        let facts = facts.facts().unwrap();
        assert_eq!(facts["mode"], "parallel");
        assert_eq!(facts["tasks"][0]["files"], serde_json::json!(["src/a.rs"]));
    }

    #[test]
    fn permanently_inaccessible_lock_fails_promptly() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let dir = root.path().join("grind");
        std::fs::create_dir(&dir).unwrap();
        let path = write(&dir, A, "{}");
        let lock = path.with_extension("lock");
        std::fs::create_dir(&lock).unwrap();
        let started = std::time::Instant::now();
        let error = record_task(&path, root.path(), &["src/a.rs".into()]).unwrap_err();
        assert!(error.contains("lock "), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2), "{error}");
        assert!(
            lock.is_dir(),
            "a failed acquisition must not remove the directory"
        );
    }
}

//! Validate the opened credential file's owner and protected owner-only DACL.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, IsWellKnownSid, TokenUser, WinBuiltinAdministratorsSid,
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct Descriptor(PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: GetSecurityInfo allocates this descriptor with LocalAlloc.
        unsafe { LocalFree(Some(HLOCAL(self.0 .0))) };
    }
}

fn insecure() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "credential file is not owner-only",
    )
}

pub fn read(path: &Path) -> io::Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(insecure());
    }
    verify_acl(&file)?;
    let mut secret = String::new();
    file.read_to_string(&mut secret)?;
    Ok(secret)
}

fn verify_acl(file: &File) -> io::Result<()> {
    let mut owner = PSID::default();
    let mut raw = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the borrowed file handle and output pointers are valid; the
    // returned descriptor owns the owner SID until Descriptor drops it.
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut raw),
        )
    };
    if status.0 != 0 {
        return Err(io::Error::other(format!(
            "could not inspect credential ACL: {}",
            status.0
        )));
    }
    let descriptor = Descriptor(raw);
    if !is_current_owner(owner)? {
        return Err(insecure());
    }
    let acl = local_string(|output| {
        // SAFETY: descriptor is live and the conversion allocates output.
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                output,
                None,
            )
        }
    })?;
    let owner_sid = local_string(|output| {
        // SAFETY: owner points inside the live descriptor.
        unsafe { ConvertSidToStringSidW(owner, output) }
    })?;
    // Accept only the protected single-owner ACL written by fs_private, or
    // its equivalent explicit current-owner SID. Extra/inherited ACEs fail closed.
    if !is_owner_only_acl(&acl, &owner_sid) {
        return Err(insecure());
    }
    Ok(())
}

/// Windows retains auto-inheritance bookkeeping flags even after protection.
/// Require protection and the exact single-owner ACE, independent of those flags.
fn is_owner_only_acl(acl: &str, owner_sid: &str) -> bool {
    let Some((flags, ace)) = acl.strip_prefix("D:").and_then(|acl| acl.split_once('(')) else {
        return false;
    };
    let flags = flags.replace("AR", "").replace("AI", "");
    flags == "P" && (ace == "A;;FA;;;OW)" || ace == format!("A;;FA;;;{owner_sid})"))
}

fn is_current_owner(owner: PSID) -> io::Result<bool> {
    // Windows may make Administrators the default owner for an elevated
    // creator. Administrators are already an OS trust boundary, like Unix
    // root; the protected owner-only DACL is still required below.
    // https://learn.microsoft.com/windows/win32/secauthz/owner-of-a-new-object
    // SAFETY: owner points into the live descriptor returned by GetSecurityInfo.
    if unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) }.as_bool() {
        return Ok(true);
    }
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and token is a
    // valid output; TOKEN_QUERY is read-only and the handle is closed below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(io::Error::other)?;
    let result = (|| {
        let mut size = 0;
        // SAFETY: the first call intentionally supplies no buffer and obtains
        // its required byte count in the valid size pointer.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let bytes = usize::try_from(size).map_err(io::Error::other)?;
        // usize storage supplies TOKEN_USER's pointer alignment, unlike Vec<u8>.
        let mut storage = vec![0_usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: storage is aligned and holds at least size bytes. The token
        // and returned TOKEN_USER SID remain live throughout the comparison.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                size,
                &mut size,
            )
            .map_err(io::Error::other)?;
            let user = &*storage.as_ptr().cast::<TOKEN_USER>();
            Ok(EqualSid(owner, user.User.Sid).is_ok())
        }
    })();
    // SAFETY: token was opened above and is no longer borrowed.
    let _ = unsafe { CloseHandle(token) };
    result
}

fn local_string(make: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> io::Result<String> {
    let mut output = PWSTR::null();
    make(&mut output).map_err(io::Error::other)?;
    // SAFETY: the documented conversion API allocated a terminated UTF-16 string.
    let value = unsafe { output.to_string() }.map_err(io::Error::other);
    // SAFETY: both conversion APIs allocate output with LocalAlloc.
    unsafe { LocalFree(Some(HLOCAL(output.0.cast()))) };
    value
}

pub fn create_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    #[test]
    fn owner_only_acl_accepts_bookkeeping_but_rejects_extra_access() {
        let owner = "S-1-5-21-123-456-789-1001";
        for flags in ["P", "PAI", "PAR", "PARAI"] {
            assert!(is_owner_only_acl(&format!("D:{flags}(A;;FA;;;OW)"), owner));
            assert!(is_owner_only_acl(
                &format!("D:{flags}(A;;FA;;;{owner})"),
                owner
            ));
        }
        for acl in [
            "D:AI(A;;FA;;;OW)",
            "D:P(A;;FA;;;WD)",
            "D:P(A;;FA;;;OW)(A;;FR;;;WD)",
            "D:P(A;ID;FA;;;OW)",
            "D:PNO_ACCESS_CONTROL(A;;FA;;;OW)",
            "D:P",
        ] {
            assert!(!is_owner_only_acl(acl, owner), "{acl}");
        }
    }

    #[test]
    fn a_written_private_file_is_readable_but_a_broadened_dacl_is_refused() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("key");
        crate::fs_private::write_private_atomic(&path, b"fixture-key").unwrap();
        assert_eq!(read(&path).unwrap(), "fixture-key");
        let mut raw = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the constant SDDL is terminated and raw is a valid output.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;;FA;;;WD)"),
                SDDL_REVISION_1,
                &mut raw,
                None,
            )
            .unwrap();
        }
        let descriptor = Descriptor(raw);
        let mut present = windows::core::BOOL::default();
        let mut defaulted = windows::core::BOOL::default();
        let mut dacl = std::ptr::null_mut();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: descriptor, path, and output pointers remain valid; this
        // deliberately broadens only the temporary test file's DACL.
        unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
                .unwrap();
            let result = SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl.cast_const()),
                None,
            );
            assert_eq!(result.0, 0);
        }
        assert_eq!(
            read(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}

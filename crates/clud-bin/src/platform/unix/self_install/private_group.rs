//! Whether a user-owned directory is writable by the user alone, counting a
//! user-private group (UPG) as the user's own.
//!
//! Debian, Ubuntu, Pop!_OS and Fedora give each user a primary group of the
//! same name with no other members and a default umask of 002, so directories
//! the user creates, `~/.local/bin` included, are group-writable by a group
//! that only that user is in. Rejecting those refused the default install
//! directory (`install directory is not exclusively user-controlled`).
//! World-writable directories, directories owned by someone else, and
//! group-writable directories whose group anyone else could be in stay unsafe.

use std::ffi::CStr;

/// The ownership and permission bits of a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirFacts {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

/// The effective user, from the account database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Account {
    pub uid: u32,
    pub primary_gid: u32,
    pub name: Vec<u8>,
}

/// A group entry: its name and its explicitly listed members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Group {
    pub name: Vec<u8>,
    pub members: Vec<Vec<u8>>,
}

/// The pure decision. `account` and `group` are only consulted when the
/// directory is group-writable; `None` (lookup failed) means unsafe.
pub(crate) fn only_user_can_write(
    dir: DirFacts,
    euid: u32,
    account: impl FnOnce() -> Option<Account>,
    group: impl FnOnce(u32) -> Option<Group>,
) -> bool {
    if dir.uid != euid || dir.mode & 0o002 != 0 {
        return false;
    }
    if dir.mode & 0o020 == 0 {
        return true;
    }
    let Some(account) = account().filter(|account| account.uid == euid) else {
        return false;
    };
    dir.gid == account.primary_gid
        && group(dir.gid).is_some_and(|group| is_private_to(&group, &account))
}

/// A user-private group: named after the user, and listing no one else.
/// Other accounts whose primary group it is are not listed in `gr_mem`; the
/// name match is the UPG convention that rules them out in practice.
fn is_private_to(group: &Group, account: &Account) -> bool {
    group.name == account.name && group.members.iter().all(|member| *member == account.name)
}

/// [`only_user_can_write`] against the live account and group databases.
pub(crate) fn user_controlled(dir: DirFacts) -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    only_user_can_write(dir, euid, || account(euid), group)
}

/// Runs a reentrant `get*_r` lookup, growing the buffer on ERANGE.
fn with_buffer<T>(mut lookup: impl FnMut(&mut [u8]) -> Result<Option<T>, i32>) -> Option<T> {
    let mut buffer = vec![0_u8; 4096];
    loop {
        match lookup(&mut buffer) {
            Err(libc::ERANGE) if buffer.len() < 1024 * 1024 => {
                buffer.resize(buffer.len() * 2, 0);
            }
            Err(_) => return None,
            Ok(found) => return found,
        }
    }
}

fn account(uid: u32) -> Option<Account> {
    with_buffer(|buffer| {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: getpwuid_r receives a writable entry and buffer that live through this call.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if status != 0 {
            return Err(status);
        }
        if found.is_null() {
            return Ok(None);
        }
        // SAFETY: found points to the entry written by a successful getpwuid_r,
        // and pw_name into the live buffer.
        let (name, primary_gid) = unsafe { ((*found).pw_name, (*found).pw_gid) };
        if name.is_null() {
            return Ok(None);
        }
        // SAFETY: a non-null pw_name is a NUL-terminated string in the buffer.
        let name = unsafe { CStr::from_ptr(name) }.to_bytes().to_vec();
        Ok(Some(Account {
            uid,
            primary_gid,
            name,
        }))
    })
}

fn group(gid: u32) -> Option<Group> {
    with_buffer(|buffer| {
        let mut entry = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: getgrgid_r receives a writable entry and buffer that live through this call.
        let status = unsafe {
            libc::getgrgid_r(
                gid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if status != 0 {
            return Err(status);
        }
        if found.is_null() {
            return Ok(None);
        }
        // SAFETY: found points to the entry written by a successful getgrgid_r.
        let (name, mut member) = unsafe { ((*found).gr_name, (*found).gr_mem) };
        if name.is_null() || member.is_null() {
            return Ok(None);
        }
        // SAFETY: a non-null gr_name is a NUL-terminated string in the buffer.
        let name = unsafe { CStr::from_ptr(name) }.to_bytes().to_vec();
        let mut members = Vec::new();
        // SAFETY: gr_mem is a NULL-terminated array of NUL-terminated strings
        // in the buffer; the walk stops at the terminating NULL.
        unsafe {
            while !(*member).is_null() {
                members.push(CStr::from_ptr(*member).to_bytes().to_vec());
                member = member.add(1);
            }
        }
        Ok(Some(Group { name, members }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (case, owner uid, gid, mode, group entry, safe)
    type Case = (&'static str, u32, u32, u32, Option<Group>, bool);

    const ME: u32 = 1000;
    const MY_GROUP: u32 = 1000;

    fn me() -> Account {
        Account {
            uid: ME,
            primary_gid: MY_GROUP,
            name: b"pat".to_vec(),
        }
    }

    fn group_of(name: &str, members: &[&str]) -> Group {
        Group {
            name: name.as_bytes().to_vec(),
            members: members.iter().map(|m| m.as_bytes().to_vec()).collect(),
        }
    }

    #[test]
    fn decision_table() {
        let upg = group_of("pat", &[]);
        let cases: &[Case] = &[
            ("0700 own dir", ME, MY_GROUP, 0o700, None, true),
            ("0755 own dir", ME, 4242, 0o755, None, true),
            (
                "UPG 0775 (umask 002)",
                ME,
                MY_GROUP,
                0o775,
                Some(upg.clone()),
                true,
            ),
            (
                "UPG listing only the user",
                ME,
                MY_GROUP,
                0o770,
                Some(group_of("pat", &["pat"])),
                true,
            ),
            (
                "world-writable",
                ME,
                MY_GROUP,
                0o777,
                Some(upg.clone()),
                false,
            ),
            (
                "world-writable, no group write",
                ME,
                MY_GROUP,
                0o757,
                None,
                false,
            ),
            ("owned by another user", 1001, MY_GROUP, 0o755, None, false),
            (
                "another user's UPG dir",
                1001,
                1001,
                0o775,
                Some(upg.clone()),
                false,
            ),
            (
                "shared group with another member",
                ME,
                MY_GROUP,
                0o775,
                Some(group_of("pat", &["pat", "sam"])),
                false,
            ),
            (
                "primary group not named after the user (users)",
                ME,
                MY_GROUP,
                0o775,
                Some(group_of("users", &[])),
                false,
            ),
            (
                "group-writable by a secondary group",
                ME,
                27,
                0o775,
                Some(group_of("pat", &[])),
                false,
            ),
            ("group lookup failed", ME, MY_GROUP, 0o775, None, false),
        ];
        for (case, uid, gid, mode, entry, safe) in cases {
            let dir = DirFacts {
                uid: *uid,
                gid: *gid,
                mode: *mode,
            };
            let entry = entry.clone();
            let got = only_user_can_write(dir, ME, || Some(me()), move |_| entry);
            assert_eq!(got, *safe, "{case}");
        }
    }

    #[test]
    fn a_failed_account_lookup_rejects_group_write_only() {
        let upg = DirFacts {
            uid: ME,
            gid: MY_GROUP,
            mode: 0o775,
        };
        assert!(!only_user_can_write(upg, ME, || None, |_| None));
        let private = DirFacts { mode: 0o700, ..upg };
        assert!(only_user_can_write(private, ME, || None, |_| None));
    }

    #[test]
    fn the_live_databases_describe_the_current_user() {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        let Some(me) = account(euid) else {
            return; // a container without a passwd entry for this uid
        };
        assert!(!me.name.is_empty());
        assert!(group(me.primary_gid).is_none_or(|group| !group.name.is_empty()));
    }
}

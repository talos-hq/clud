//! Unix half of `crate::rm_tool`: file ownership and device/inode identity
//! from `MetadataExt`, mode bits, the effective uid, Linux mount and tmpfs
//! detection.

use std::fs::{Metadata, OpenOptions, Permissions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The caller's effective uid.
pub fn effective_uid() -> Option<u32> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    Some(unsafe { libc::geteuid() })
}

/// The owner uid of `meta`.
pub fn file_owner(meta: &Metadata) -> Option<u32> {
    Some(meta.uid())
}

/// The `(dev, ino)` identity of `meta`.
pub fn file_identity(meta: &Metadata) -> Option<(u64, u64)> {
    Some((meta.dev(), meta.ino()))
}

/// Give a directory's owner full access.
pub fn make_dir_permissions_writable(permissions: &mut Permissions) {
    permissions.set_mode(permissions.mode() | 0o700);
}

/// Files keep their mode bits on Unix; only directories are opened up.
pub fn make_file_writable(_path: &Path, _permissions: Permissions) {}

/// Retry an idle managed leaf's removal through caller-owned directories.
/// Only its parent directory gains owner permissions; shared file inodes and
/// sibling trees are untouched. Every descendant directory is opened without
/// following links, and unlink uses the same parent handle as the chmod.
pub fn remove_owned_leaf_writable(root: &Path, path: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let denied = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "scratch is not caller-owned",
        )
    };
    let relative = path.strip_prefix(root).map_err(|_| denied())?;
    let parent = relative.parent().ok_or_else(denied)?;
    let name = relative.file_name().ok_or_else(denied)?;
    let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(root)?;
    if file_owner(&directory.metadata()?) != effective_uid() {
        return Err(denied());
    }
    for component in parent.components() {
        let Component::Normal(component) = component else {
            return Err(denied());
        };
        let component = CString::new(component.as_bytes()).map_err(|_| denied())?;
        // SAFETY: the live directory fd and NUL-terminated component outlive
        // openat; O_NOFOLLOW and O_DIRECTORY refuse links and non-directories.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                flags | libc::O_RDONLY,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a newly owned descriptor on success.
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
        if file_owner(&directory.metadata()?) != effective_uid() {
            return Err(denied());
        }
    }
    let name = CString::new(name.as_bytes()).map_err(|_| denied())?;
    // SAFETY: an all-zero stat is valid storage for fstatat's output.
    let mut leaf: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the directory fd, NUL-terminated name and writable stat storage
    // are live; AT_SYMLINK_NOFOLLOW inspects a link itself, never its target.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut leaf,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if Some(leaf.st_uid) != effective_uid() {
        return Err(denied());
    }
    let mut permissions = directory.metadata()?.permissions();
    make_dir_permissions_writable(&mut permissions);
    directory.set_permissions(permissions)?;
    // SAFETY: the live directory fd and NUL-terminated leaf name identify an
    // entry in this same owned directory; flags=0 never follows the leaf link.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Recreate the symlink `source` (pointing at `link`) at `dest`.
pub fn copy_symlink(_source: &Path, link: PathBuf, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link, dest)
}

#[cfg(target_os = "linux")]
pub fn reject_mounts(target: &Path) -> Result<(), String> {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Ok(());
    };
    for line in mounts.lines() {
        let Some(field) = line.split_whitespace().nth(4) else {
            continue;
        };
        let mount = field
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        if Path::new(&mount).starts_with(target) {
            return Err(format!("contains the mount point {mount}"));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn reject_mounts(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Whether `path` is on tmpfs or ramfs (#1659).
#[cfg(target_os = "linux")]
pub fn is_ram_backed(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    const RAMFS_MAGIC: i64 = 0x8584_58f6;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: an all-zero statfs is a valid value for this plain C struct.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `buf` is a writable statfs that
    // outlives the call.
    if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
        return false;
    }
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let kind = buf.f_type as i64;
    kind == TMPFS_MAGIC || kind == RAMFS_MAGIC
}

#[cfg(not(target_os = "linux"))]
pub fn is_ram_backed(_: &Path) -> bool {
    false
}

/// A session grant's checkout identity: its device and inode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Whether the caller owns the checkout `root`.
pub fn owned_by_caller(root: &Path) -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    std::fs::metadata(root).is_ok_and(|meta| meta.uid() == unsafe { libc::geteuid() })
}

/// The identity to record for the checkout `root`.
pub fn grant_identity(root: &Path) -> Result<GrantIdentity, String> {
    let meta = std::fs::metadata(root).map_err(|e| format!("cannot stat checkout: {e}"))?;
    Ok(GrantIdentity {
        device: meta.dev(),
        inode: meta.ino(),
    })
}

/// Whether `root` is still the checkout `identity` recorded.
pub fn same_grant_identity(root: &Path, identity: &GrantIdentity) -> bool {
    grant_identity(root)
        .is_ok_and(|now| now.device == identity.device && now.inode == identity.inode)
}

/// Grant records are owner-only.
pub fn grant_record_mode(options: &mut OpenOptions) {
    options.mode(0o600);
}

/// Seal a test directory read-only (`0555`), as a build output would be.
#[cfg(test)]
pub(crate) fn seal_dir_for_test(dir: &Path) {
    std::fs::set_permissions(dir, Permissions::from_mode(0o555)).unwrap();
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::is_ram_backed;
    use crate::gc::{CreatedEntry, CreatedKind};
    use crate::rm_tool::tests::{run_ledger, shm_roots, shm_world, temp_world, world};
    use crate::rm_tool::{copy_nofollow, Kind};
    use std::path::Path;

    #[test]
    fn readonly_leaf_retry_rejects_a_symlinked_parent_without_changing_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("managed");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"precious").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        super::seal_dir_for_test(&outside);
        let result = super::remove_owned_leaf_writable(&root, &root.join("link/keep"));
        let traversal = super::remove_owned_leaf_writable(&root, &root.join("../outside/keep"));
        let unchanged = std::fs::metadata(&outside)
            .unwrap()
            .permissions()
            .readonly();
        crate::platform::gc_test_fs::set_mode(&outside, 0o700).unwrap();
        assert!(result.is_err());
        assert!(traversal.is_err());
        assert!(unchanged);
        assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"precious");
    }

    #[test]
    fn readonly_leaf_retry_refuses_a_root_owned_by_someone_else() {
        let root = Path::new("/");
        if super::file_owner(&std::fs::metadata(root).unwrap()) == super::effective_uid() {
            return;
        }
        let result = super::remove_owned_leaf_writable(root, &root.join("unused-gc-leaf"));
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn readonly_leaf_retry_unlinks_a_final_symlink_without_chmodding_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("managed");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("protected");
        std::fs::write(&outside, b"precious").unwrap();
        crate::platform::gc_test_fs::set_mode(&outside, 0o444).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        super::seal_dir_for_test(&root);
        let result = super::remove_owned_leaf_writable(&root, &link);
        crate::platform::gc_test_fs::set_mode(&root, 0o700).unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(std::fs::symlink_metadata(link).is_err());
        assert!(std::fs::metadata(&outside)
            .unwrap()
            .permissions()
            .readonly());
        assert_eq!(std::fs::read(outside).unwrap(), b"precious");
    }

    #[test]
    fn a_symlinked_parent_that_leaves_the_roots_is_refused_and_a_final_link_is_removed_as_a_link() {
        let w = world();
        let outside = w.home.parent().unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"precious").unwrap();
        std::os::unix::fs::symlink(&outside, w.root.join("escape")).unwrap();
        let (code, _, err) = w.run(Kind::File, &["escape/keep"]);
        assert_eq!(code, 1);
        assert!(err.contains("outside"), "{err}");
        assert!(outside.join("keep").exists());
        // `safe-rm escape` would follow nothing: the link is not a directory.
        let (code, _, err) = w.run(Kind::Dir, &["escape"]);
        assert_eq!(code, 1);
        assert!(err.contains("not a directory"), "{err}");
        // `safe-rm escape` trashes the link itself; the target survives.
        let (code, out, err) = w.run(Kind::File, &["escape"]);
        assert_eq!(code, 0, "{err}");
        assert!(out.is_empty(), "{out}");
        assert!(std::fs::symlink_metadata(w.root.join("escape")).is_err());
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn a_symlinked_parent_into_git_metadata_is_refused() {
        let w = world();
        let git = w.root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config"), b"keep").unwrap();
        std::os::unix::fs::symlink(&git, w.root.join("link")).unwrap();
        let (code, _, err) = w.run(Kind::File, &["--purge", "link/config"]);
        assert_eq!(code, 1);
        assert!(err.contains("git metadata"), "{err}");
        assert!(git.join("config").exists());
    }

    #[test]
    fn cross_volume_copy_keeps_symlinks_as_links() {
        let w = world();
        let src = w.root.join("tree");
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::write(src.join("d/f"), b"f").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.join("link")).unwrap();
        let dest = w.home.join("copy");
        copy_nofollow(&src, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("d/f")).unwrap(), b"f");
        assert!(std::fs::symlink_metadata(dest.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_symlink_escape_from_the_system_temp_dir_is_refused_and_the_link_removed_as_a_link() {
        let tw = temp_world();
        std::os::unix::fs::symlink(&tw.outside, tw.temp.join("link")).unwrap();
        let (code, _, err) = tw.run(&["--purge", &tw.t("link/keep")]);
        assert_eq!(code, 1, "{err}");
        assert!(err.contains("outside"), "{err}");
        assert!(tw.outside.join("keep").exists());
        // The link itself goes; its target stays.
        let (code, _, err) = tw.run(&["--purge", &tw.t("link")]);
        assert_eq!(code, 0, "{err}");
        assert!(std::fs::symlink_metadata(tw.temp.join("link")).is_err());
        assert!(tw.outside.join("keep").exists());
    }

    #[test]
    fn a_temp_entry_owned_by_another_user_is_refused() {
        let tw = temp_world();
        std::fs::create_dir_all(tw.temp.join("theirs/sub")).unwrap();
        std::fs::write(tw.temp.join("theirs/sub/f"), b"x").unwrap();
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        let other = me.wrapping_add(1);
        let roots = || tw.roots().with_temp_owner(other);
        let (code, _, err) = tw.run_with_roots(roots(), &["-r", "--purge", &tw.t("theirs")]);
        assert_eq!(code, 1, "{err}");
        assert!(err.contains("not owned by you"), "{err}");
        // A path below an entry owned by someone else is refused too.
        let (code, _, err) = tw.run_with_roots(roots(), &["--purge", &tw.t("theirs/sub/f")]);
        assert_eq!(code, 1, "{err}");
        assert!(err.contains("not owned by you"), "{err}");
        assert!(tw.temp.join("theirs/sub/f").exists());
    }

    #[test]
    fn ram_backed_temp_roots_keep_the_ownership_and_symlink_rules() {
        let (tw, shm) = shm_world();
        // SAFETY: geteuid has no preconditions.
        let other = unsafe { libc::geteuid() }.wrapping_add(1);
        let target = shm.join("wild-review").to_string_lossy().into_owned();
        let roots = shm_roots(&tw, &shm).with_temp_owner(other);
        let (code, _, err) = tw.run_with_roots(roots, &["-r", &target]);
        assert_eq!(code, 1, "{err}");
        assert!(err.contains("not owned by you"), "{err}");
        assert!(shm.join("wild-review/out/a.o").exists());
        // A link out of the shm root is judged by its target.
        std::os::unix::fs::symlink(&tw.outside, shm.join("link")).unwrap();
        let escape = shm.join("link/keep").to_string_lossy().into_owned();
        let (code, _, err) = tw.run_with_roots(shm_roots(&tw, &shm), &[&escape]);
        assert_eq!(code, 1, "{err}");
        assert!(tw.outside.join("keep").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ram_backing_is_detected_from_the_filesystem_type() {
        // /proc is procfs, never tmpfs; a missing path is not RAM-backed.
        assert!(!is_ram_backed(Path::new("/proc")));
        assert!(!is_ram_backed(Path::new("/definitely/not/here")));
    }

    fn recorded(path: &Path, kind: CreatedKind) -> CreatedEntry {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).unwrap();
        CreatedEntry {
            session_id: "session-1".into(),
            path: path.display().to_string(),
            kind,
            role: "agent".into(),
            created_unix: 1,
            dev: Some(meta.dev()),
            ino: Some(meta.ino()),
            uid: Some(meta.uid()),
        }
    }

    #[test]
    fn ledger_recorded_directory_outside_the_roots_is_removed_wholesale() {
        let w = world();
        let out = w.home.parent().unwrap().join("elsewhere").join("out");
        std::fs::create_dir_all(out.join("a")).unwrap();
        std::fs::write(out.join("a").join("b.txt"), "x").unwrap();
        let rows = Ok(vec![recorded(&out, CreatedKind::Dir)]);
        let (code, _, err) = run_ledger(&w, rows, &["-r", out.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        assert!(!out.exists());
        let records = w.audit_records();
        let reason = records[0]["paths"][0]["reason"]
            .as_str()
            .unwrap_or_default();
        assert!(reason.contains("creation ledger"), "{records:?}");
    }

    #[test]
    fn ledger_pre_existing_sibling_of_a_recorded_file_is_refused() {
        let w = world();
        let docs = w.home.parent().unwrap().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let report = docs.join("report.md");
        let other = docs.join("other.md");
        std::fs::write(&report, "mine").unwrap();
        std::fs::write(&other, "theirs").unwrap();
        let rows = vec![recorded(&report, CreatedKind::File)];
        let (code, _, err) = run_ledger(&w, Ok(rows.clone()), &[other.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert!(err.contains("not created by this session"), "{err}");
        assert!(other.exists());
        let (code, _, err) = run_ledger(&w, Ok(rows), &[report.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        assert!(!report.exists());
    }

    #[test]
    fn ledger_recorded_path_swapped_for_a_symlink_is_refused_and_the_target_untouched() {
        let w = world();
        let base = w.home.parent().unwrap().to_path_buf();
        let victim = base.join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), "keep").unwrap();
        let out = base.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let rows = vec![recorded(&out, CreatedKind::Dir)];
        std::fs::remove_dir(&out).unwrap();
        std::os::unix::fs::symlink(&victim, &out).unwrap();

        let (code, _, err) = run_ledger(&w, Ok(rows.clone()), &["-r", out.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert!(err.contains("symlink"), "{err}");
        let inside = out.join("keep.txt");
        let (code, _, err) = run_ledger(&w, Ok(rows), &[inside.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert!(err.contains("not created by this session"), "{err}");
        assert!(victim.join("keep.txt").exists());
    }

    #[test]
    fn ledger_recorded_path_with_a_different_inode_is_refused() {
        let w = world();
        let base = w.home.parent().unwrap().to_path_buf();
        let out = base.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let rows = vec![recorded(&out, CreatedKind::Dir)];
        // Keep the old inode alive so the new directory cannot reuse it.
        std::fs::rename(&out, base.join("out-old")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let (code, _, err) = run_ledger(&w, Ok(rows), &["-r", out.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert!(err.contains("replaced"), "{err}");
        assert!(out.exists());
    }
}

#[cfg(test)]
mod extra_roots_tests {
    use crate::clud_settings::SafeRmExtraRootEntry;
    use crate::rm_tool::extra_roots::{self, verdict, EntryFacts};
    use crate::rm_tool::tests::extra_roots_tests::{extra_world, run_extra, with_extra};
    use std::path::PathBuf;

    fn facts(raw: &str, reason: &str) -> EntryFacts {
        EntryFacts {
            raw: raw.into(),
            reason: reason.into(),
            canonical: Ok(PathBuf::from(raw)),
            is_dir: true,
            home: Some(PathBuf::from("/home/u")),
            owner: Some(1000),
            me: Some(1000),
        }
    }

    #[test]
    fn entry_decision_table() {
        let ok = |f: EntryFacts| verdict(&f).map(|r| r.root);
        let err = |f: EntryFacts| verdict(&f).unwrap_err();
        assert_eq!(
            ok(facts("/srv/bench", "benchmark output")),
            Ok(PathBuf::from("/srv/bench"))
        );
        // Inside HOME is allowed (that is the point); HOME and above are not.
        assert!(ok(facts("/home/u/bench-out", "bench")).is_ok());
        assert!(err(facts("/home/u", "x")).contains("home directory"));
        assert!(err(facts("/home", "x")).contains("home directory"));
        assert!(err(facts("/", "x")).contains("filesystem root"));
        assert!(err(facts("/srv/bench", "  ")).contains("reason"));
        assert!(err(facts("relative/dir", "x")).contains("absolute"));
        assert!(err(facts("/srv/repo/.git", "x")).contains("git metadata"));
        let mut missing = facts("/srv/gone", "x");
        missing.canonical = Err("No such file or directory".into());
        assert!(err(missing).contains("cannot resolve"));
        let mut file = facts("/srv/file", "x");
        file.is_dir = false;
        assert!(err(file).contains("not a directory"));
        let mut foreign = facts("/srv/theirs", "x");
        foreign.owner = Some(0);
        assert!(err(foreign).contains("not owned by you"));
        // A symlinked entry is judged by its canonical target.
        let mut link = facts("/srv/link", "x");
        link.canonical = Ok(PathBuf::from("/home"));
        assert!(err(link).contains("home directory"));
    }

    #[test]
    fn a_symlink_under_an_extra_root_cannot_escape_it() {
        let (w, extra) = extra_world();
        let victim = w.home.parent().unwrap().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), "keep").unwrap();
        std::os::unix::fs::symlink(&victim, extra.join("link")).unwrap();
        let mut roots = with_extra(&w, &extra);
        let through = extra.join("link").join("keep.txt");
        let (code, _, err) = run_extra(&w, &mut roots, &[through.to_str().unwrap()]);
        assert_eq!(code, 1, "{err}");
        assert!(victim.join("keep.txt").exists());
    }

    #[test]
    fn an_entry_under_an_extra_root_owned_by_another_user_is_refused() {
        let (w, extra) = extra_world();
        let target = extra.join("theirs.txt");
        std::fs::write(&target, "x").unwrap();
        // SAFETY: geteuid has no preconditions.
        let other = unsafe { libc::geteuid() }.wrapping_add(1);
        let mut roots = with_extra(&w, &extra).with_temp_owner(other);
        let (code, _, err) = run_extra(&w, &mut roots, &[target.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert!(err.contains("not owned by you"), "{err}");
        assert!(target.exists());
    }

    #[test]
    fn load_drops_home_and_reasonless_entries_and_keeps_good_ones() {
        let (w, extra) = extra_world();
        let entries = vec![
            SafeRmExtraRootEntry {
                path: extra.display().to_string(),
                reason: "bench".into(),
            },
            SafeRmExtraRootEntry {
                path: w.home.display().to_string(),
                reason: "everything".into(),
            },
        ];
        // SAFETY: geteuid has no preconditions.
        let me = Some(unsafe { libc::geteuid() });
        let (roots, rejected) =
            extra_roots::load(&entries, vec!["shape".into()], Some(&w.home), me);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].root, extra);
        assert_eq!(rejected.len(), 2, "{rejected:?}");
        assert!(rejected[1].contains("home directory"), "{rejected:?}");
    }
}

#[cfg(test)]
mod session_grants_tests {
    use crate::rm_tool::session_grants::tests::setup;
    use crate::rm_tool::session_grants::{grant, load};
    use crate::rm_tool::Roots;
    use std::fs;

    #[test]
    fn symlink_out_of_granted_checkout_is_refused() {
        use std::os::unix::fs::symlink;
        let (tmp, launch, sibling, other) = setup();
        symlink(&other, sibling.join("escape")).unwrap();
        let state = tmp.path().join("state");
        grant(
            &sibling,
            "approved cleanup",
            "session-a",
            std::slice::from_ref(&launch),
            &state,
        )
        .unwrap();
        let mut roots =
            Roots::fixed(vec![launch], true).with_session_grants(load(&state, "session-a", &[]));
        // Loading against a different launch scope cannot transfer a grant.
        assert!(roots.session_grants.is_empty());
        roots = Roots::fixed(vec![tmp.path().join("launch")], true).with_session_grants(load(
            &state,
            "session-a",
            &[tmp.path().join("launch")],
        ));
        assert!(crate::rm_tool::resolve(
            sibling.join("escape/obsolete.rs").to_str().unwrap(),
            tmp.path(),
            None,
            &mut roots,
        )
        .is_err());
    }

    #[test]
    fn replaced_checkout_does_not_keep_its_grant() {
        let (tmp, launch, sibling, _) = setup();
        let state = tmp.path().join("state");
        grant(
            &sibling,
            "approved cleanup",
            "session-a",
            std::slice::from_ref(&launch),
            &state,
        )
        .unwrap();
        fs::rename(&sibling, tmp.path().join("old-sibling")).unwrap();
        fs::create_dir_all(sibling.join(".git")).unwrap();
        assert!(load(&state, "session-a", &[launch]).is_empty());
    }

    /// macOS temp dirs live under `/var`, a symlink to `/private/var`. `grant`
    /// canonicalizes the sibling, so a launch root reached through a symlink must
    /// still count as its neighbor.
    #[test]
    fn sibling_through_a_symlinked_parent_is_still_eligible() {
        let (tmp, _, _, _) = setup();
        let link = tmp.path().with_extension("link");
        std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
        let launch = link.join("launch");
        let sibling = link.join("sibling");
        let state = tmp.path().join("state");
        grant(
            &sibling,
            "approved cleanup",
            "session-a",
            std::slice::from_ref(&launch),
            &state,
        )
        .unwrap();
        assert_eq!(load(&state, "session-a", &[launch]).len(), 1);
        let _ = fs::remove_file(&link);
    }
}

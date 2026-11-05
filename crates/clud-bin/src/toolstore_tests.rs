use super::*;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

/// A fake `clud` build: distinct bytes give a distinct build id (no ELF note).
fn fake_build(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    let path = dir.join(name).join("clud");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    crate::platform::shim_fs::set_mode(&path, 0o755).unwrap();
    path
}

fn owned_names(home: &Path) -> Vec<PathBuf> {
    OWNED_SUBDIRS
        .iter()
        .flat_map(|subdir| {
            ["clud-cmd-scan", "rm", "python"].map(|name| home.join(subdir).join(name))
        })
        .collect()
}

fn never_alive(_: u32) -> bool {
    false
}

#[test]
fn same_size_builds_get_different_ids() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let old = fake_build(tmp.path(), "a", b"clud build one");
    let new = fake_build(tmp.path(), "b", b"clud build two");
    let (a, b) = (
        build_id(&home, &old).unwrap(),
        build_id(&home, &new).unwrap(),
    );
    assert_ne!(a, b, "same length must not mean same build (#2558)");
    assert!(a.starts_with(env!("CARGO_PKG_VERSION")));
    assert_eq!(build_id(&home, &old).unwrap(), a, "memoized id is stable");
    assert_eq!(sanitize("2.8.50+ab/c d"), "2.8.50+ab_c_d");
}

/// (i) Names pointed at an older build follow the newer launcher; a foreign
/// install is never touched.
#[test]
fn a_newer_launcher_repoints_every_owned_name_and_leaves_foreign_ones() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let old = fake_build(tmp.path(), "old", b"clud old build");
    let new = fake_build(tmp.path(), "new", b"clud new build");
    let foreign = fake_build(tmp.path(), "venv-bin", b"clud pip copy");
    let foreign_before = fs::read(&foreign).unwrap();
    let names = owned_names(&home);
    for name in &names {
        assert!(matches!(
            install_owned(&home, &old, name).unwrap(),
            Placed::Made(_)
        ));
    }
    let old_id = build_id(&home, &old).unwrap();
    let new_id = build_id(&home, &new).unwrap();
    for name in &names {
        assert_eq!(
            resolved_build(&home, name).as_deref(),
            Some(old_id.as_str())
        );
        install_owned(&home, &new, name).unwrap();
    }
    let new_bin = fs::canonicalize(build_bin(&home, &new_id)).unwrap();
    for name in &names {
        assert_eq!(
            resolved_build(&home, name).as_deref(),
            Some(new_id.as_str())
        );
        assert_eq!(
            fs::canonicalize(name).unwrap(),
            new_bin,
            "{}",
            name.display()
        );
        assert_eq!(fs::read(name).unwrap(), b"clud new build");
        assert!(is_current(&home, &new, name));
        assert!(!is_current(&home, &old, name));
        assert_eq!(install_owned(&home, &new, name).unwrap(), Placed::Unchanged);
    }
    // The old build dir is never mutated: rollback stays possible.
    assert_eq!(
        fs::read(build_bin(&home, &old_id)).unwrap(),
        b"clud old build"
    );
    assert_eq!(fs::read(&foreign).unwrap(), foreign_before);
    assert_eq!(resolved_build(&home, &foreign), None);
}

/// (ii) A swap never invalidates a handle a reader already opened, and the
/// name is valid at every point (rename is atomic).
#[test]
fn a_symlink_swap_leaves_an_open_name_readable() {
    use std::io::Read;
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let old = fake_build(tmp.path(), "old", b"clud old build");
    let new = fake_build(tmp.path(), "new", b"clud new build");
    let name = home.join(".clud/state/helper-bin/clud-cmd-scan");
    install_owned(&home, &old, &name).unwrap();
    let mut opened = fs::File::open(&name).unwrap();
    install_owned(&home, &new, &name).unwrap();
    let mut body = Vec::new();
    opened.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"clud old build", "the open handle keeps its build");
    assert_eq!(fs::read(&name).unwrap(), b"clud new build");
    let leftovers: Vec<_> = fs::read_dir(name.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no staging file survives a swap");
}

/// (iii) GC keeps the newest two and any referenced build.
#[test]
fn select_prunable_keeps_newest_two_and_referenced() {
    let at = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
    let builds: Vec<Build> = [("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5)]
        .map(|(id, s)| Build {
            id: id.to_string(),
            published: at(s),
        })
        .to_vec();
    let referenced: BTreeSet<String> = ["a".to_string()].into();
    let mut pruned = select_prunable(&builds, &referenced, KEEP_NEWEST);
    pruned.sort();
    assert_eq!(pruned, ["b", "c"]);
}

#[test]
fn prune_deletes_only_unreferenced_old_builds() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let mut ids = Vec::new();
    for (i, body) in ["v1", "v2", "v3", "v4", "v5"].iter().enumerate() {
        let exe = fake_build(tmp.path(), body, body.as_bytes());
        let id = build_id(&home, &exe).unwrap();
        publish(&home, &exe, &id).unwrap();
        let dir = store_dir(&home).join(&id);
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + i as u64);
        fs::File::open(&dir).unwrap().set_modified(when).unwrap();
        ids.push((id, exe));
    }
    // v1 is still named by a shim; v2 is pinned by a live process.
    install_owned(&home, &ids[0].1, &home.join(".clud/state/shims/rm")).unwrap();
    let pinned = store_dir(&home).join(&ids[1].0).join(PINS_DIR);
    fs::write(pinned.join("4242"), b"").unwrap();
    // Restore publish times touched by the pins / link above.
    for (i, (id, _)) in ids.iter().enumerate() {
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + i as u64);
        fs::File::open(store_dir(&home).join(id))
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
    let alive = |pid: u32| pid == 4242;
    let deleted = prune(&home, None, &alive);
    assert_eq!(deleted, [ids[2].0.clone()]);
    for (i, (id, _)) in ids.iter().enumerate() {
        assert_eq!(build_bin(&home, id).is_file(), i != 2, "{id}");
    }
    // Nothing is referenced by a launcher or pin any more except the name.
    assert!(prune(&home, Some(&ids[3].0), &never_alive).contains(&ids[1].0));
}

/// (iv) The copy fallback still yields a working name and names its build.
#[test]
fn the_copy_fallback_yields_a_working_named_build() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let exe = fake_build(tmp.path(), "a", b"clud copy build");
    let name = home.join(".clud/state/rm-shim/gh");
    assert_eq!(
        install_owned_with(&home, &exe, &name, &[Method::Copy]).unwrap(),
        Placed::Made(Method::Copy)
    );
    assert!(!fs::symlink_metadata(&name)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&name).unwrap(), b"clud copy build");
    let id = build_id(&home, &exe).unwrap();
    assert_eq!(resolved_build(&home, &name), Some(id));
    assert_eq!(
        install_owned_with(&home, &exe, &name, &[Method::Copy]).unwrap(),
        Placed::Unchanged
    );
    if let Some(mode) = crate::platform::shim_fs::mode_bits(&fs::metadata(&name).unwrap()) {
        assert_eq!(mode & 0o111, 0o111);
    }
}

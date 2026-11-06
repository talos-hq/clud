use super::*;
use tempfile::tempdir;

const NOW: u64 = 10_000_000;

fn build(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    let path = dir.join(name).join(crate::shim_registry::file_name("clud"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    path
}

fn old_build(root: &Path, name: &str, used: u64) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join(SESSION_LEAF)).unwrap();
    write_last_used(&dir, used).unwrap();
    dir
}

#[test]
fn two_builds_of_one_version_get_separate_directories() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let one = place(&home, &build(tmp.path(), "a", b"build one")).unwrap();
    let two = place(&home, &build(tmp.path(), "b", b"build two")).unwrap();
    assert_ne!(one, two);
    assert_eq!(fs::read(one.image()).unwrap(), b"build one");
    assert_eq!(fs::read(two.image()).unwrap(), b"build two");
}

#[test]
fn two_installs_of_the_same_bytes_share_a_directory() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let one = place(&home, &build(tmp.path(), "a", b"same")).unwrap();
    let two = place(&home, &build(tmp.path(), "b", b"same")).unwrap();
    assert_eq!(one, two);
}

#[test]
fn the_image_survives_the_install_being_replaced() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let clud = build(tmp.path(), "a", b"old");
    let dir = place(&home, &clud).unwrap();
    let names = vec!["rm".to_string()];
    install_aliases(&dir, &dir.session(), &names, &alias_link::DEFAULT_ORDER).unwrap();

    let staged = clud.with_extension("new");
    fs::write(&staged, b"new").unwrap();
    fs::rename(&staged, &clud).unwrap();

    assert_eq!(fs::read(dir.session().join("rm")).unwrap(), b"old");
}

#[test]
fn a_symlinked_alias_stays_pinned_to_its_build() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let clud = build(tmp.path(), "a", b"old");
    let dir = place(&home, &clud).unwrap();
    let names = vec!["rm".to_string()];
    // Symlinks need a privilege on Windows: only assert when one was made.
    if install_aliases(&dir, &dir.session(), &names, &[Method::Symlink]).is_err() {
        return;
    }
    let staged = clud.with_extension("new");
    fs::write(&staged, b"new").unwrap();
    fs::rename(&staged, &clud).unwrap();
    assert_eq!(fs::read(dir.session().join("rm")).unwrap(), b"old");
}

#[test]
fn a_copied_alias_is_a_complete_executable() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = place(&home, &build(tmp.path(), "a", b"body")).unwrap();
    let names = vec!["rm".to_string()];
    install_aliases(&dir, &dir.session(), &names, &[Method::Copy]).unwrap();
    assert_eq!(fs::read(dir.session().join("rm")).unwrap(), b"body");
}

#[test]
fn a_rebuilt_binary_is_hashed_again_despite_the_memo() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let clud = build(tmp.path(), "a", b"first");
    let first = locate(&home, &clud).unwrap();
    fs::create_dir_all(home.join(ROOT_SUBDIR)).unwrap();
    assert_eq!(locate(&home, &clud).unwrap(), first);
    fs::write(&clud, b"second build").unwrap();
    assert_ne!(locate(&home, &clud).unwrap(), first);
}

#[test]
fn sweep_removes_only_a_stale_unleased_build() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let current = old_build(root, "v1-current", 0);
    let stale = old_build(root, "v1-stale", 0);
    let fresh = old_build(root, "v1-fresh", NOW - 1);
    let leased = old_build(root, "v1-leased", 0);
    let lease = open_lock_file(&leased.join(LEASE_FILE)).unwrap();
    FileExt::lock_shared(&lease).unwrap();

    let summary = sweep(root, &current, NOW, STALE_SECONDS).unwrap();

    assert_eq!(
        summary,
        GcSummary {
            removed: 1,
            leased: 1,
            fresh: 1
        }
    );
    assert!(!stale.exists());
    assert!(current.exists() && fresh.exists() && leased.exists());
    drop(lease);
}

#[test]
fn a_dead_process_releases_its_lease() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let dir = old_build(root, "v1-old", 0);
    let lease = open_lock_file(&dir.join(LEASE_FILE)).unwrap();
    FileExt::lock_shared(&lease).unwrap();
    assert!(is_leased(&dir));
    drop(lease);
    assert!(!is_leased(&dir));
}

#[test]
fn a_build_with_no_ledger_is_stamped_not_removed() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let dir = root.join("v1-unstamped");
    fs::create_dir_all(&dir).unwrap();
    let summary = sweep(root, Path::new("/none"), NOW, STALE_SECONDS).unwrap();
    assert_eq!(summary.fresh, 1);
    assert_eq!(read_last_used(&dir), Some(NOW));
}

#[test]
fn sweep_collects_an_interrupted_delete_and_ignores_other_entries() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let interrupted = root.join(".gc-v1-old-7");
    fs::create_dir_all(&interrupted).unwrap();
    let memo = root.join(HASH_MEMO_DIR);
    fs::create_dir_all(&memo).unwrap();
    sweep(root, Path::new("/none"), NOW, STALE_SECONDS).unwrap();
    assert!(!interrupted.exists());
    assert!(memo.exists());
}

#[test]
fn the_sweep_runs_at_most_once_per_interval() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    assert!(sweep_if_due(root, Path::new("/none"), NOW)
        .unwrap()
        .is_some());
    assert!(sweep_if_due(root, Path::new("/none"), NOW + 1)
        .unwrap()
        .is_none());
    assert!(sweep_if_due(root, Path::new("/none"), NOW + GC_INTERVAL_SECONDS)
        .unwrap()
        .is_some());
}

#[test]
fn every_builds_alias_dirs_are_listed_and_recognized() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let root = home.join(ROOT_SUBDIR);
    fs::create_dir_all(&root).unwrap();
    let one = old_build(&root, "v1-one", NOW).join(SESSION_LEAF);
    let two = old_build(&root, "v2-two", NOW).join(SESSION_LEAF);
    assert_eq!(alias_dirs_named(home, SESSION_LEAF), vec![one.clone(), two]);
    assert!(is_alias_dir_named(home, &one, SESSION_LEAF));
    assert!(!is_alias_dir_named(home, &one, HELPER_LEAF));
    assert!(!is_alias_dir_named(
        home,
        Path::new("/elsewhere/v1/rm-shim"),
        SESSION_LEAF
    ));
}

// Wiring: the callers of this module.

#[test]
fn session_aliases_of_two_builds_never_clobber_each_other() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let one = build(tmp.path(), "a", b"build one");
    let two = build(tmp.path(), "b", b"build two");
    let dir_one = crate::shim_install::install_rm_at(&home, &one).unwrap();
    let dir_two = crate::shim_install::install_rm_at(&home, &two).unwrap();
    assert_ne!(dir_one, dir_two);
    let rm = crate::shim_registry::file_name("rm");
    assert_eq!(fs::read(dir_one.join(&rm)).unwrap(), b"build one");
    assert_eq!(fs::read(dir_two.join(&rm)).unwrap(), b"build two");
    assert!(
        !home.join(crate::shim_registry::SESSION_SUBDIR).exists(),
        "a launch must not write the legacy shared dir"
    );
}

#[test]
fn interpreter_aliases_live_in_the_build_directory() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let clud = build(tmp.path(), "a", b"body");
    crate::shim_install::extract_shims_at(&home, &clud).unwrap();
    let dir = locate(&home, &clud).unwrap();
    let python = crate::shim_registry::file_name("python");
    assert_eq!(fs::read(dir.interpreter().join(python)).unwrap(), b"body");
    assert!(!home.join(crate::shim_registry::INTERPRETER_SUBDIR).exists());
}

#[test]
fn another_builds_alias_is_never_the_real_program() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let old = build(tmp.path(), "old", b"old build");
    let new = build(tmp.path(), "new", b"new build");
    let old_dir = crate::shim_install::install_rm_at(&home, &old).unwrap();
    let dirs = crate::shim_registry::shim_dirs(&new, None, Some(&home));
    let old_gh = old_dir.join(crate::shim_registry::file_name("gh"));
    assert!(
        !crate::shim_registry::valid_target(&old_gh, &new, &dirs),
        "{old_gh:?} is an alias from an older build, not gh"
    );
}

#[test]
fn another_builds_alias_is_not_a_real_program_for_clud_spawns() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let old = build(tmp.path(), "old", b"old build");
    let old_dir = crate::shim_install::install_rm_at(&home, &old).unwrap();
    let old_git = old_dir.join(crate::shim_registry::file_name("git"));
    let var = |key: &str| {
        (key == crate::shim_registry::GIT_TARGET_KEY).then(|| old_git.clone().into_os_string())
    };
    assert_eq!(
        crate::shim_registry::real_program_with("git", &var, Some(&home)),
        None
    );
}

#[test]
fn a_session_mismatch_is_reported() {
    let tmp = tempdir().unwrap();
    let active = tmp.path().join("v2-new/rm-shim");
    let session = tmp.path().join("v1-old/rm-shim");
    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&session).unwrap();
    assert!(crate::doctor::session_mismatch(Some(&session), &active).is_some());
    assert!(crate::doctor::session_mismatch(Some(&active), &active).is_none());
    assert!(crate::doctor::session_mismatch(None, &active).is_none());
}

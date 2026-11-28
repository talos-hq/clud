//! Issue #1485 acceptance 1, 2 and the size backstop. Every test works under
//! a `tempdir` home; none touches the real `~/.clud` or `~/dev`.

use super::*;
use crate::gc::session_tmp::{self, session_tmp_dir_for};
use kernal_api::platform::fs::TemporaryDirectory;
use std::time::{Duration, SystemTime};

fn age_path(path: &Path, age: Duration) {
    let when = SystemTime::now() - age;
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when)).unwrap();
}

/// Acceptance 1: the root is `<home>/.clud/worktrees` and is not under the
/// `session_tmp` root.
#[test]
fn worktree_root_is_clud_home_tmp_wt_and_not_under_session_tmp() {
    let home = TemporaryDirectory::new().unwrap();
    let root = worktree_root_for(home.path());
    assert_eq!(root, home.path().join(".clud").join("worktrees"));
    let tmp = session_tmp_dir_for(home.path());
    assert!(
        !root.starts_with(&tmp),
        "{} must not sit under {}",
        root.display(),
        tmp.display()
    );
    assert_eq!(root.parent(), tmp.parent(), "worktrees is a sibling of tmp");
}

/// Acceptance 1: the root exists after the start-up call, and the call is
/// idempotent (a second start with worktrees inside leaves them alone).
#[test]
fn ensure_worktree_root_creates_it_idempotently() {
    let home = TemporaryDirectory::new().unwrap();
    let root = ensure_worktree_root_at(home.path()).unwrap();
    assert!(root.is_dir());
    let child = root.join("repo-wt-1");
    fs::create_dir_all(&child).unwrap();
    assert_eq!(ensure_worktree_root_at(home.path()).unwrap(), root);
    assert!(child.is_dir(), "a second start must not disturb worktrees");
}

/// Acceptance 2: the `session_tmp` sweep, given a `worktrees/<repo>-wt-1`
/// entry far past the stale threshold, never touches it, because the root is
/// outside its scope.
#[test]
fn session_tmp_sweep_never_touches_tmp_wt() {
    let home = TemporaryDirectory::new().unwrap();
    let tmp = session_tmp_dir_for(home.path());
    fs::create_dir_all(&tmp).unwrap();
    let wt = ensure_worktree_root_at(home.path())
        .unwrap()
        .join("repo-wt-1");
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join("f.txt"), "work").unwrap();
    let ancient = Duration::from_secs(365 * 24 * 3600);
    age_path(&wt.join("f.txt"), ancient);
    age_path(&wt, ancient);
    // A stale sibling inside tmp proves the sweep really ran.
    let stale = tmp.join("stale");
    fs::create_dir_all(&stale).unwrap();
    age_path(&stale, ancient);

    session_tmp::sweep_stale_at(&tmp, SystemTime::now(), false).unwrap();

    assert!(!stale.exists(), "the sweep must have run over tmp");
    assert!(wt.join("f.txt").exists(), "worktrees is outside the sweep");
}

#[test]
fn under_root_matches_children_only() {
    let home = TemporaryDirectory::new().unwrap();
    let root = ensure_worktree_root_at(home.path()).unwrap();
    let child = root.join("repo-wt-2");
    fs::create_dir_all(&child).unwrap();
    assert!(is_under_worktree_root(&child, &root));
    assert!(
        !is_under_worktree_root(&root, &root),
        "never the root itself"
    );
    assert!(!is_under_worktree_root(home.path(), &root));
}

#[test]
fn size_check_is_under_over_or_unknown() {
    let dir = TemporaryDirectory::new().unwrap();
    fs::create_dir_all(dir.path().join("a/b")).unwrap();
    fs::write(dir.path().join("a/b/x"), vec![0u8; 100]).unwrap();
    fs::write(dir.path().join("a/y"), vec![0u8; 50]).unwrap();
    assert_eq!(
        check_tree_size(dir.path(), 1_000, 100),
        SizeCheck::Under(150)
    );
    assert!(matches!(
        check_tree_size(dir.path(), 120, 100),
        SizeCheck::Over(n) if n > 120
    ));
    assert_eq!(check_tree_size(dir.path(), 1_000, 1), SizeCheck::Unknown);
    assert_eq!(
        check_tree_size(&dir.path().join("missing"), 10, 10),
        SizeCheck::Under(0)
    );
}

#[test]
fn size_warning_only_when_over_or_unknown_and_enabled() {
    let root = Path::new("/r");
    assert_eq!(size_warning(root, 10, SizeCheck::Under(5)), None);
    assert!(size_warning(root, 10, SizeCheck::Over(11))
        .unwrap()
        .contains("never deleted for size"));
    assert!(size_warning(root, 10, SizeCheck::Unknown).is_some());
    assert_eq!(size_warning(root, 0, SizeCheck::Over(11)), None);
}

/// The root's own directory name is never mistaken for a sibling clone.
/// (Entries under the root are found by listing it, not by name shape.)
#[test]
fn the_root_is_not_a_sibling_clone_name() {
    assert!(!crate::gc::reconcile::is_sibling_clone_dir_name(
        "clud2",
        WORKTREE_ROOT_DIR_NAME
    ));
}

// ---- #1486: the ordinal allocator. ----

#[test]
fn alloc_takes_the_plain_name_first_and_it_exists() {
    let home = TemporaryDirectory::new().unwrap();
    let root = worktree_root_for(home.path());
    let path = alloc_wt_path_in(&root, "clud", "432").unwrap();
    assert!(path.is_dir(), "returned path must exist on return");
    assert_eq!(path, root.join("clud-wt-432"));
}

/// #1486 acceptance 6 at the allocator: consecutive calls get `clud-wt-432`,
/// `clud-wt-432-2`, `clud-wt-432-3`, and each exists the moment it is returned.
#[test]
fn alloc_collisions_take_the_next_ordinal_and_every_path_exists() {
    let home = TemporaryDirectory::new().unwrap();
    let root = worktree_root_for(home.path());
    let mut got = Vec::new();
    for _ in 0..3 {
        let path = alloc_wt_path_in(&root, "clud", "432").unwrap();
        assert!(path.exists(), "{} must exist on return", path.display());
        got.push(path);
    }
    assert_eq!(
        got,
        [
            root.join("clud-wt-432"),
            root.join("clud-wt-432-2"),
            root.join("clud-wt-432-3")
        ]
    );
}

/// A pre-existing plain file counts as taken, and a gap is reused lowest
/// ordinal first.
#[test]
fn alloc_skips_a_file_and_fills_a_gap() {
    let home = TemporaryDirectory::new().unwrap();
    let root = ensure_worktree_root_at(home.path()).unwrap();
    fs::write(root.join("clud-wt-7"), b"not a dir").unwrap();
    fs::create_dir(root.join("clud-wt-7-3")).unwrap();
    assert_eq!(
        alloc_wt_path_in(&root, "clud", "7").unwrap(),
        root.join("clud-wt-7-2")
    );
    assert_eq!(
        alloc_wt_path_in(&root, "clud", "7").unwrap(),
        root.join("clud-wt-7-4")
    );
}

#[test]
fn concurrent_allocations_never_share_a_path() {
    let home = TemporaryDirectory::new().unwrap();
    let root = worktree_root_for(home.path());
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || alloc_wt_path_in(&root, "clud", "1").unwrap())
        })
        .collect();
    let mut paths: Vec<PathBuf> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(paths.iter().all(|p| p.is_dir()));
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 8, "every caller must get its own directory");
}

#[test]
fn alloc_rejects_components_that_would_escape_the_root() {
    let home = TemporaryDirectory::new().unwrap();
    let root = worktree_root_for(home.path());
    for (slug, suffix) in [
        ("", "1"),
        ("clud", ""),
        ("..", "1"),
        (".", "1"),
        ("clud", ".."),
        ("a/b", "1"),
        ("a\\b", "1"),
        ("clud", "1/../../x"),
        ("c:", "1"),
        ("clud", "a\nb"),
    ] {
        let err = alloc_wt_path_in(&root, slug, suffix).unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{slug:?} {suffix:?}"
        );
    }
    assert!(
        !root.exists() || fs::read_dir(&root).unwrap().next().is_none(),
        "a rejected call must reserve nothing"
    );
}

// ---- #1962: one constructor for every path under the root. ----

#[test]
fn every_worktree_path_is_repo_wt_name() {
    let root = Path::new("/h/.clud/worktrees");
    for (repo, name, ordinal, expected) in [
        ("clud", "1962", 1, "clud-wt-1962"),
        ("clud", "PR1234", 1, "clud-wt-PR1234"),
        ("clud", "1962", 2, "clud-wt-1962-2"),
        // A name that already carries the prefix is not prefixed twice.
        ("clud", "clud-wt-1962", 1, "clud-wt-1962"),
        // A bare `wt-` or `<repo>-` prefix is not repeated either.
        ("clud", "wt-7", 1, "clud-wt-7"),
        ("clud", "clud-7", 1, "clud-wt-7"),
        // Branch-style names become one plain component.
        ("clud", "feat/x y", 1, "clud-wt-feat-x-y"),
        ("my repo", "a", 1, "my-repo-wt-a"),
        // No name: the repo alone.
        ("clud", "", 1, "clud"),
    ] {
        assert_eq!(
            worktree_path_in(root, repo, name, ordinal),
            root.join(expected),
            "{repo:?} {name:?} {ordinal}"
        );
    }
}

#[test]
fn the_suggestion_names_the_first_free_path_and_creates_nothing() {
    let home = TemporaryDirectory::new().unwrap();
    let root = ensure_worktree_root_at(home.path()).unwrap();
    fs::create_dir(root.join("clud-wt-1962")).unwrap();
    let path = suggest_wt_path_in(&root, "clud", "1962");
    assert_eq!(path, root.join("clud-wt-1962-2"));
    assert!(!path.exists());
}

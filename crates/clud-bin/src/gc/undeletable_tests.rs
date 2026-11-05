use super::*;

fn denied() -> io::Error {
    io::Error::from(io::ErrorKind::PermissionDenied)
}

#[test]
fn empty_has_no_summary() {
    assert_eq!(Undeletable::default().summary_line(), None);
}

#[test]
fn thousands_of_denials_become_one_line_with_an_example() {
    let mut u = Undeletable::default();
    for i in 0..2000 {
        u.record(Path::new(&format!("/no/such/root-owned/{i}")), &denied());
    }
    let line = u.summary_line().unwrap();
    assert!(!line.contains('\n'), "{line}");
    assert!(
        line.starts_with(
            "2000 paths could not be removed (permission denied), e.g. /no/such/root-owned/0"
        ),
        "{line}"
    );
}

#[test]
fn singular_noun_for_one_path() {
    let mut u = Undeletable::default();
    u.record(Path::new("/x"), &denied());
    assert!(u.summary_line().unwrap().starts_with("1 path could"));
}

#[test]
fn mixed_failures_are_counted_separately() {
    let mut u = Undeletable::default();
    u.record(Path::new("/a"), &denied());
    u.record(Path::new("/b"), &io::Error::other("busy"));
    let line = u.summary_line().unwrap();
    assert!(line.contains("2 paths"), "{line}");
    assert!(line.contains("1 permission denied, 1 other"), "{line}");
    assert!(line.contains("/b: busy"), "{line}");
}

#[test]
fn merge_sums_and_keeps_the_earliest_example() {
    let mut a = Undeletable::default();
    a.record(Path::new("/first"), &denied());
    let mut b = Undeletable::default();
    b.record(Path::new("/second"), &denied());
    a.merge(&b);
    let line = a.summary_line().unwrap();
    assert!(
        line.starts_with("2 paths") && line.contains("/first"),
        "{line}"
    );
}

#[test]
fn a_root_owned_path_gets_the_native_gc_remediation_hint() {
    // `/proc/1` is root-owned on Linux; skip where it is not available.
    let Some(meta) = std::fs::symlink_metadata("/").ok() else {
        return;
    };
    if crate::platform::rm_tool::file_owner(&meta) != Some(0) {
        return;
    }
    let mut u = Undeletable::default();
    u.record(Path::new("/"), &denied());
    assert!(u
        .summary_line()
        .unwrap()
        .ends_with("owned by root \u{2014} restore ownership, then retry clud gc"));
}

#[test]
fn collecting_remover_clears_trees_files_and_missing_paths_without_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir_all(tree.join("a/b")).unwrap();
    std::fs::write(tree.join("a/b/f"), b"x").unwrap();
    let mut failures = Undeletable::default();
    for path in [&tree, &tmp.path().join("missing")] {
        crate::rm_tool::remove_dir_tree_collecting(path, &mut |p, e| failures.record(p, e));
    }
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!tree.exists());
}

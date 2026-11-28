use super::super::tests::{args, world, World};
use super::super::{parse_args, run_with, Kind, Roots};
use super::*;

fn run(w: &World, roots: &mut Roots, list: &[&str]) -> (i32, String) {
    let options = parse_args(&args(list)).unwrap().unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(Kind::Safe, &options, &w.ctx(), roots, &mut out, &mut err);
    (code, String::from_utf8(err).unwrap())
}

/// Trash `name` from the world's root as session-1; returns its entry dir.
fn trashed(w: &World, name: &str) -> PathBuf {
    let file = w.root.join(name);
    std::fs::write(&file, "x").unwrap();
    let before = w.entries();
    let (code, err) = run(w, &mut w.roots(), &[file.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let entry = w
        .entries()
        .into_iter()
        .find(|e| !before.contains(e))
        .unwrap();
    crate::path_norm::canonicalize_plain(&entry).unwrap()
}

fn trash_root(entry: &Path) -> PathBuf {
    entry.parent().unwrap().to_path_buf()
}

#[test]
fn own_entry_is_purged_and_leaves_nothing_behind() {
    let w = world();
    let entry = trashed(&w, "scratch.txt");
    let own = load(&trash_root(&entry), "session-1");
    assert_eq!(own, vec![entry.clone()]);
    let mut roots = w.roots().with_own_trash(own);
    let (code, err) = run(&w, &mut roots, &["-r", "--purge", entry.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(!entry.exists());
    assert!(w.entries().is_empty(), "{:?}", w.entries());
}

#[test]
fn own_entry_without_purge_is_refused_and_not_retrashed() {
    let w = world();
    let entry = trashed(&w, "scratch.txt");
    let mut roots = w
        .roots()
        .with_own_trash(load(&trash_root(&entry), "session-1"));
    let (code, err) = run(&w, &mut roots, &["-r", entry.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(err.contains("--purge"), "{err}");
    assert!(entry.exists());
    assert_eq!(w.entries().len(), 1);
}

#[test]
fn another_sessions_entry_stays_refused() {
    let w = world();
    let entry = trashed(&w, "scratch.txt");
    assert!(load(&trash_root(&entry), "session-2").is_empty());
    let mut roots = w
        .roots()
        .with_own_trash(load(&trash_root(&entry), "session-2"));
    let (code, err) = run(&w, &mut roots, &["-r", "--purge", entry.to_str().unwrap()]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("outside the allowed roots"), "{err}");
    assert!(entry.exists());
}

#[test]
fn clud_trash_owner_marker_is_recognized() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let dir = tmp.path().join("20261024-abc123");
    std::fs::create_dir(&dir).unwrap();
    write_owner(&dir, "session-9");
    assert_eq!(load(tmp.path(), "session-9").len(), 1);
    assert!(load(tmp.path(), "session-1").is_empty());
}

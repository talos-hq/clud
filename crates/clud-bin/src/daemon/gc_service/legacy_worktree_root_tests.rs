//! #1962: worktrees under the old `~/.clud/tmp-wt` move to
//! `~/.clud/worktrees`. Every test works under a tempdir home.

use super::super::extern_repo::probe_cmd;
use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::time::Duration;

fn run_git(cwd: &Path, args: &[&str]) -> String {
    match probe_cmd("git", cwd, args, Duration::from_secs(30)) {
        Some((0, out)) => out,
        other => panic!("git {} failed: {other:?}", args.join(" ")),
    }
}

/// A repo with one commit, outside the clud home.
fn repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_git(&repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.email", "t@example.com"),
        ("user.name", "t"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", ""),
    ] {
        run_git(&repo, &["config", key, value]);
    }
    std::fs::write(repo.join("a.txt"), "a").unwrap();
    run_git(&repo, &["add", "a.txt"]);
    run_git(&repo, &["commit", "-q", "-m", "a"]);
    repo
}

fn legacy(home: &Path) -> PathBuf {
    crate::gc::worktree_root::legacy_worktree_root_for(home)
}

fn new_root(home: &Path) -> PathBuf {
    crate::gc::worktree_root::worktree_root_for(home)
}

#[test]
fn an_idle_worktree_moves_and_git_follows_it() {
    let tmp = TemporaryDirectory::new().unwrap();
    let home = tmp.path().join("home");
    let repo = repo(tmp.path());
    let old = legacy(&home).join("repo-wt-a");
    std::fs::create_dir_all(legacy(&home)).unwrap();
    run_git(
        &repo,
        &["worktree", "add", "-q", old.to_str().unwrap(), "-b", "a"],
    );
    std::fs::write(old.join("unpushed.txt"), "work").unwrap();

    let log = migrate(&home, &[]);

    let moved = new_root(&home).join("repo-wt-a");
    assert!(moved.join("unpushed.txt").exists(), "{log:?}");
    assert!(!old.exists(), "{log:?}");
    let list = run_git(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        list.lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|path| crate::path_norm::same_path(Path::new(path), &moved)),
        "git still records the old path: {list}"
    );
    assert_eq!(
        run_git(&moved, &["status", "--porcelain"]).trim(),
        "?? unpushed.txt"
    );
    assert!(!legacy(&home).exists(), "the empty old root is removed");
}

#[test]
fn a_worktree_a_live_process_works_in_stays_until_it_is_idle() {
    let tmp = TemporaryDirectory::new().unwrap();
    let home = tmp.path().join("home");
    let busy = legacy(&home).join("repo-wt-busy");
    std::fs::create_dir_all(busy.join("src")).unwrap();

    migrate(&home, &[busy.join("src")]);
    assert!(busy.exists(), "a live session's directory is never moved");
    assert!(legacy(&home).exists());

    migrate(&home, &[]);
    assert!(!busy.exists());
    assert!(new_root(&home).join("repo-wt-busy").join("src").exists());
}

#[test]
fn other_entries_are_renamed_and_a_taken_name_gets_a_suffix() {
    let tmp = TemporaryDirectory::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(legacy(&home).join("repo-wt-x")).unwrap();
    std::fs::write(legacy(&home).join("repo-wt-x").join("f"), "1").unwrap();
    std::fs::create_dir_all(new_root(&home).join("repo-wt-x")).unwrap();

    migrate(&home, &[]);

    assert!(new_root(&home).join("repo-wt-x-2").join("f").exists());
    assert!(
        new_root(&home).join("repo-wt-x").exists(),
        "the existing entry is untouched"
    );
}

#[test]
fn without_an_old_root_nothing_happens() {
    let tmp = TemporaryDirectory::new().unwrap();
    assert!(migrate(tmp.path(), &[]).is_empty());
    assert!(!new_root(tmp.path()).exists(), "nothing is created");
}

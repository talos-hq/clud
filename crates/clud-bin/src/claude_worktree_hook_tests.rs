//! #1962/#1971: Claude Code's worktrees are built under clud's worktree root.

use super::*;
use crate::worktrees::run_git;
use serde_json::json;

fn git(cwd: &Path, args: &[&str]) -> String {
    run_git(cwd, args)
        .unwrap_or_else(|e| panic!("git {} in {}: {e}", args.join(" "), cwd.display()))
}

/// A repo with one commit on `main` and an `origin` whose default is `main`.
fn repo_with_origin(root: &Path) -> PathBuf {
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--initial-branch=main"]);
    for (key, value) in [
        ("user.email", "t@example.com"),
        ("user.name", "t"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", ""),
    ] {
        git(&origin, &["config", key, value]);
    }
    std::fs::write(origin.join("a.txt"), "a\n").unwrap();
    git(&origin, &["add", "a.txt"]);
    git(&origin, &["commit", "-q", "-m", "base"]);
    let repo = root.join("myrepo");
    git(
        root,
        &[
            "clone",
            "-q",
            &origin.to_string_lossy(),
            &repo.to_string_lossy(),
        ],
    );
    repo
}

#[test]
fn payloads_parse() {
    let request =
        parse_create(r#"{"cwd": "/r", "name": "bold-oak-a3f2", "session_id": "s"}"#).unwrap();
    assert_eq!(
        request,
        CreateRequest {
            cwd: PathBuf::from("/r"),
            name: "bold-oak-a3f2".into()
        }
    );
    assert!(parse_create(r#"{"name": "x"}"#).is_err());
    assert!(parse_create("not json").is_err());
    assert_eq!(
        parse_remove(r#"{"worktree_path": "/w"}"#).unwrap(),
        PathBuf::from("/w")
    );
    assert!(parse_remove("{}").is_err());
}

#[test]
fn names_become_safe_path_components() {
    assert_eq!(sanitize_component("agent-a1b2c3"), "agent-a1b2c3");
    assert_eq!(sanitize_component("feat/x y"), "feat-x-y");
    assert_eq!(sanitize_component("../.."), "worktree");
    assert_eq!(sanitize_component(""), "worktree");
    assert_eq!(sanitize_component(&"a".repeat(200)).len(), 64);
}

#[test]
fn base_ref_follows_the_first_setting_in_precedence_order() {
    assert_eq!(base_ref_from(&[]), BaseRef::Fresh);
    assert_eq!(
        base_ref_from(&[json!({"worktree": {"baseRef": "head"}})]),
        BaseRef::Head
    );
    assert_eq!(
        base_ref_from(&[
            json!({}),
            json!({"worktree": {"baseRef": "fresh"}}),
            json!({"worktree": {"baseRef": "head"}})
        ]),
        BaseRef::Fresh
    );
}

#[test]
fn branch_names_skip_ones_already_taken() {
    let names: Vec<String> = branch_candidates("x").take(3).collect();
    assert_eq!(names, ["worktree-x", "worktree-x-2", "worktree-x-3"]);
}

#[test]
fn a_claude_code_worktree_lands_under_the_worktree_root() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let repo = repo_with_origin(tmp.path());
    let wt_root = tmp.path().join("worktrees");
    let request = CreateRequest {
        cwd: repo.clone(),
        name: "agent-a1".into(),
    };

    let path = create_in(&request, &wt_root, BaseRef::Fresh).expect("created");
    assert_eq!(path, wt_root.join("myrepo-wt-agent-a1"));
    assert!(path.join("a.txt").is_file(), "checked out from origin/main");
    assert_eq!(
        git(&path, &["branch", "--show-current"]).trim(),
        "worktree-agent-a1"
    );
    assert!(!repo.join(".claude").join("worktrees").exists());

    // The same name again gets the next directory and branch.
    let again = create_in(&request, &wt_root, BaseRef::Head).expect("created again");
    assert_eq!(again, wt_root.join("myrepo-wt-agent-a1-2"));
    assert_eq!(
        git(&again, &["branch", "--show-current"]).trim(),
        "worktree-agent-a1-2"
    );
}

#[test]
fn creation_outside_a_repository_fails_with_a_reason() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let request = CreateRequest {
        cwd: tmp.path().to_path_buf(),
        name: "x".into(),
    };
    assert!(create_in(&request, &tmp.path().join("wt"), BaseRef::Fresh).is_err());
}

#[test]
fn removal_accepts_the_new_root_and_the_old_one_until_it_drains() {
    let home = Path::new("/home/u");
    assert!(removable(
        Path::new("/home/u/.clud/worktrees/repo-wt-a"),
        home
    ));
    assert!(removable(Path::new("/home/u/.clud/tmp-wt/repo-wt-a"), home));
    assert!(!removable(Path::new("/repo/.claude/worktrees/a"), home));
    assert!(!removable(Path::new("/home/u/.clud/tmp/x"), home));
}

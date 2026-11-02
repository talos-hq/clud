use super::*;

pub(crate) fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn msys_drive_paths_normalize_without_touching_unc_paths() {
    assert_eq!(msys_drive_path("/c/work/file"), Some("C:/work/file".into()));
    assert_eq!(msys_drive_path("/d"), Some("D:/".into()));
    assert_eq!(msys_drive_path("//server/share"), None);
    assert_eq!(msys_drive_path("/home/user"), None);
}

pub(crate) struct World {
    _tmp: tempfile::TempDir,
    pub(crate) root: PathBuf,
    trash: PathBuf,
    audit: PathBuf,
    pub(crate) home: PathBuf,
}

pub(crate) fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let base = crate::path_norm::canonicalize_plain(tmp.path()).unwrap();
    let root = base.join("repo");
    let home = base.join("home");
    for dir in [&root, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    World {
        trash: base.join("trash"),
        audit: base.join("audit"),
        root,
        home,
        _tmp: tmp,
    }
}

impl World {
    pub(crate) fn ctx(&self) -> Context {
        Context {
            cwd: self.root.clone(),
            home: Some(self.home.clone()),
            trash_root: self.trash.clone(),
            audit_dir: Some(self.audit.clone()),
            session_id: Some("session-1".into()),
            role: "agent".into(),
            register: false,
            now: SystemTime::now(),
        }
    }

    pub(crate) fn roots(&self) -> Roots {
        Roots::fixed(vec![self.root.clone()], true)
    }

    pub(crate) fn run(&self, kind: Kind, list: &[&str]) -> (i32, String, String) {
        let options = parse_args(&args(list)).unwrap().unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with(
            kind,
            &options,
            &self.ctx(),
            &mut self.roots(),
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    pub(crate) fn entries(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.trash)
            .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn audit_records(&self) -> Vec<serde_json::Value> {
        std::fs::read_dir(&self.audit)
            .map(|d| {
                d.filter_map(Result::ok)
                    .flat_map(|e| {
                        std::fs::read_to_string(e.path())
                            .unwrap()
                            .lines()
                            .map(|l| serde_json::from_str(l).unwrap())
                            .collect::<Vec<_>>()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[test]
fn argv0_names_select_the_command() {
    assert_eq!(Kind::from_program_name("safe-rm"), Some(Kind::Safe));
    assert_eq!(Kind::from_program_name("safe-rm.exe"), Some(Kind::Safe));
    assert_eq!(Kind::from_program_name(concat!("rm", "-file")), None);
    assert_eq!(Kind::from_program_name(concat!("rm", "-dir")), None);
    assert_eq!(Kind::from_program_name("rm"), None);
    assert_eq!(Kind::from_program_name("clud"), None);
}

#[test]
fn only_three_flags_parse() {
    let parsed = parse_args(&args(&[
        "--purge",
        "--tracked",
        "--dry-run",
        "--",
        "a",
        "-b",
    ]))
    .unwrap()
    .unwrap();
    assert!(parsed.purge && parsed.tracked && parsed.dry_run);
    assert_eq!(parsed.paths, args(&["a", "-b"]));
    // After the first path, everything is a path: a file named `--purge`
    // from a glob cannot switch the call to purge.
    let later = parse_args(&args(&["a", "--purge", "--"])).unwrap().unwrap();
    assert!(!later.purge);
    assert_eq!(later.paths, args(&["a", "--purge", "--"]));
    assert_eq!(parse_args(&args(&["--help"])).unwrap(), None);
    let compatible = parse_args(&args(&["-Rfv", "x"])).unwrap().unwrap();
    assert!(compatible.recursive && compatible.force && compatible.verbose);
    assert!(parse_args(&args(&["--force", "x"])).unwrap().unwrap().force);
    assert!(parse_args(&args(&["--purge"])).is_err(), "no paths");
    assert!(parse_args(&args(&["-f"]))
        .unwrap()
        .unwrap()
        .paths
        .is_empty());
}

#[test]
fn roots_come_from_the_env_or_the_git_checkout() {
    let w = world();
    let nested = w.root.join("src/deep");
    std::fs::create_dir_all(&nested).unwrap();
    // No env: the nearest checkout, found by its `.git`.
    std::fs::create_dir_all(w.root.join(".git")).unwrap();
    let roots = Roots::resolve(None, &nested);
    assert_eq!(roots.roots, vec![w.root.clone()]);
    assert!(!roots.from_env);
    // Outside a repo: the cwd.
    let roots = Roots::resolve(None, &w.home);
    assert_eq!(roots.roots, vec![w.home.clone()]);
    // The env wins, relative and missing entries are dropped.
    let value =
        std::env::join_paths([w.home.clone(), PathBuf::from("rel"), w.root.join("missing")])
            .unwrap();
    let roots = Roots::resolve(Some(&value), &nested);
    assert_eq!(roots.roots, vec![w.home.clone()]);
    assert!(roots.from_env);
    // Set but empty is treated as unset.
    assert!(!Roots::resolve(Some(OsStr::new("")), &nested).from_env);
}

#[test]
fn refuses_roots_home_outside_and_git_metadata() {
    let w = world();
    std::fs::create_dir_all(w.root.join(".git")).unwrap();
    let notes = w.home.parent().unwrap().join("notes");
    std::fs::write(&notes, b"x").unwrap();
    let mut roots = w.roots();
    let refused =
        |raw: &str, roots: &mut Roots| resolve(raw, &w.root, Some(&w.home), roots).unwrap_err();
    assert!(refused(w.root.to_str().unwrap(), &mut roots).contains("root itself"));
    assert!(refused(".", &mut roots).contains("name it directly"));
    assert!(refused("sub/.", &mut roots).contains("name it directly"));
    assert!(refused("./", &mut roots).contains("name it directly"));
    assert!(refused(".git/objects", &mut roots).contains("git metadata"));
    assert!(refused(".GIT/HEAD", &mut roots).contains("git metadata"));
    assert!(refused("sub/..", &mut roots).contains("name it directly"));
    assert!(refused("/", &mut roots).contains("name it directly"));
    assert!(refused(".git", &mut roots).contains("git metadata"));
    assert!(refused(notes.to_str().unwrap(), &mut roots).contains("outside"));
    assert!(refused("", &mut roots).contains("empty"));
    // Home and its ancestors are never roots, however the session started.
    let value = std::env::join_paths([w.home.clone(), w.root.clone()]).unwrap();
    let from_home = Roots::resolve_with_home(Some(&value), &w.root, Some(&w.home));
    assert_eq!(from_home.roots, vec![w.root.clone()]);
    let parent = w.home.parent().unwrap().to_path_buf();
    assert!(Roots::resolve_with_home(None, &parent, Some(&w.home))
        .roots
        .is_empty());
    // Home and its ancestors are refused even when a root contains them.
    let mut wide = Roots::fixed(vec![w.home.parent().unwrap().to_path_buf()], true);
    assert!(
        resolve(w.home.to_str().unwrap(), &w.root, Some(&w.home), &mut wide)
            .unwrap_err()
            .contains("home")
    );
    // A missing path resolves as missing, not as an error.
    assert!(matches!(
        resolve("nope/deeper", &w.root, Some(&w.home), &mut roots),
        Ok(Resolved::Missing(path)) if path == w.root.join("nope/deeper")
    ));
    // ...but only inside the roots, even when its parent is missing too
    // (Windows turns `/etc/passwd` into `C:\etc\passwd`, whose parent is absent).
    let outside = w.home.join("no-such-dir").join("file");
    assert!(resolve(
        outside.to_str().unwrap(),
        &w.root,
        Some(&w.home),
        &mut roots
    )
    .unwrap_err()
    .contains("outside the allowed roots"));
}

#[test]
fn trash_by_default_keeps_paths_relative_to_their_root_with_a_manifest() {
    let w = world();
    std::fs::create_dir_all(w.root.join("build/obj")).unwrap();
    std::fs::write(w.root.join("build/obj/a.o"), b"obj").unwrap();
    std::fs::write(w.root.join("notes.txt"), b"note").unwrap();
    let (code, out, err) = w.run(Kind::Dir, &["build"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(!w.root.join("build").exists());
    let entries = w.entries();
    assert_eq!(entries.len(), 1, "one call is one trash entry");
    let entry = &entries[0];
    let name = entry.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with("-build"), "{name}");
    let root_name = w.root.file_name().unwrap();
    assert_eq!(
        std::fs::read(entry.join(root_name).join("build/obj/a.o")).unwrap(),
        b"obj"
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(entry.join(TRASH_MANIFEST)).unwrap()).unwrap();
    assert_eq!(manifest["command"], "safe-rm");
    let expected_origin = w.root.join("build").display().to_string();
    let expected_origin = if crate::platform::host::WINDOWS {
        expected_origin
            .strip_prefix(r"\\?\")
            .unwrap_or(&expected_origin)
            .to_string()
    } else {
        expected_origin
    };
    assert_eq!(manifest["items"][0]["origin"], expected_origin);
    // safe-rm on a file, into a second entry.
    let (code, _, _) = w.run(Kind::File, &["notes.txt"]);
    assert_eq!(code, 0);
    assert_eq!(w.entries().len(), 2);
}

#[test]
fn purge_deletes_and_dry_run_changes_nothing() {
    let w = world();
    std::fs::write(w.root.join("a"), b"a").unwrap();
    std::fs::create_dir_all(w.root.join("d/e")).unwrap();
    let (code, out, _) = w.run(Kind::File, &["--dry-run", "a"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("would-trash"), "{out}");
    let (_, out, _) = w.run(Kind::Dir, &["--dry-run", "--purge", "d"]);
    assert!(out.starts_with("would-purge"), "{out}");
    assert!(w.root.join("a").exists() && w.root.join("d").exists());
    assert!(w.entries().is_empty());
    let (code, out, _) = w.run(Kind::File, &["--purge", "a"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("purged"), "{out}");
    let (code, _, _) = w.run(Kind::Dir, &["--purge", "d"]);
    assert_eq!(code, 0);
    assert!(!w.root.join("a").exists() && !w.root.join("d").exists());
    assert!(w.entries().is_empty(), "purge never trashes");
}

#[test]
fn wrong_kind_gets_a_hint_and_batches_keep_going() {
    let w = world();
    std::fs::create_dir_all(w.root.join("dir/inner")).unwrap();
    std::fs::write(w.root.join("file"), b"f").unwrap();
    std::fs::write(w.root.join("other"), b"o").unwrap();
    let (code, _, err) = w.run(Kind::File, &["dir", "other"]);
    assert_eq!(code, 1);
    assert!(err.contains("is a directory"), "{err}");
    assert!(
        !w.root.join("other").exists(),
        "the rest of the batch still runs"
    );
    let (code, _, err) = w.run(Kind::Dir, &["file"]);
    assert_eq!(code, 1);
    assert!(err.contains("not a directory"), "{err}");
    // `find -exec safe-rm {} +` order: the parent first, then its child.
    let (code, _, err) = w.run(Kind::Dir, &["dir", "dir/inner"]);
    assert_eq!(
        code, 0,
        "a path inside an already removed dir is skipped: {err}"
    );
    // `find -depth` order: the child first, then its parent.
    std::fs::create_dir_all(w.root.join("deep/obj")).unwrap();
    let (code, _, err) = w.run(Kind::Dir, &["deep/obj", "deep"]);
    assert_eq!(code, 0, "the parent takes over its listed child: {err}");
    assert!(!w.root.join("deep").exists());
    let (code, _, err) = w.run(Kind::File, &["missing"]);
    assert_eq!(code, 1);
    assert!(err.contains("no such file"), "{err}");
}

#[test]
fn tracked_paths_need_the_tracked_flag() {
    let w = world();
    let git = |a: &[&str]| crate::worktrees::run_git(&w.root, a).unwrap();
    git(&["init", "-q"]);
    std::fs::create_dir_all(w.root.join("src")).unwrap();
    std::fs::write(w.root.join("src/lib.rs"), b"code").unwrap();
    std::fs::write(w.root.join("scratch.txt"), b"tmp").unwrap();
    git(&["add", "src/lib.rs"]);
    let (code, _, err) = w.run(Kind::File, &["src/lib.rs"]);
    assert_eq!(code, 1);
    assert!(err.contains("git rm"), "{err}");
    let (code, _, err) = w.run(Kind::Dir, &["src"]);
    assert_eq!(code, 1);
    assert!(err.contains("git-tracked"), "{err}");
    assert!(w.root.join("src/lib.rs").exists());
    let (code, _, err) = w.run(Kind::File, &["scratch.txt"]);
    assert_eq!(code, 0, "untracked: {err}");
    let (code, _, err) = w.run(Kind::Dir, &["--tracked", "src"]);
    assert_eq!(code, 0, "{err}");
    assert!(!w.root.join("src").exists());
}

#[test]
fn every_call_writes_one_audit_record() {
    let w = world();
    std::fs::write(w.root.join("a"), b"a").unwrap();
    w.run(Kind::File, &["a", "/definitely/outside"]);
    let records = w.audit_records();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r["command"], "safe-rm");
    assert_eq!(r["session_id"], "session-1");
    assert_eq!(r["role"], "agent");
    assert_eq!(r["exit"], 1);
    assert_eq!(r["roots"][0], w.root.display().to_string());
    let actions: Vec<&str> = r["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, vec!["refused", "trashed"]);
    assert!(r["paths"][1]["trash_path"].is_string());
}

#[test]
fn rm_trash_entries_expire_after_the_keep_window_and_quarantine_entries_are_not_kept() {
    let w = world();
    std::fs::write(w.root.join("a"), b"a").unwrap();
    w.run(Kind::File, &["a"]);
    let entry = w.entries().pop().unwrap();
    let now = SystemTime::now();
    assert!(keep_trash_entry(&entry, now));
    assert!(expired_trash_entries(&w.trash, now).is_empty());
    let later = now + TRASH_KEEP + Duration::from_secs(60);
    assert!(!keep_trash_entry(&entry, later));
    assert_eq!(expired_trash_entries(&w.trash, later), vec![entry]);
    // A `clud trash` quarantine dir has no manifest: never kept, never swept here.
    let quarantine = w.trash.join("20260101T000000Z-abcdef");
    std::fs::create_dir_all(&quarantine).unwrap();
    assert!(!keep_trash_entry(&quarantine, now));
    assert!(!expired_trash_entries(&w.trash, later).contains(&quarantine));
}

#[test]
fn session_roots_are_the_checkout_and_the_session_temp_dir() {
    let w = world();
    std::fs::create_dir_all(w.root.join(".git")).unwrap();
    std::fs::create_dir_all(w.root.join("sub")).unwrap();
    let value = session_roots_value(&w.root.join("sub")).unwrap();
    let roots: Vec<PathBuf> = std::env::split_paths(&value).collect();
    assert_eq!(roots[0], w.root);
}

#[test]
fn session_roots_include_sister_checkouts_and_the_extern_dir() {
    // #1696: sister repos are first-class working locations.
    let w = world();
    let parent = w.root.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(w.root.join(".git")).unwrap();
    std::fs::create_dir_all(parent.join("soldr/.git")).unwrap();
    std::fs::create_dir_all(parent.join("repo-extern/soldr-166")).unwrap();
    std::fs::create_dir_all(parent.join("plain-dir")).unwrap();
    std::fs::write(parent.join("linked-wt"), b"").unwrap();
    let value = session_roots_value(&w.root).unwrap();
    let roots: Vec<PathBuf> = std::env::split_paths(&value).collect();
    assert_eq!(roots[0], w.root);
    assert!(roots.contains(&parent.join("soldr")), "{roots:?}");
    assert!(roots.contains(&parent.join("repo-extern")), "{roots:?}");
    assert!(!roots.contains(&parent.join("plain-dir")), "{roots:?}");
    assert!(!roots.contains(&w.home), "{roots:?}");
    assert!(!roots.contains(&parent), "{roots:?}");
}

#[test]
fn a_linked_worktree_session_uses_its_main_checkouts_extern_dir() {
    let w = world();
    let parent = w.root.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(w.root.join(".git/worktrees/wt-1")).unwrap();
    std::fs::create_dir_all(parent.join("repo-extern")).unwrap();
    let wt = parent.join("repo-wt-1");
    std::fs::create_dir_all(&wt).unwrap();
    let gitdir = w.root.join(".git/worktrees/wt-1");
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    let value = session_roots_value(&wt).unwrap();
    let roots: Vec<PathBuf> = std::env::split_paths(&value).collect();
    assert_eq!(roots[0], wt);
    assert!(roots.contains(&parent.join("repo-extern")), "{roots:?}");
    assert!(roots.contains(&w.root), "{roots:?}");
}

#[test]
fn sister_checkouts_need_a_repo_session() {
    let w = world();
    let parent = w.root.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(parent.join("soldr/.git")).unwrap();
    let value = session_roots_value(&w.root).unwrap();
    let roots: Vec<PathBuf> = std::env::split_paths(&value).collect();
    assert!(!roots.contains(&parent.join("soldr")), "{roots:?}");
}

/// A repo at `w.root` with one commit, and a sibling worktree `wt-x` outside
/// the roots, holding a read-only build tree (#1573).
fn world_with_worktree() -> (World, PathBuf) {
    let w = world();
    let git = |a: &[&str]| crate::worktrees::run_git(&w.root, a).unwrap();
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(w.root.join("a.txt"), b"a").unwrap();
    git(&["add", "a.txt"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@localhost",
        "commit",
        "-q",
        "-m",
        "init",
    ]);
    let wt = w.root.parent().unwrap().join("wt-x");
    git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt-x"]);
    let target = wt.join("target/debug");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("artifact"), b"sealed").unwrap();
    for dir in [&target, &wt.join("target")] {
        crate::platform::rm_tool::seal_dir_for_test(dir);
    }
    (w, crate::path_norm::canonicalize_plain(&wt).unwrap())
}

#[test]
fn a_registered_worktree_of_an_allowed_repo_is_trashed_even_when_sealed() {
    let (w, wt) = world_with_worktree();
    let (code, _, err) = w.run(Kind::Dir, &["../wt-x"]);
    assert_eq!(code, 0, "{err}");
    assert!(!wt.exists(), "the worktree directory is gone");
    let moved = w.entries();
    assert_eq!(moved.len(), 1, "one trash entry: {moved:?}");
}

#[test]
fn a_half_removed_worktree_is_still_trashable() {
    let (w, wt) = world_with_worktree();
    // Git dropped it from `git worktree list` but the directory stayed.
    let meta = w.root.join(".git/worktrees/wt-x");
    assert!(meta.is_dir(), "{}", meta.display());
    std::fs::remove_dir_all(&meta).unwrap();
    let (code, _, err) = w.run(Kind::Dir, &["../wt-x"]);
    assert_eq!(code, 0, "{err}");
    assert!(!wt.exists());
}

#[test]
fn a_directory_git_did_not_create_for_the_repo_is_still_refused() {
    let (w, _wt) = world_with_worktree();
    let other = w.root.parent().unwrap().join("unrelated");
    std::fs::create_dir_all(&other).unwrap();
    let (code, _, err) = w.run(Kind::Dir, &["../unrelated"]);
    assert_eq!(code, 1);
    assert!(err.contains("outside the allowed roots"), "{err}");
    assert!(other.exists());
    // A `.git` file naming some other repo's metadata does not count.
    std::fs::write(other.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
    let (code, _, err) = w.run(Kind::Dir, &["../unrelated"]);
    assert_eq!(code, 1, "{err}");
    assert!(other.exists());
}

#[test]
fn a_sealed_tree_is_purged_too() {
    let (w, wt) = world_with_worktree();
    let (code, _, err) = w.run(Kind::Dir, &["--purge", "../wt-x"]);
    assert_eq!(code, 0, "{err}");
    assert!(!wt.exists());
}

fn git(cwd: &Path, a: &[&str]) -> String {
    crate::worktrees::run_git(cwd, a).unwrap()
}

/// Commit `file` (written with `body`) in the checkout at `dir`.
fn commit_file(dir: &Path, file: &str, body: &str) {
    std::fs::write(dir.join(file), body).unwrap();
    git(dir, &["add", file]);
    let identity = ["-c", "user.name=t", "-c", "user.email=t@localhost"];
    let mut commit = identity.to_vec();
    commit.extend(["commit", "-q", "-m", file]);
    git(dir, &commit);
}

/// `w.root` pushes to a bare `remote.git` (its `origin`), beside the root and
/// outside the allowed roots (#1573).
fn world_with_remote() -> World {
    let w = world();
    let base = w.root.parent().unwrap().to_path_buf();
    let bare = base.join("remote.git");
    let bare = bare.to_str().unwrap();
    git(&base, &["init", "-q", "--bare", "-b", "main", bare]);
    git(&w.root, &["init", "-q", "-b", "main"]);
    commit_file(&w.root, "a.txt", "a");
    git(&w.root, &["remote", "add", "origin", bare]);
    git(&w.root, &["push", "-q", "origin", "main"]);
    w
}

/// A fresh clone of `remote.git` named `name`, beside the root.
fn clone_beside(w: &World, name: &str) -> PathBuf {
    let base = w.root.parent().unwrap();
    let bare = base.join("remote.git");
    git(base, &["clone", "-q", bare.to_str().unwrap(), name]);
    base.join(name)
}

#[test]
fn a_clean_fully_pushed_clone_of_an_allowed_repo_is_trashed() {
    let w = world_with_remote();
    let clone = clone_beside(&w, "clone");
    let (code, _, err) = w.run(Kind::Dir, &["../clone"]);
    assert_eq!(code, 0, "{err}");
    assert!(!clone.exists(), "the clone is gone");
    assert_eq!(w.entries().len(), 1, "one trash entry");
}

#[test]
fn a_clone_with_unpushed_commits_is_refused() {
    let w = world_with_remote();
    let clone = clone_beside(&w, "clone");
    commit_file(&clone, "b.txt", "b");
    let (code, _, err) = w.run(Kind::Dir, &["../clone"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("unpushed commits on main"), "{err}");
    assert!(clone.join("b.txt").exists());
}

#[test]
fn a_clone_with_an_untracked_file_is_refused() {
    let w = world_with_remote();
    let clone = clone_beside(&w, "clone");
    std::fs::write(clone.join("notes.txt"), b"keep me").unwrap();
    let (code, _, err) = w.run(Kind::Dir, &["../clone"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("dirty"), "{err}");
    assert!(clone.join("notes.txt").exists());
}

#[test]
fn an_unrelated_directory_or_repo_beside_the_root_is_refused() {
    let w = world_with_remote();
    let base = w.root.parent().unwrap().to_path_buf();
    let plain = base.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let (code, _, err) = w.run(Kind::Dir, &["../plain"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("outside the allowed roots"), "{err}");
    assert!(plain.exists());

    let other = base.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    let url = "https://example.com/else/repo";
    git(&other, &["remote", "add", "origin", url]);
    let (code, _, err) = w.run(Kind::Dir, &["../other"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("origin mismatch"), "{err}");
    assert!(other.exists());
}

// ---- System temp directories (#1622) ----
//
// The "system temp" here is a directory the test creates inside the world, so
// no test reads or deletes anything in the real /tmp beyond its own tempdir.

pub(crate) struct TempWorld {
    pub(crate) w: World,
    pub(crate) temp: PathBuf,
    pub(crate) outside: PathBuf,
}

pub(crate) fn temp_world() -> TempWorld {
    let w = world();
    let base = w.root.parent().unwrap().to_path_buf();
    let temp = base.join("systmp");
    let outside = base.join("outside");
    for dir in [&temp, &outside] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(outside.join("keep"), b"precious").unwrap();
    TempWorld { w, temp, outside }
}

impl TempWorld {
    pub(crate) fn roots(&self) -> Roots {
        Roots::fixed(vec![self.w.root.clone()], true).with_temp_roots(vec![self.temp.clone()])
    }

    pub(crate) fn run_with_roots(&self, mut roots: Roots, list: &[&str]) -> (i32, String, String) {
        let options = parse_args(&args(list)).unwrap().unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with(
            Kind::File,
            &options,
            &self.w.ctx(),
            &mut roots,
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    pub(crate) fn run(&self, list: &[&str]) -> (i32, String, String) {
        self.run_with_roots(self.roots(), list)
    }

    pub(crate) fn t(&self, rel: &str) -> String {
        self.temp.join(rel).to_string_lossy().into_owned()
    }
}

#[test]
fn a_file_under_the_system_temp_dir_is_deletable() {
    let tw = temp_world();
    std::fs::write(tw.temp.join("issue-body.md"), b"x").unwrap();
    let (code, _, err) = tw.run(&[&tw.t("issue-body.md")]);
    assert_eq!(code, 0, "{err}");
    assert!(!tw.temp.join("issue-body.md").exists());
    // --purge too.
    std::fs::write(tw.temp.join("again"), b"x").unwrap();
    let (code, _, err) = tw.run(&["--purge", &tw.t("again")]);
    assert_eq!(code, 0, "{err}");
    assert!(!tw.temp.join("again").exists());
}

#[test]
fn a_nested_directory_under_the_system_temp_dir_is_deletable() {
    let tw = temp_world();
    std::fs::create_dir_all(tw.temp.join("build/obj")).unwrap();
    std::fs::write(tw.temp.join("build/obj/a.o"), b"obj").unwrap();
    let (code, _, err) = tw.run(&["-r", &tw.t("build")]);
    assert_eq!(code, 0, "{err}");
    assert!(!tw.temp.join("build").exists());
    assert!(tw.temp.is_dir(), "the temp root itself must survive");
}

#[test]
fn the_system_temp_dir_itself_is_refused() {
    let tw = temp_world();
    std::fs::write(tw.temp.join("f"), b"x").unwrap();
    let plain = tw.temp.to_string_lossy().into_owned();
    for operand in [plain.clone(), format!("{plain}/"), format!("{plain}/.")] {
        let (code, _, err) = tw.run(&["-r", "--purge", &operand]);
        assert_eq!(code, 1, "{operand}: {err}");
        assert!(
            err.contains("temp directory") || err.contains("name it directly"),
            "{operand}: {err}"
        );
    }
    assert!(tw.temp.join("f").exists());
    let err = resolve(&plain, &tw.w.root, Some(&tw.w.home), &mut tw.roots()).unwrap_err();
    assert!(err.contains("system temp directory itself"), "{err}");
}

#[test]
fn dot_dot_out_of_the_system_temp_dir_is_judged_by_its_target() {
    let tw = temp_world();
    let (code, _, err) = tw.run(&["--purge", &tw.t("../outside/keep")]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("outside the allowed roots"), "{err}");
    // The refusal lists the temp root among the allowed roots.
    assert!(err.contains(&*tw.temp.to_string_lossy()), "{err}");
    assert!(tw.outside.join("keep").exists());
}

#[test]
fn temp_root_candidates_follow_the_platform_and_env() {
    let unix_env = |key: &str| match key {
        "TMPDIR" => Some("/var/folders/ab/xyz/T/".into()),
        "TEMP" => Some("/ignored/on/unix".into()),
        _ => None,
    };
    let unix = temp_root_candidates(false, &unix_env);
    assert!(unix.contains(&PathBuf::from("/tmp")), "{unix:?}");
    // #1659: POSIX shared memory is a temp root; the per-user runtime dir,
    // which holds live session sockets, is not.
    assert!(unix.contains(&PathBuf::from("/dev/shm")), "{unix:?}");
    assert!(!unix.iter().any(|p| p.starts_with("/run")), "{unix:?}");
    assert!(unix.contains(&PathBuf::from("/var/tmp")), "{unix:?}");
    assert!(
        unix.iter()
            .any(|p| p.to_string_lossy().starts_with("/var/folders/ab/xyz/T")),
        "{unix:?}"
    );
    assert!(!unix.iter().any(|p| p.starts_with("/ignored")), "{unix:?}");
    // A relative or empty TMPDIR is ignored.
    let rel = temp_root_candidates(false, &|k: &str| (k == "TMPDIR").then(|| "tmp".into()));
    assert_eq!(
        rel,
        vec![
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
            PathBuf::from("/dev/shm")
        ]
    );

    // Windows: %TEMP% and %TMP%, verbatim prefix stripped, duplicates folded,
    // no Unix defaults. Pure string handling, so it runs on every host.
    let win_env = |key: &str| match key {
        "TEMP" => Some(r"C:\Users\u\AppData\Local\Temp".into()),
        "TMP" => Some(r"\\?\C:\Users\u\AppData\Local\Temp".into()),
        "TMPDIR" => Some("/tmp".into()),
        _ => None,
    };
    let win = temp_root_candidates(true, &win_env);
    assert_eq!(win, vec![PathBuf::from(r"C:\Users\u\AppData\Local\Temp")]);
    let relative = temp_root_candidates(true, &|k: &str| (k == "TEMP").then(|| "Temp".into()));
    assert!(relative.is_empty(), "{relative:?}");
}

// ---- RAM-backed temp roots: /dev/shm (#1659) ----
//
// "shm" is a directory the test creates inside the world standing in for
// /dev/shm; no test touches the real /dev/shm.

pub(crate) fn shm_world() -> (TempWorld, PathBuf) {
    let tw = temp_world();
    let shm = tw.w.root.parent().unwrap().join("shm");
    std::fs::create_dir_all(shm.join("wild-review/out")).unwrap();
    std::fs::write(shm.join("wild-review/out/a.o"), b"link output").unwrap();
    (tw, shm)
}

pub(crate) fn shm_roots(tw: &TempWorld, shm: &Path) -> Roots {
    Roots::fixed(vec![tw.w.root.clone()], true)
        .with_temp_roots(vec![tw.temp.clone(), shm.to_path_buf()])
        .with_ram_roots(vec![shm.to_path_buf()])
}

#[test]
fn a_path_outside_every_root_is_refused_even_with_purge_and_says_so() {
    let (tw, shm) = shm_world();
    // The 2.8.20 shape: the shm dir is not a temp root at all.
    let target = shm.join("wild-review").to_string_lossy().into_owned();
    for flags in [&["-r"][..], &["-r", "--purge"][..]] {
        let mut list = flags.to_vec();
        list.push(&target);
        let (code, _, err) = tw.run(&list);
        assert_eq!(code, 1, "{err}");
        assert!(err.contains("outside the allowed roots"), "{err}");
        assert!(err.contains("--purge does not relax"), "{err}");
        assert!(err.contains("session temp dir"), "{err}");
    }
    assert!(shm.join("wild-review/out/a.o").exists());
}

#[test]
fn a_tree_under_a_ram_backed_temp_root_is_purged_without_trashing() {
    let (tw, shm) = shm_world();
    let target = shm.join("wild-review").to_string_lossy().into_owned();
    let (code, out, err) = tw.run_with_roots(shm_roots(&tw, &shm), &["-r", "--dry-run", &target]);
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("would-purge"), "{out}{err}");
    assert!(shm.join("wild-review").exists());
    // No --purge given: still purged, and nothing lands in the trash.
    let (code, _, err) = tw.run_with_roots(shm_roots(&tw, &shm), &["-r", &target]);
    assert_eq!(code, 0, "{err}");
    assert!(!shm.join("wild-review").exists());
    assert!(shm.is_dir(), "the shm root itself must survive");
    assert!(tw.w.entries().is_empty(), "{:?}", tw.w.entries());
}

#[test]
fn a_ram_backed_temp_root_itself_is_refused() {
    let (tw, shm) = shm_world();
    let plain = shm.to_string_lossy().into_owned();
    let (code, _, err) = tw.run_with_roots(shm_roots(&tw, &shm), &["-r", "--purge", &plain]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("temp directory itself"), "{err}");
    assert!(shm.join("wild-review/out/a.o").exists());
}

#[test]
fn a_disk_backed_temp_root_still_trashes() {
    let (tw, shm) = shm_world();
    std::fs::write(tw.temp.join("f"), b"x").unwrap();
    let (code, _, err) = tw.run_with_roots(shm_roots(&tw, &shm), &[&tw.t("f")]);
    assert_eq!(code, 0, "{err}");
    assert!(!tw.temp.join("f").exists());
    assert_eq!(tw.w.entries().len(), 1, "a disk temp entry is trashed");
}

// ---------- #1666: creation ledger consult ----------

use crate::gc::CreatedEntry;

#[derive(Debug)]
struct FakeLedger(Result<Vec<CreatedEntry>, String>);

impl ledger::CreationLedger for FakeLedger {
    fn lookup(&self, _path: &Path) -> Result<Vec<CreatedEntry>, String> {
        self.0.clone()
    }
}

pub(crate) fn run_ledger(
    w: &World,
    rows: Result<Vec<CreatedEntry>, String>,
    list: &[&str],
) -> (i32, String, String) {
    let options = parse_args(&args(list)).unwrap().unwrap();
    let mut roots = w.roots().with_ledger(Arc::new(FakeLedger(rows)));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(
        Kind::Safe,
        &options,
        &w.ctx(),
        &mut roots,
        &mut out,
        &mut err,
    );
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn ledger_unavailable_keeps_the_strict_refusal_and_says_so() {
    let w = world();
    let outside = w.home.parent().unwrap().join("outside.txt");
    std::fs::write(&outside, "x").unwrap();
    let rows = Err("clud daemon not reachable: connection refused".to_string());
    let (code, _, err) = run_ledger(&w, rows, &[outside.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(err.contains("outside the allowed roots"), "{err}");
    assert!(err.contains("creation ledger unavailable"), "{err}");
    assert!(outside.exists());
}

#[test]
fn daemon_ledger_without_a_running_daemon_is_unavailable() {
    let state = tempfile::tempdir().unwrap();
    let source = ledger::DaemonLedger {
        state_dir: Some(state.path().to_path_buf()),
        session_id: Some("session-1".into()),
    };
    let error = ledger::CreationLedger::lookup(&source, Path::new("/x")).unwrap_err();
    assert!(error.contains("not reachable"), "{error}");
    let no_session = ledger::DaemonLedger {
        state_dir: Some(state.path().to_path_buf()),
        session_id: None,
    };
    let error = ledger::CreationLedger::lookup(&no_session, Path::new("/x")).unwrap_err();
    assert!(error.contains("no clud session id"), "{error}");
}

// ---------- #1668: human-set extra roots ----------

#[path = "rm_tool_extra_roots_tests.rs"]
pub(crate) mod extra_roots_tests;

/// A `.venv` is disposable wherever it lives: safe-rm moves it to the trash
/// even outside every root, with the reason in the audit record.
#[test]
fn venv_outside_the_roots_is_trashed() {
    let w = world();
    let outside = w.root.parent().unwrap().join("other-project");
    let venv = outside.join(".venv");
    std::fs::create_dir_all(venv.join("lib")).unwrap();
    std::fs::write(venv.join("lib").join("site.py"), "x").unwrap();
    let (code, _out, err) = w.run(Kind::Safe, &["-r", venv.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(!venv.exists(), "the .venv must be moved to the trash");
    assert_eq!(w.entries().len(), 1);
    let reasons: Vec<String> = w
        .audit_records()
        .iter()
        .flat_map(|r| r["paths"].as_array().cloned().unwrap_or_default())
        .filter_map(|p| p["reason"].as_str().map(str::to_string))
        .collect();
    assert!(
        reasons.iter().any(|r| r.contains(".venv")),
        "audit records must say why: {reasons:?}"
    );
}

#[test]
fn path_inside_a_venv_outside_the_roots_is_trashed() {
    let w = world();
    let venv = w.root.parent().unwrap().join("other-project").join(".venv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    let python = venv.join("bin").join("python");
    std::fs::write(&python, "x").unwrap();
    let (code, _out, err) = w.run(Kind::Safe, &[python.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(!python.exists());
}

#[test]
fn only_a_component_named_exactly_venv_is_exempt() {
    let w = world();
    let outside = w.root.parent().unwrap().join("other-project");
    for name in [".venv-old", "venv", "src"] {
        let dir = outside.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let (code, _out, err) = w.run(Kind::Safe, &["-r", dir.to_str().unwrap()]);
        assert_ne!(code, 0, "{name} must stay refused");
        assert!(err.contains("outside the allowed roots"), "{name}: {err}");
        assert!(
            err.contains("clud trash <PATH>"),
            "{name}: the refusal names the workaround: {err}"
        );
        assert!(dir.exists());
    }
}

#[cfg(test)]
#[path = "rm_tool_home_files_tests.rs"]
mod home_files_tests;

/// A root whose `.git` is not a valid repository, nested inside a real repo:
/// git walks up to the outer repo, and its worktree list must not widen the
/// roots to that repo. This held only when the test's temp dir happened to
/// sit outside every checkout, so the fixture now builds the outer repo.
#[test]
fn bogus_git_marker_does_not_widen_roots_to_the_enclosing_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let outer = crate::path_norm::canonicalize_plain(tmp.path()).unwrap();
    git(&outer, &["init", "--quiet"]);
    let inner = outer.join("inner");
    let neighbour = outer.join("neighbour");
    std::fs::create_dir_all(inner.join(".git")).unwrap();
    std::fs::create_dir_all(&neighbour).unwrap();
    std::fs::write(neighbour.join("victim.rs"), "keep").unwrap();
    let mut roots = Roots::fixed(vec![inner], true);
    let refused = resolve(
        neighbour.join("victim.rs").to_str().unwrap(),
        &outer,
        None,
        &mut roots,
    );
    assert!(refused.is_err(), "{refused:?}");
}

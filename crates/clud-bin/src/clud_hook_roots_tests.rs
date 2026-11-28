//! The typed root registry and its firing rule (#967 Phase 3).

use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs;

fn make_repo(root: &Path, relative: &[&str]) -> PathBuf {
    let mut path = root.to_path_buf();
    for segment in relative {
        path.push(segment);
    }
    fs::create_dir_all(path.join(".git")).expect("mkdir repo");
    lexical_normalize(&path)
}

fn kind_for(roots: &HookRoots, path: &Path) -> Option<RootKind> {
    roots.containing(path).map(|root| root.kind)
}

#[test]
fn the_session_root_is_the_parent() {
    let tmp = TemporaryDirectory::new().unwrap();
    let roots = HookRoots::parent_only(tmp.path());

    assert_eq!(kind_for(&roots, tmp.path()), Some(RootKind::Parent));
    assert_eq!(
        kind_for(&roots, &tmp.path().join("src").join("lib.rs")),
        Some(RootKind::Parent)
    );
}

#[test]
fn extern_repos_children_are_registered_implicitly() {
    // The GC-tracked convention: clud put them there, so it knows what they
    // are without anyone declaring them.
    let tmp = TemporaryDirectory::new().unwrap();
    let sub = make_repo(tmp.path(), &[EXTERN_REPOS_DIR, "running-process"]);

    let roots = HookRoots::parent_only(tmp.path());

    assert_eq!(kind_for(&roots, &sub), Some(RootKind::Extern));
    assert_eq!(
        kind_for(&roots, &sub.join("src").join("lib.rs")),
        Some(RootKind::Extern)
    );
}

#[test]
fn a_nested_git_repo_is_not_a_child_unless_declared() {
    // #966 D8: declaration is the consent that makes the child tier's
    // no-prompt trust sound, and it collapses if nothing was declared.
    let tmp = TemporaryDirectory::new().unwrap();
    let nested = make_repo(tmp.path(), &["vendor", "somelib"]);

    let undeclared = HookRoots::parent_only(tmp.path());
    assert_eq!(
        kind_for(&undeclared, &nested),
        Some(RootKind::Parent),
        "an undeclared nested repo is just part of the parent tree"
    );

    let declared = HookRoots::resolve(tmp.path(), &["vendor/somelib".to_string()], None);
    assert_eq!(kind_for(&declared, &nested), Some(RootKind::Child));
}

#[test]
fn a_declared_child_may_be_absolute_or_relative_in_either_spelling() {
    let tmp = TemporaryDirectory::new().unwrap();
    let nested = make_repo(tmp.path(), &["child"]);

    for declaration in [
        "child".to_string(),
        "./child".to_string(),
        nested.to_string_lossy().into_owned(),
    ] {
        let roots = HookRoots::resolve(tmp.path(), std::slice::from_ref(&declaration), None);
        assert_eq!(
            kind_for(&roots, &nested),
            Some(RootKind::Child),
            "{declaration}"
        );
    }
}

#[test]
fn the_most_specific_root_wins() {
    // A sub-repo lives inside the parent's tree by construction, so a
    // registration-order-dependent lookup would answer "parent" for it.
    let tmp = TemporaryDirectory::new().unwrap();
    let sub = make_repo(tmp.path(), &[EXTERN_REPOS_DIR, "dep"]);

    let roots = HookRoots::parent_only(tmp.path());

    assert_eq!(
        kind_for(&roots, &sub.join("file.rs")),
        Some(RootKind::Extern)
    );
    assert_eq!(
        kind_for(&roots, &tmp.path().join("file.rs")),
        Some(RootKind::Parent)
    );
}

#[test]
fn a_path_outside_every_root_is_unregistered() {
    let tmp = TemporaryDirectory::new().unwrap();
    let elsewhere = TemporaryDirectory::new().unwrap();

    let roots = HookRoots::parent_only(tmp.path());

    assert_eq!(kind_for(&roots, elsewhere.path()), None);
    assert!(!roots.parent_hooks_apply_to(elsewhere.path()));
}

// -----------------------------------------------------------------
// The firing rule.
// -----------------------------------------------------------------

#[test]
fn parent_hooks_never_fire_in_an_extern_root() {
    // A parent guard pointed at a foreign checkout can only misfire or error
    // -- that is the #841 ENOENT wedge.
    let tmp = TemporaryDirectory::new().unwrap();
    let sub = make_repo(tmp.path(), &[EXTERN_REPOS_DIR, "dep"]);

    let roots = HookRoots::parent_only(tmp.path());

    assert!(roots.parent_hooks_apply_to(&tmp.path().join("src/lib.rs")));
    assert!(!roots.parent_hooks_apply_to(&sub.join("src/lib.rs")));
}

#[test]
fn parent_hooks_do_fire_in_a_declared_child() {
    // A declared child is part of the parent's world, unlike a visitor.
    let tmp = TemporaryDirectory::new().unwrap();
    let child = make_repo(tmp.path(), &["packages", "core"]);

    let roots = HookRoots::resolve(tmp.path(), &["packages/core".to_string()], None);

    assert!(roots.parent_hooks_apply_to(&child.join("src/lib.rs")));
}

#[test]
fn the_kind_alone_decides_whether_parent_hooks_apply() {
    assert!(RootKind::Parent.parent_hooks_apply());
    assert!(RootKind::Child.parent_hooks_apply());
    assert!(!RootKind::Extern.parent_hooks_apply());
}

// -----------------------------------------------------------------
// The env channel.
// -----------------------------------------------------------------

#[test]
fn roots_round_trip_through_the_env_encoding() {
    // The hook cannot rediscover --add-dir targets: they appear in no hook
    // payload, so clud has to carry them across the process boundary.
    let tmp = TemporaryDirectory::new().unwrap();
    let extra = TemporaryDirectory::new().unwrap();

    let encoded = HookRoots::resolve(
        tmp.path(),
        &[],
        Some(&format!(
            r#"[{{"kind":"child","path":{}}}]"#,
            serde_json::to_string(&extra.path().to_string_lossy()).unwrap()
        )),
    )
    .to_env_value();

    let restored = HookRoots::resolve(tmp.path(), &[], Some(&encoded));
    assert_eq!(kind_for(&restored, extra.path()), Some(RootKind::Child));
    assert_eq!(kind_for(&restored, tmp.path()), Some(RootKind::Parent));
}

#[test]
fn an_unparsable_env_value_is_ignored_rather_than_fatal() {
    let tmp = TemporaryDirectory::new().unwrap();

    let roots = HookRoots::resolve(tmp.path(), &[], Some("{not json"));

    assert_eq!(kind_for(&roots, tmp.path()), Some(RootKind::Parent));
}

#[test]
fn an_env_entry_missing_a_field_is_skipped_not_guessed() {
    let tmp = TemporaryDirectory::new().unwrap();

    let roots = HookRoots::resolve(
        tmp.path(),
        &[],
        Some(r#"[{"path":"/somewhere"},{"kind":"nonsense","path":"/other"}]"#),
    );

    assert_eq!(roots.all().len(), 1, "only the parent survives");
}

#[test]
fn the_same_root_registered_twice_appears_once() {
    let tmp = TemporaryDirectory::new().unwrap();
    let child = make_repo(tmp.path(), &["child"]);
    let encoded = format!(
        r#"[{{"kind":"child","path":{}}}]"#,
        serde_json::to_string(&child.to_string_lossy()).unwrap()
    );

    let roots = HookRoots::resolve(tmp.path(), &["child".to_string()], Some(&encoded));

    assert_eq!(roots.all().len(), 2, "{:?}", roots.all());
}

#[test]
fn paths_lists_every_root_for_the_cwd_pinning_set() {
    let tmp = TemporaryDirectory::new().unwrap();
    let sub = make_repo(tmp.path(), &[EXTERN_REPOS_DIR, "dep"]);

    let paths = HookRoots::parent_only(tmp.path()).paths();

    assert!(paths.iter().any(|path| key_of(path) == key_of(&sub)));
    assert!(paths
        .iter()
        .any(|path| key_of(path) == key_of(&lexical_normalize(tmp.path()))));
}

// -----------------------------------------------------------------
// What a call names.
// -----------------------------------------------------------------

#[test]
fn a_tool_that_names_a_file_reports_it_resolved_against_cwd() {
    let cwd = if crate::platform::host::WINDOWS {
        PathBuf::from(r"C:\repo")
    } else {
        PathBuf::from("/repo")
    };

    let absolute = tool_input_paths(
        Some(&serde_json::json!({"file_path": cwd.join("src").to_string_lossy()})),
        &cwd,
    );
    assert_eq!(absolute, vec![lexical_normalize(&cwd.join("src"))]);

    let relative = tool_input_paths(Some(&serde_json::json!({"file_path": "src/lib.rs"})), &cwd);
    assert_eq!(
        relative,
        vec![lexical_normalize(&cwd.join("src").join("lib.rs"))]
    );
}

#[test]
fn a_tool_that_names_nothing_reports_nothing() {
    // Empty rather than cwd: only the caller knows whether the command
    // relocates itself with a `cd` before doing its work.
    let cwd = PathBuf::from("/repo");
    assert!(tool_input_paths(None, &cwd).is_empty());
    assert!(tool_input_paths(Some(&serde_json::json!({"command": "ls"})), &cwd).is_empty());
    assert!(tool_input_paths(Some(&serde_json::json!({"file_path": "  "})), &cwd).is_empty());
}

#[test]
fn notebook_and_generic_path_fields_are_recognized_too() {
    let cwd = PathBuf::from("/repo");
    assert_eq!(
        tool_input_paths(Some(&serde_json::json!({"notebook_path": "a.ipynb"})), &cwd).len(),
        1
    );
    assert_eq!(
        tool_input_paths(Some(&serde_json::json!({"path": "a.txt"})), &cwd).len(),
        1
    );
}

// -----------------------------------------------------------------
// The containment resolution order.
// -----------------------------------------------------------------

#[test]
fn a_cd_target_replaces_cwd_rather_than_joining_it() {
    // The regression this locks: adding cd targets *alongside* cwd and asking
    // whether any path is parent-owned keeps answering "yes" for
    // `cd .extern-repos/dep && make`, because cwd is still the parent. The
    // parent's guards then fire inside the sub-repo -- the exact failure this
    // tier exists to prevent.
    let tmp = TemporaryDirectory::new().unwrap();
    let extern_root = make_repo(tmp.path(), &[EXTERN_REPOS_DIR, "dep"]);
    let roots = HookRoots::parent_only(tmp.path());

    let touched = containment_paths(Vec::new(), vec![extern_root.clone()], tmp.path());

    assert_eq!(touched, vec![extern_root]);
    assert!(
        !touched.iter().any(|path| roots.parent_hooks_apply_to(path)),
        "cwd must not sneak back in and re-arm the parent's hooks"
    );
}

#[test]
fn a_named_path_wins_over_a_cd_target() {
    // A tool that says which file it is editing has already answered the
    // question; where the shell would have moved is irrelevant.
    let named = PathBuf::from("/repo/src/lib.rs");
    let cd_target = PathBuf::from("/elsewhere");

    let touched = containment_paths(vec![named.clone()], vec![cd_target], Path::new("/repo"));

    assert_eq!(touched, vec![named]);
}

#[test]
fn cwd_is_the_fallback_when_a_call_names_nothing_and_goes_nowhere() {
    let cwd = if crate::platform::host::WINDOWS {
        PathBuf::from(r"C:\repo")
    } else {
        PathBuf::from("/repo")
    };

    assert_eq!(
        containment_paths(Vec::new(), Vec::new(), &cwd),
        vec![lexical_normalize(&cwd)]
    );
}

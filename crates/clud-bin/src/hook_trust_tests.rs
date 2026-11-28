//! Trust-store lifecycle: parse, record, revoke, and the name+origin gate.

use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs;

fn repo(base: &Path, name: &str) -> PathBuf {
    let path = base.join(name);
    fs::create_dir_all(path.join(".git")).unwrap();
    path
}

#[test]
fn an_empty_or_missing_store_is_empty() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");

    assert!(load(&root).is_empty());
    assert!(parse("").unwrap().is_empty());
    assert!(parse("{}").unwrap().is_empty());
    assert!(parse(r#"{"rust": {"use_soldr": true}}"#)
        .unwrap()
        .is_empty());
}

#[test]
fn parse_reads_the_extern_allowlist() {
    let text = r#"{
        "hook_trust": {
            "extern": [
                { "name": "alpha", "origin": "https://example.com/alpha.git" },
                { "name": "beta", "origin": "git@github.com:user/beta.git" }
            ]
        }
    }"#;
    let store = parse(text).unwrap();
    assert_eq!(
        store.extern_entries,
        vec![
            TrustEntry {
                name: "alpha".to_string(),
                origin: "https://example.com/alpha.git".to_string(),
            },
            TrustEntry {
                name: "beta".to_string(),
                origin: "git@github.com:user/beta.git".to_string(),
            },
        ]
    );
}

#[test]
fn parse_skips_malformed_entries_and_dedupes() {
    let text = r#"{
        "hook_trust": {
            "extern": [
                { "name": "a", "origin": "https://a" },
                { "name": "a", "origin": "https://a" },
                { "name": "", "origin": "https://b" },
                { "origin": "https://c" },
                { "name": "d", "origin": "" },
                "not an object"
            ]
        }
    }"#;
    let store = parse(text).unwrap();
    assert_eq!(store.extern_entries.len(), 1);
    assert_eq!(store.extern_entries[0].name, "a");
}

#[test]
fn parse_errors_only_on_bad_json() {
    assert!(parse("not json").is_err());
    assert!(parse(r#"{"hook_trust": "nope"}"#).is_ok()); // warned, not fatal
}

#[test]
fn record_writes_and_load_reads_back() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");

    record(
        &root,
        "running-process",
        "https://github.com/zackees/running-process.git",
    )
    .unwrap();
    let store = load(&root);
    assert!(is_trusted(
        &store,
        "running-process",
        Some("https://github.com/zackees/running-process.git")
    ));
    assert!(!is_trusted(
        &store,
        "running-process",
        Some("https://github.com/someone-else/running-process.git")
    ));
    assert!(!is_trusted(
        &store,
        "other",
        Some("https://github.com/zackees/running-process.git")
    ));
}

#[test]
fn record_preserves_other_keys_in_settings_local_json() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    fs::create_dir_all(root.join(".clud")).unwrap();
    fs::write(
        root.join(".clud").join("settings.local.json"),
        r#"{ "bad_commands": [ { "pattern": "*rm -rf*", "replace": "echo" } ] }"#,
    )
    .unwrap();

    record(&root, "alpha", "https://a").unwrap();

    let text = fs::read_to_string(root.join(".clud").join("settings.local.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        doc["bad_commands"].is_array(),
        "the pre-existing section must survive the trust write"
    );
    assert_eq!(doc["hook_trust"]["extern"][0]["name"], "alpha");
}

#[test]
fn record_replaces_an_existing_entry_for_the_same_name() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");

    record(&root, "alpha", "https://first").unwrap();
    record(&root, "alpha", "https://second").unwrap();

    let store = load(&root);
    assert_eq!(store.extern_entries.len(), 1);
    assert!(is_trusted(&store, "alpha", Some("https://second")));
    assert!(!is_trusted(&store, "alpha", Some("https://first")));
}

#[test]
fn revoke_removes_by_name_and_reports_whether_anything_was_removed() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");

    assert!(!revoke(&root, "alpha").unwrap(), "nothing to revoke yet");

    record(&root, "alpha", "https://a").unwrap();
    record(&root, "beta", "https://b").unwrap();
    assert!(revoke(&root, "alpha").unwrap());
    assert!(!revoke(&root, "alpha").unwrap(), "already gone");

    let store = load(&root);
    assert_eq!(store.extern_entries.len(), 1);
    assert_eq!(store.extern_entries[0].name, "beta");
}

#[test]
fn trust_is_keyed_to_name_plus_origin() {
    let store = TrustStore {
        extern_entries: vec![TrustEntry {
            name: "alpha".to_string(),
            origin: "https://a".to_string(),
        }],
    };

    // Name matches and origin matches.
    assert!(is_trusted(&store, "alpha", Some("https://a")));
    // Re-cloned from a different remote: the origin key refuses.
    assert!(!is_trusted(&store, "alpha", Some("https://b")));
    // No readable origin: name alone carries, because nothing could have
    // changed it.
    assert!(is_trusted(&store, "alpha", None));
    // A different checkout of the same name is not trusted.
    assert!(!is_trusted(&store, "beta", Some("https://a")));
}

#[test]
fn a_stale_entry_after_gc_teardown_is_harmless() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let extern_root = root.parent().unwrap().join("myrepo-extern");
    fs::create_dir_all(extern_root.join("alpha")).unwrap();
    record(&root, "alpha", "https://a").unwrap();

    // GC tears the checkout down; the entry stays in the store.
    fs::remove_dir_all(extern_root.join("alpha")).unwrap();
    assert!(load(&root).extern_entries.len() == 1, "stale entry remains");

    // A fresh checkout from a different origin must not inherit the trust.
    fs::create_dir_all(extern_root.join("alpha")).unwrap();
    assert!(!is_trusted(
        &load(&root),
        "alpha",
        Some("https://different")
    ));
    // A fresh checkout from the same origin is still trusted — the user
    // trusted that name + remote.
    assert!(is_trusted(&load(&root), "alpha", Some("https://a")));
}

#[test]
fn valid_name_rejects_paths() {
    assert!(valid_name("alpha"));
    assert!(valid_name("my-checkout_2"));
    assert!(!valid_name(""));
    assert!(!valid_name("."));
    assert!(!valid_name(".."));
    assert!(!valid_name("../alpha"));
    assert!(!valid_name("a/b"));
    assert!(!valid_name(r"a\b"));
}

#[test]
fn origin_of_reads_the_git_config_file() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");

    assert_eq!(origin_of(&root), None, "no config yet");

    fs::write(
        root.join(".git").join("config"),
        "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://example.com/myrepo.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
    )
    .unwrap();
    assert_eq!(
        origin_of(&root),
        Some("https://example.com/myrepo.git".to_string())
    );
}

#[test]
fn origin_of_accepts_both_section_spellings_and_quotes() {
    assert_eq!(
        remote_origin_url("[remote.origin]\n\turl = git@github.com:user/repo.git\n"),
        Some("git@github.com:user/repo.git".to_string())
    );
    assert_eq!(
        remote_origin_url("[REMOTE \"ORIGIN\"]\n\turl = \"https://x/y\"\n"),
        Some("https://x/y".to_string())
    );
    assert_eq!(
        remote_origin_url("[remote \"upstream\"]\n\turl = https://upstream\n[remote \"origin\"]\n\turl=https://mine\n"),
        Some("https://mine".to_string())
    );
    assert_eq!(remote_origin_url("[remote \"origin\"]\n"), None);
    assert_eq!(
        remote_origin_url("[core]\n\turl = https://not-a-remote\n"),
        None
    );
}

#[test]
fn remote_origin_url_skips_valueless_keys_and_bare_remote_headers() {
    assert_eq!(
        remote_origin_url("[remote \"origin\"]\n\tmirror\n\turl = https://evil/x.git\n"),
        Some("https://evil/x.git".to_string())
    );
    assert_eq!(remote_origin_url("[remote]\n\turl = https://nope\n"), None);
}

#[test]
fn origin_of_follows_a_worktree_gitdir_file() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let worktree = tmp.path().join("myrepo-wt");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}", root.join(".git").display()),
    )
    .unwrap();
    fs::write(
        root.join(".git").join("config"),
        "[remote \"origin\"]\n\turl = https://example.com/myrepo.git\n",
    )
    .unwrap();

    assert_eq!(
        origin_of(&worktree),
        Some("https://example.com/myrepo.git".to_string())
    );
}

#[test]
fn origin_of_reads_config_through_worktree_commondir() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let wt_git = root.join(".git").join("worktrees").join("wt");
    fs::create_dir_all(&wt_git).unwrap();
    fs::write(wt_git.join("commondir"), "../..\n").unwrap();
    let worktree = tmp.path().join("wt");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}", wt_git.display()),
    )
    .unwrap();
    fs::write(
        root.join(".git").join("config"),
        "[remote \"origin\"]\n\turl = https://example.com/myrepo.git\n",
    )
    .unwrap();

    assert_eq!(
        origin_of(&worktree),
        Some("https://example.com/myrepo.git".to_string())
    );
}

#[test]
fn extern_dir_for_finds_sibling_then_legacy() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");

    assert_eq!(extern_dir_for(&root, "alpha"), None);

    let sibling = tmp.path().join("myrepo-extern");
    fs::create_dir_all(sibling.join("alpha")).unwrap();
    assert_eq!(extern_dir_for(&root, "alpha"), Some(sibling.join("alpha")));

    // The legacy in-tree location is still recognized.
    fs::create_dir_all(root.join(".extern-repos").join("legacy-one")).unwrap();
    assert_eq!(
        extern_dir_for(&root, "legacy-one"),
        Some(root.join(".extern-repos").join("legacy-one"))
    );

    // Path-shaped names are refused, never looked up.
    assert_eq!(extern_dir_for(&root, "../alpha"), None);
}

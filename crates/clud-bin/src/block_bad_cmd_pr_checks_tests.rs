use super::*;

fn config(enabled: bool) -> Option<PrChecksConfig> {
    Some(PrChecksConfig {
        commands: vec!["bash lint --host-only".into(), "bash test --host".into()],
        enabled,
    })
}

/// The receipt check the hook runs, against a real receipt directory.
fn receipt_for(state: &Path, tree: &str) -> bool {
    let commands = config(true).unwrap().commands;
    pr_checks::receipt_path(state, Path::new("/repo"), tree, &commands).is_file()
}

#[test]
fn not_gated_without_config() {
    assert_eq!(decide(&Facts::default()), Decision::Allow);
}

#[test]
fn denied_without_receipt_and_names_command_and_opt_outs() {
    let facts = Facts {
        config: config(true),
        ..Facts::default()
    };
    let Decision::Deny(reason) = decide(&facts) else {
        panic!("expected deny");
    };
    assert!(reason.contains("clud pr-check"));
    assert!(reason.contains("bash test --host"));
    assert!(reason.contains("CLUD_SKIP_PR_CHECKS=1"));
    assert!(reason.contains("pr_checks.enabled: false"));
}

#[test]
fn allowed_with_receipt_for_same_tree_and_denied_after_tree_changes() {
    let state = tempfile::tempdir().unwrap();
    let commands = config(true).unwrap().commands;
    let code =
        pr_checks::check_and_record(state.path(), Path::new("/repo"), "tree1", &commands, |_| 0);
    assert_eq!(code, 0);

    let same = Facts {
        config: config(true),
        receipt_present: receipt_for(state.path(), "tree1"),
        ..Facts::default()
    };
    assert_eq!(decide(&same), Decision::Allow);

    let changed = Facts {
        config: config(true),
        receipt_present: receipt_for(state.path(), "tree2"),
        ..Facts::default()
    };
    assert!(matches!(decide(&changed), Decision::Deny(_)));
}

#[test]
fn skip_switch_and_enabled_false_allow() {
    let skip = Facts {
        config: config(true),
        skip: true,
        ..Facts::default()
    };
    assert_eq!(decide(&skip), Decision::Skipped);
    let disabled = Facts {
        config: config(false),
        ..Facts::default()
    };
    assert_eq!(decide(&disabled), Decision::Allow);
}

#[test]
fn skip_word_must_be_whole() {
    assert!(word_set("CLUD_SKIP_PR_CHECKS=1 git push", SKIP_ENV));
    assert!(!word_set("XCLUD_SKIP_PR_CHECKS=1 git push", SKIP_ENV));
    assert!(!word_set("CLUD_SKIP_PR_CHECKS=10 git push", SKIP_ENV));
}

#[test]
fn trigger_recognizes_push_and_pr_create() {
    for command in [
        "git push",
        "git push -u origin feat",
        "git -C /tmp/x push origin HEAD",
        "cd x && git push --force-with-lease",
        "CLUD_SKIP_PR_CHECKS=1 git push",
        "gh pr create --fill",
        "ghd gh pr create --title t --body b",
        "gh -R talos-hq/clud pr create",
        "/usr/bin/git push",
    ] {
        assert!(triggers(command), "{command}");
    }
    for command in [
        "git status",
        "git push --dry-run",
        "gh pr view 1",
        "gh pr list --search create",
        "echo git push",
        "git stash push -m x",
        "",
    ] {
        assert!(!triggers(command), "{command}");
    }
}

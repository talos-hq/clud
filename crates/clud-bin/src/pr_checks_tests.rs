use super::*;
use serde_json::json;

fn cmds() -> Vec<String> {
    vec!["bash lint --host-only".into(), "bash test --host".into()]
}

#[test]
fn no_key_means_no_config() {
    assert_eq!(config_from(&[json!({"gh": {"backend": "ghd"}})]), None);
    assert_eq!(config_from(&[json!({"pr_checks": {"commands": []}})]), None);
}

#[test]
fn highest_layer_owns_commands_and_enabled() {
    let local = json!({"pr_checks": {"enabled": false}});
    let repo = json!({"pr_checks": {"commands": ["bash lint --host-only", "bash test --host"]}});
    let user = json!({"pr_checks": {"commands": ["other"], "enabled": true}});
    let config = config_from(&[local, repo, user]).unwrap();
    assert_eq!(config.commands, cmds());
    assert!(!config.enabled);
}

#[test]
fn receipt_written_only_when_every_command_passes() {
    let dir = tempfile::tempdir().unwrap();
    let root = Path::new("/repo");
    let path = receipt_path(dir.path(), root, "abc", &cmds());

    let mut ran = Vec::new();
    let code = check_and_record(dir.path(), root, "abc", &cmds(), |c| {
        ran.push(c.to_string());
        if c.starts_with("bash lint") {
            3
        } else {
            0
        }
    });
    assert_eq!(code, 3);
    assert_eq!(
        ran,
        vec!["bash lint --host-only".to_string()],
        "stops at first failure"
    );
    assert!(!path.exists());

    let code = check_and_record(dir.path(), root, "abc", &cmds(), |_| 0);
    assert_eq!(code, 0);
    assert!(path.is_file());
}

#[test]
fn receipt_key_depends_on_tree_and_commands() {
    let dir = Path::new("/state");
    let root = Path::new("/repo");
    let base = receipt_path(dir, root, "abc", &cmds());
    assert_ne!(base, receipt_path(dir, root, "def", &cmds()));
    assert_ne!(base, receipt_path(dir, root, "abc", &["x".to_string()]));
    assert_ne!(base, receipt_path(dir, Path::new("/other"), "abc", &cmds()));
}

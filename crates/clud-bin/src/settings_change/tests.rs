use super::*;

fn world() -> (tempfile::TempDir, PathBuf, Value) {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".clud")).unwrap();
    let document = json!({
        "web_term": {"enabled": true}, "safe_rm": {"extra_roots": []},
        "unrelated": {"secret": "unrelated-secret-sentinel"},
    });
    fs::write(super::super::settings_path_at(&home), document.to_string()).unwrap();
    (directory, home, document)
}

fn setting(pointer: &str, value: &str) -> Mutation {
    Mutation::Set(Box::new(SetArgs {
        pointer: pointer.into(),
        value: value.into(),
        reason: "Review this particular change".into(),
    }))
}

#[test]
fn approved_operation_preserves_roots_and_unrelated_values_without_seeding() {
    let (directory, home, mut expected) = world();
    let root = directory.path().join("already-approved-data");
    fs::create_dir(&root).unwrap();
    expected["safe_rm"]["extra_roots"] = json!([
        {"path": root, "reason": "Preserve the existing user decision exactly"}
    ]);
    fs::write(super::super::settings_path_at(&home), expected.to_string()).unwrap();
    let path = execute(&home, &setting("/web_term/enabled", "false"), |preview| {
        assert!(!preview.roots);
        assert_eq!(preview.changes.len(), 1);
        assert_eq!(preview.changes[0].pointer, "/web_term/enabled");
        assert_eq!(preview.changes[0].before, Some(json!(true)));
        assert_eq!(preview.changes[0].after, Some(json!(false)));
        Ok(())
    })
    .unwrap();
    expected["web_term"]["enabled"] = json!(false);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
        expected
    );
    let audit = fs::read_to_string(home.join(".clud/settings-audit.jsonl")).unwrap();
    assert!(audit.contains("approved"));
    assert!(audit.contains("saved"));
    assert!(!audit.contains("unrelated-secret-sentinel"));
    assert!(!audit.contains("Review this particular change"));
}

#[test]
fn cancellation_and_unavailable_authority_leave_settings_and_audit_untouched() {
    for decision in [Failure::Cancelled, Failure::Unavailable] {
        let (_directory, home, _) = world();
        let path = super::super::settings_path_at(&home);
        let original = fs::read(&path).unwrap();
        assert!(
            execute(&home, &setting("/web_term/enabled", "false"), |_| Err(
                decision
            ))
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!home.join(".clud/settings-audit.jsonl").exists());
    }
}

#[test]
fn ordinary_operations_cannot_replace_or_remove_protected_roots() {
    let (_directory, home, _) = world();
    for mutation in [
        setting("/safe_rm", "null"),
        setting("/safe_rm", "{}"),
        setting("/safe_rm/extra_roots", "null"),
        setting(
            "/safe_rm/extra_roots",
            r#"[{"path":"/other","reason":"forged"}]"#,
        ),
    ] {
        assert!(matches!(
            execute(&home, &mutation, |_| panic!("must refuse before approval")),
            Err(Failure::Refused(_))
        ));
    }
}

#[test]
fn separate_root_operations_validate_paths_and_require_a_new_decision() {
    let (directory, home, _) = world();
    let root = directory.path().join("approved-data");
    fs::create_dir(&root).unwrap();
    let value = json!([{"path": root, "reason": "Owned test data"}]).to_string();
    let add = Mutation::Roots {
        value,
        reason: "Allow the reviewed data directory".into(),
    };
    execute(&home, &add, |preview| {
        assert!(preview.roots);
        assert_eq!(preview.changes[0].pointer, "/safe_rm/extra_roots");
        Ok(())
    })
    .unwrap();
    let path = super::super::settings_path_at(&home);
    let approved = fs::read(&path).unwrap();
    let remove = Mutation::Roots {
        value: "[]".into(),
        reason: "Revoke the data directory".into(),
    };
    assert!(matches!(
        execute(&home, &remove, |_| Err(Failure::Unavailable)),
        Err(Failure::Unavailable)
    ));
    assert_eq!(
        fs::read(&path).unwrap(),
        approved,
        "prior approval cannot authorize removal"
    );
    execute(&home, &remove, |_| Ok(())).unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["safe_rm"]["extra_roots"], json!([]));
}

#[test]
fn root_approval_cannot_relax_existing_validation() {
    let (directory, home, _) = world();
    let root = directory.path().join("data");
    fs::create_dir(&root).unwrap();
    for value in [
        json!([{"path": root, "reason": ""}]),
        json!([{"path": home, "reason": "home is excluded"}]),
        json!([{"path": "relative", "reason": "relative paths are excluded"}]),
        json!([{"path": root, "reason": "one"}, {"path": root, "reason": "two"}]),
        json!([{"path": root, "reason": "forged", "approved": true}]),
    ] {
        let change = Mutation::Roots {
            value: value.to_string(),
            reason: "Not authority".into(),
        };
        assert!(matches!(
            execute(&home, &change, |_| panic!(
                "invalid roots cannot reach approval"
            )),
            Err(Failure::Refused(_))
        ));
    }
}

#[test]
fn concurrent_changes_and_expired_review_cannot_reuse_authority() {
    let (_directory, home, _) = world();
    let path = super::super::settings_path_at(&home);
    let changed = br#"{"externally_changed":true}"#;
    let result = execute(&home, &setting("/web_term/enabled", "false"), |_| {
        fs::write(&path, changed).unwrap();
        Ok(())
    });
    assert!(matches!(result, Err(Failure::Changed)));
    assert_eq!(fs::read(&path).unwrap(), changed);

    // Advance the clock rather than back-dating `created`: Windows `Instant`
    // counts from boot and cannot go 601 s back on a fresh runner (#2031).
    let prepared = prepare(&home, &setting("/web_term/enabled", "true")).unwrap();
    let expired = prepared.preview.created + APPROVAL_LIFETIME + Duration::from_secs(1);
    assert!(matches!(
        commit_with_clock(prepared, || expired),
        Err(Failure::Changed)
    ));
    assert_eq!(fs::read(path).unwrap(), changed);
}

#[test]
fn previews_hide_unchanged_siblings_and_reject_invalid_pointers() {
    let (_directory, home, _) = world();
    let change = setting(
        "/unrelated",
        r#"{"secret":"unrelated-secret-sentinel","feature":true}"#,
    );
    let prepared = prepare(&home, &change).unwrap();
    assert_eq!(prepared.preview.changes.len(), 1);
    assert_eq!(prepared.preview.changes[0].pointer, "/unrelated/feature");
    for pointer in ["", "/", "web_term/enabled", "/invalid~2key", "/bad\nkey"] {
        assert!(prepare(&home, &setting(pointer, "false")).is_err());
    }
}

#[test]
fn approval_expiring_during_lock_and_validation_is_refused_before_audit() {
    let (_directory, home, _) = world();
    let path = super::super::settings_path_at(&home);
    let original = fs::read(&path).unwrap();
    let prepared = prepare(&home, &setting("/web_term/enabled", "false")).unwrap();
    let created = prepared.preview.created;
    let mut calls = 0;
    // A deterministic clock models time consumed by the real lock/validation
    // path, avoiding a long sleep or scheduler-dependent expiry fixture.
    let result = commit_with_clock(prepared, || {
        calls += 1;
        if calls == 1 {
            created
        } else {
            created + APPROVAL_LIFETIME + Duration::from_secs(1)
        }
    });
    assert_eq!(calls, 2, "must check again under the settings lock");
    assert!(matches!(result, Err(Failure::Changed)));
    assert_eq!(fs::read(path).unwrap(), original);
    assert!(!home.join(".clud/settings-audit.jsonl").exists());
}

#[test]
fn changed_script_input_cannot_expand_or_replay_an_approved_operation() {
    let (directory, home, mut expected) = world();
    let script_input = directory.path().join("proposal.json");
    fs::write(&script_input, "false").unwrap();
    let mutation = setting(
        "/web_term/enabled",
        &fs::read_to_string(&script_input).unwrap(),
    );
    execute(&home, &mutation, |_| {
        // Script inputs can change while the user reviews; the prepared
        // operation must retain exactly the original value and key scope.
        fs::write(&script_input, r#"{"enabled":true,"extra":"unapproved"}"#).unwrap();
        Ok(())
    })
    .unwrap();
    expected["web_term"]["enabled"] = json!(false);
    let path = super::super::settings_path_at(&home);
    let saved = fs::read(&path).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&saved).unwrap(), expected);
    let expanded = setting("/web_term", &fs::read_to_string(&script_input).unwrap());
    assert!(matches!(
        execute(&home, &expanded, |_| Err(Failure::Unavailable)),
        Err(Failure::Unavailable)
    ));
    assert_eq!(
        fs::read(path).unwrap(),
        saved,
        "new input requires a new decision"
    );
}

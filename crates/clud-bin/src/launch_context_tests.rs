use super::*;

const SECRET_KEY: &str = "sk-or-v1-TOPSECRETKEYSHOULDNOTLEAK";
const SESSION_ID: &str = "622e60e1-5376-44bd-be2c-059a72786a16";

type Ambient = Option<Option<u64>>;
type MaxRow = (
    Route,
    Ambient,
    Option<(u32, ContextWindowSource)>,
    Option<u32>,
    Setting,
);
type CompactRow = (Route, Ambient, Option<u32>, Setting);

fn facts(route: Route) -> Facts {
    Facts {
        route,
        ambient_max_context: None,
        ambient_compact_window: None,
        model_window: None,
        catalog_compact_window: None,
        codex_common_window: None,
    }
}

fn s(value: Option<u64>, source: Source) -> Setting {
    Setting { value, source }
}

#[test]
fn max_context_decision_table() {
    let served = Some((1_048_576, ContextWindowSource::Served));
    let catalog = Some((400_000, ContextWindowSource::Catalog));
    // (route, ambient, model window, codex common) -> expected
    let rows: &[MaxRow] = &[
        (
            Route::Direct,
            None,
            served,
            None,
            s(Some(1_048_576), Source::Served),
        ),
        (
            Route::Direct,
            None,
            catalog,
            None,
            s(Some(400_000), Source::Catalog),
        ),
        (Route::Direct, None, None, None, s(None, Source::Unset)),
        // push_default: an ambient value wins on the direct route.
        (
            Route::Direct,
            Some(Some(4242)),
            served,
            None,
            s(Some(4242), Source::Ambient),
        ),
        (
            Route::Direct,
            Some(None),
            served,
            None,
            s(None, Source::Ambient),
        ),
        // set_env: the Codex bridge overrides an ambient value.
        (
            Route::CodexBridge,
            Some(Some(4242)),
            None,
            Some(272_000),
            s(Some(272_000), Source::Catalog),
        ),
        (
            Route::CodexBridge,
            None,
            None,
            None,
            s(None, Source::Unknown),
        ),
        // The gateway and native launches never touch the key.
        (
            Route::UnifiedGateway,
            None,
            served,
            Some(1),
            s(None, Source::Unset),
        ),
        (
            Route::UnifiedGateway,
            Some(Some(7)),
            served,
            None,
            s(Some(7), Source::Ambient),
        ),
        (Route::Native, None, catalog, None, s(None, Source::Unset)),
        (
            Route::Native,
            Some(Some(9)),
            None,
            None,
            s(Some(9), Source::Ambient),
        ),
    ];
    for (route, ambient, window, codex, expected) in rows {
        let f = Facts {
            ambient_max_context: *ambient,
            model_window: *window,
            codex_common_window: *codex,
            ..facts(*route)
        };
        assert_eq!(
            decide_max_context(&f),
            *expected,
            "{route:?} {ambient:?} {window:?}"
        );
    }
}

#[test]
fn compact_window_decision_table() {
    let rows: &[CompactRow] = &[
        (
            Route::Direct,
            None,
            Some(180_000),
            s(Some(180_000), Source::Catalog),
        ),
        // The direct overlay scrubs an ambient compact window.
        (Route::Direct, Some(Some(5)), None, s(None, Source::Unset)),
        (
            Route::CodexBridge,
            Some(Some(5)),
            Some(1),
            s(Some(5), Source::Ambient),
        ),
        (Route::UnifiedGateway, None, Some(1), s(None, Source::Unset)),
        (
            Route::Native,
            Some(Some(6)),
            None,
            s(Some(6), Source::Ambient),
        ),
    ];
    for (route, ambient, catalog, expected) in rows {
        let f = Facts {
            ambient_compact_window: *ambient,
            catalog_compact_window: *catalog,
            ..facts(*route)
        };
        assert_eq!(decide_compact_window(&f), *expected, "{route:?}");
    }
}

#[test]
fn reconcile_takes_the_child_value_and_flags_surprises() {
    let served = s(Some(1_048_576), Source::Served);
    assert_eq!(reconcile(served, Some(Some(1_048_576))), served);
    assert_eq!(
        reconcile(served, Some(Some(200_000))),
        s(Some(200_000), Source::Unknown)
    );
    assert_eq!(reconcile(served, None), s(None, Source::Unknown));
    let unset = s(None, Source::Unset);
    assert_eq!(reconcile(unset, None), unset);
    assert_eq!(reconcile(unset, Some(Some(3))), s(Some(3), Source::Unknown));
}

#[test]
fn session_hash_is_the_documented_vector() {
    // The Python reader's test asserts the same vector
    // (tests/test_transcript_report.py::test_join_key_matches_the_writer).
    assert_eq!(session_hash(SESSION_ID), "eaf004849717de56");
    assert_ne!(session_hash(SESSION_ID), session_hash("other"));
}

fn plan_facts() -> PlanFacts {
    PlanFacts {
        route: Route::Direct,
        harness: "claude".into(),
        provider: "openrouter".into(),
        wire_model: Some("xiaomi/mimo-v2.6-flash".into()),
    }
}

fn hostile_env() -> Vec<(String, String)> {
    vec![
        ("ANTHROPIC_AUTH_TOKEN".into(), SECRET_KEY.into()),
        ("OPENROUTER_API_KEY".into(), SECRET_KEY.into()),
        ("CLAUDE_CODE_SESSION_ID".into(), SESSION_ID.into()),
        ("PWD".into(), "/home/user/secret-project".into()),
        ("CLUD_PROMPT".into(), "USER-PROMPT-SHOULD-NOT-LEAK".into()),
    ]
}

#[test]
fn record_contains_no_secrets_paths_or_raw_session_ids() {
    let mut child = hostile_env();
    child.push((MAX_CONTEXT_ENV.into(), "1048576".into()));
    let mut record = plan_facts().at_launch(&hostile_env(), &child, 1);
    record.session = Some(session_hash(SESSION_ID));
    let text = serde_json::to_string(&record).unwrap();
    for needle in [
        SECRET_KEY,
        SESSION_ID,
        "secret-project",
        "/home/",
        "USER-PROMPT",
        "TOKEN",
    ] {
        assert!(!text.contains(needle), "{needle} leaked: {text}");
    }
    assert!(text.len() < 512, "record is {} bytes", text.len());
}

#[test]
fn bind_copies_pending_to_the_hashed_name_and_prunes() {
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temp.path();
    let child = vec![(MAX_CONTEXT_ENV.to_string(), "200000".to_string())];
    let record = plan_facts().at_launch(&[], &child, 7);
    let token = write_pending(state, &record).expect("pending written");
    bind(state, &token, SESSION_ID);
    let bound: Record = serde_json::from_slice(
        &std::fs::read(dir(state).join(format!("{}.json", session_hash(SESSION_ID)))).unwrap(),
    )
    .unwrap();
    assert_eq!(
        bound.session.as_deref(),
        Some(session_hash(SESSION_ID).as_str())
    );
    assert_eq!(bound.max_context_tokens.value, Some(200_000));
    // A bad token or an empty session id writes nothing.
    bind(state, "../../etc/passwd", SESSION_ID);
    bind(state, &token, "");
    assert_eq!(std::fs::read_dir(dir(state)).unwrap().count(), 2);
}

#[test]
fn write_failure_is_silent() {
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    // A file where the directory should be: every write fails.
    std::fs::write(temp.path().join(DIR_NAME), b"x").unwrap();
    assert_eq!(write_pending(temp.path(), &plan_facts().preview(&[])), None);
    bind(temp.path(), "0123456789abcdef", SESSION_ID);
}

#[test]
fn prune_enforces_age_and_count() {
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let d = temp.path();
    for i in 0..(MAX_RECORDS + 5) {
        std::fs::write(d.join(format!("r{i}.json")), b"{}").unwrap();
    }
    std::fs::write(d.join("keep.txt"), b"x").unwrap();
    prune(d, SystemTime::now());
    let json = |d: &Path| {
        std::fs::read_dir(d)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count()
    };
    assert_eq!(json(d), MAX_RECORDS);
    // Everything is older than MAX_AGE as seen from 15 days in the future.
    prune(d, SystemTime::now() + MAX_AGE + Duration::from_secs(86_400));
    assert_eq!(json(d), 0);
    assert!(
        d.join("keep.txt").exists(),
        "non-record files are left alone"
    );
}

#[test]
fn preview_matches_launch_record_on_the_direct_route() {
    // The direct overlay's value for this served-map model, as the child
    // env carries it, must equal the dry-run prediction.
    let pf = plan_facts();
    let preview = pf.preview(&[]);
    let mut child = Vec::new();
    if let Some(value) = preview.max_context_tokens.value {
        child.push((MAX_CONTEXT_ENV.to_string(), value.to_string()));
    }
    if let Some(value) = preview.auto_compact_window.value {
        child.push((AUTO_COMPACT_WINDOW_ENV.to_string(), value.to_string()));
    }
    let launched = pf.at_launch(&[], &child, 0);
    assert_eq!(launched, preview);
}

use super::*;

#[test]
fn diagnostic_tool_fixtures_rank_candidates() {
    for (text, expected) in [
        (
            include_str!("../../tests/fixtures/gate/rustc.txt"),
            "error[E0308]: mismatched types",
        ),
        (
            include_str!("../../tests/fixtures/gate/clippy.txt"),
            "error: unneeded `return` statement",
        ),
        (
            include_str!("../../tests/fixtures/gate/ruff.txt"),
            "src/example.py:4:7: F821 Undefined name `missing`",
        ),
        (
            include_str!("../../tests/fixtures/gate/pytest.txt"),
            "tests/test_example.py:12: AssertionError",
        ),
        ("noise\n --> src/main.rs:3:2\n", "--> src/main.rs:3:2"),
        ("src/main.rs:3:2 — failure", "src/main.rs:3:2 — failure"),
    ] {
        assert_eq!(first_error(text).as_deref(), Some(expected));
    }
    assert_eq!(
        first_error("# t0=123\n# t0=2026-11-03T17:23:11\nduration:12:34\nChecking foo\nFAILED tests/test.py\n"),
        None
    );
}

fn fixture() -> (tempfile::TempDir, Vec<Element>) {
    let dir = tempfile::tempdir().unwrap();
    for name in ["check.py", "sum.py"] {
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/gate/pass")
                .join(name),
            dir.path().join(name),
        )
        .unwrap();
    }
    let python = which::which("python")
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let items = vec![
        element("lint", "lint", &[&python, "check.py"]),
        element("test", "test", &[&python, "check.py"]),
    ];
    (dir, items)
}

#[test]
fn green_fixture_emits_only_passed_names_under_400_bytes() {
    let (dir, items) = fixture();
    let logs = dir.path().join("logs");
    let payload = evaluate(
        Lane::Both,
        false,
        dir.path(),
        &logs,
        "20261103T000000-123456",
        &items,
    )
    .unwrap();
    assert_eq!(payload["status"], "pass");
    assert_eq!(payload["elements"], json!([]));
    assert_eq!(payload["passed_elements"], json!(["lint", "test"]));
    assert_eq!(payload["skipped"], json!([]));
    let bytes = payload.to_string().len();
    println!("green payload: {bytes} bytes\n{payload}");
    assert!(bytes < 400);
    assert!(logs.join("lint.log").is_file());
    assert!(logs.join("test.log").is_file());
}

#[test]
fn both_fast_fail_skips_test_with_reason_and_no_log() {
    let (dir, mut items) = fixture();
    items[0].argv = vec![
        items[0].argv[0].clone(),
        "-c".into(),
        "raise SystemExit(1)".into(),
    ];
    let logs = dir.path().join("logs");
    let payload = evaluate(Lane::Both, false, dir.path(), &logs, "red", &items).unwrap();
    assert_eq!(payload["status"], "fail");
    assert_eq!(payload["fast_fail"], true);
    assert_eq!(payload["elements"].as_array().unwrap().len(), 1);
    assert_eq!(
        payload["skipped"],
        json!([{"id":"test", "reason":"fast_fail: previous element failed"}])
    );
    assert!(!logs.join("test.log").exists());
    assert!(payload["elements"][0].get("first_error").is_none());
    let continued = evaluate(Lane::Both, true, dir.path(), &logs, "continue", &items).unwrap();
    assert_eq!(continued["passed_elements"], json!(["test"]));
    assert_eq!(continued["skipped"], json!([]));
}

#[test]
fn registry_detects_all_tools_and_orders_lint_first() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Cargo.toml"), "").unwrap();
    fs::write(dir.path().join("pyproject.toml"), "").unwrap();
    fs::write(
        dir.path().join("package.json"),
        r#"{"devDependencies":{"eslint":"*","vitest":"*","jest":"*"}}"#,
    )
    .unwrap();
    let items = detect(dir.path());
    for id in [
        "fmt", "clippy", "ruff", "pytest", "eslint", "vitest", "jest",
    ] {
        assert!(items.iter().any(|item| item.id == id));
    }
    let first_test = items.iter().position(|item| item.kind == "test").unwrap();
    assert!(items[first_test..].iter().all(|item| item.kind == "test"));
    assert!(items
        .iter()
        .filter(|item| ["fmt", "clippy", "nextest", "cargo-test"].contains(&item.id))
        .all(|item| item.argv[0] == "soldr"));
}

#[test]
fn cli_dispatch_keeps_gate_options_out_of_backend_passthrough() {
    let args = crate::args::Args::parse_from_raw(vec![
        "clud".into(),
        "gate".into(),
        "both".into(),
        "--repo".into(),
        ".".into(),
        "--no-fast-fail".into(),
    ]);
    assert!(matches!(
        args.command,
        Some(crate::args::Command::Gate {
            lane: Lane::Both,
            no_fast_fail: true,
            ..
        })
    ));
}

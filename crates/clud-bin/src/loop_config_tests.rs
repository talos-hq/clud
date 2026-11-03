use super::*;

fn full() -> LoopConfig {
    LoopConfig {
        schema_version: SCHEMA_VERSION,
        task: Some("t".into()),
        loop_count: Some(3),
        done: Some("D".into()),
        no_done: Some(false),
        repeat: Some("5m".into()),
        refresh: Some(true),
        message: Some("m".into()),
    }
}

#[test]
fn from_cli_only_sets_explicit_values() {
    let argv: Vec<String> = vec!["clud".into(), "loop".into(), "t".into()];
    let cli = LoopConfig::from_cli(&argv, Some("t"), 50, None, false, None, false, None);
    assert_eq!(
        cli.loop_count, None,
        "clap default must not override the file"
    );
    assert_eq!(cli.no_done, None);
    assert_eq!(cli.refresh, None);
    assert_eq!(cli.task.as_deref(), Some("t"));
    let argv: Vec<String> = ["clud", "loop", "--loop-count=50"]
        .map(String::from)
        .to_vec();
    let cli = LoopConfig::from_cli(&argv, None, 50, None, true, None, true, Some("m"));
    assert_eq!(cli.loop_count, Some(50));
    assert_eq!(cli.no_done, Some(true));
    assert_eq!(cli.refresh, Some(true));
    assert_eq!(cli.message.as_deref(), Some("m"));
}

#[test]
fn round_trips_through_json() {
    let cfg = full();
    let text = serde_json::to_string(&cfg).unwrap();
    assert_eq!(LoopConfig::parse(Path::new("x"), &text).unwrap(), cfg);
}

#[test]
fn minimal_file_needs_only_schema_version() {
    let cfg = LoopConfig::parse(Path::new("x"), r#"{"schema_version":1}"#).unwrap();
    assert_eq!(
        cfg,
        LoopConfig {
            schema_version: 1,
            ..LoopConfig::default()
        }
    );
}

#[test]
fn rejects_unknown_fields_wrong_version_and_conflicts() {
    let p = Path::new("loop.json");
    assert!(matches!(
        LoopConfig::parse(p, r#"{"schema_version":1,"loopcount":3}"#),
        Err(LoopConfigError::Parse { .. })
    ));
    assert!(matches!(
        LoopConfig::parse(p, r#"{"schema_version":2}"#),
        Err(LoopConfigError::Version { found: 2, .. })
    ));
    assert!(matches!(
        LoopConfig::parse(p, r#"{"task":"x"}"#),
        Err(LoopConfigError::Parse { .. })
    ));
    assert!(matches!(
        LoopConfig::parse(p, r#"{"schema_version":1,"done":"D","no_done":true}"#),
        Err(LoopConfigError::Conflict { .. })
    ));
}

#[test]
fn cli_values_override_file_values() {
    let file = full();
    let cli = LoopConfig {
        loop_count: Some(9),
        message: Some("cli".into()),
        ..LoopConfig::default()
    };
    let merged = file.overridden_by(&cli);
    assert_eq!(merged.loop_count, Some(9));
    assert_eq!(merged.message.as_deref(), Some("cli"));
    assert_eq!(merged.task.as_deref(), Some("t"));
    assert_eq!(merged.repeat.as_deref(), Some("5m"));
    assert_eq!(merged.schema_version, SCHEMA_VERSION);
}

#[test]
fn discovers_implicit_and_explicit_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert!(LoopConfig::discover(root, None).unwrap().is_none());

    std::fs::write(
        root.join(FILE_NAME),
        r#"{"schema_version":1,"loop_count":4}"#,
    )
    .unwrap();
    let (path, cfg) = LoopConfig::discover(root, None).unwrap().unwrap();
    assert_eq!(path, root.join(FILE_NAME));
    assert_eq!(cfg.loop_count, Some(4));

    let other = root.join("custom.json");
    std::fs::write(&other, r#"{"schema_version":1,"loop_count":7}"#).unwrap();
    let (path, cfg) = LoopConfig::discover(root, Some(&other)).unwrap().unwrap();
    assert_eq!(path, other);
    assert_eq!(cfg.loop_count, Some(7));

    let missing = root.join("missing.json");
    assert!(matches!(
        LoopConfig::discover(root, Some(&missing)),
        Err(LoopConfigError::Read { .. })
    ));
}

#[test]
fn writes_resolved_copy_under_loop_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let path = full().write_resolved(tmp.path()).unwrap();
    assert_eq!(path, LoopConfig::resolved_path(tmp.path()));
    let back = LoopConfig::parse(&path, &std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(back, full());
}

#[test]
fn schema_properties_match_struct_fields() {
    let schema: serde_json::Value = serde_json::from_str(JSON_SCHEMA).unwrap();
    let mut schema_keys: Vec<String> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let serialized = serde_json::to_value(full()).unwrap();
    let mut struct_keys: Vec<String> = serialized.as_object().unwrap().keys().cloned().collect();
    schema_keys.sort();
    struct_keys.sort();
    assert_eq!(schema_keys, struct_keys);
    assert_eq!(schema["additionalProperties"], false);
}

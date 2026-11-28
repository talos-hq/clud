//! `.clud/hooks.json` parsing and matcher semantics (#967 Phase 2).

use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs;

fn entry(matcher: Option<&str>, command: &str) -> HookEntry {
    HookEntry {
        matcher: matcher.map(ToOwned::to_owned),
        command: command.to_string(),
        timeout_secs: DEFAULT_TIMEOUT_SECS,
    }
}

#[test]
fn the_documented_shape_parses() {
    let hooks = parse(
        r#"{
          "hooks": {
            "PreToolUse": [
              { "matcher": "Bash", "command": "uv run python ci/hooks/check_cmd.py" }
            ],
            "Stop": [
              { "command": "uv run python ci/hooks/check_on_stop.py", "timeout": 120 }
            ]
          }
        }"#,
    )
    .expect("parses");

    assert_eq!(
        hooks.events().collect::<Vec<_>>(),
        vec!["PreToolUse", "Stop"]
    );
    assert_eq!(
        hooks.for_event("PreToolUse"),
        [entry(Some("Bash"), "uv run python ci/hooks/check_cmd.py")]
    );
    assert_eq!(hooks.for_event("Stop")[0].timeout_secs, 120);
    assert_eq!(hooks.for_event("Stop")[0].matcher, None);
}

#[test]
fn an_empty_or_hookless_document_declares_nothing() {
    assert!(parse("").expect("parses").is_empty());
    assert!(parse("{}").expect("parses").is_empty());
    assert!(parse(r#"{"hooks":{}}"#).expect("parses").is_empty());
}

#[test]
fn only_an_unusable_document_is_an_error() {
    assert!(parse("{not json").is_err());
    assert!(parse("[]").is_err(), "a top-level array is not a document");
}

#[test]
fn one_malformed_entry_does_not_take_the_others_down() {
    // The same posture as `repo_clud_config`: a settings file that disarms
    // itself over one typo is worse than one that runs what it still
    // understands.
    let hooks = parse(
        r#"{
          "hooks": {
            "PreToolUse": [
              { "command": "" },
              { "matcher": "Bash" },
              "not-an-object",
              { "command": "real-guard" }
            ]
          }
        }"#,
    )
    .expect("document still parses");

    assert_eq!(hooks.for_event("PreToolUse"), [entry(None, "real-guard")]);
}

#[test]
fn a_bad_timeout_drops_only_its_own_entry() {
    let hooks = parse(
        r#"{"hooks":{"Stop":[{"command":"a","timeout":0},{"command":"b","timeout":"soon"},{"command":"c"}]}}"#,
    )
    .expect("parses");
    assert_eq!(hooks.for_event("Stop"), [entry(None, "c")]);
}

#[test]
fn unknown_top_level_keys_and_unknown_events_are_tolerated() {
    // Forward compatibility: a future schema key, or an event this clud has
    // not learned to fire, must not break an older/newer build.
    let hooks = parse(r#"{"version":2,"hooks":{"SomeFutureEvent":[{"command":"later"}]}}"#)
        .expect("parses");
    assert_eq!(hooks.for_event("SomeFutureEvent"), [entry(None, "later")]);
    assert!(hooks.for_event("PreToolUse").is_empty());
}

#[test]
fn a_non_array_event_is_skipped_not_fatal() {
    let hooks =
        parse(r#"{"hooks":{"Stop":"nope","PreToolUse":[{"command":"ok"}]}}"#).expect("parses");
    assert!(hooks.for_event("Stop").is_empty());
    assert_eq!(hooks.for_event("PreToolUse"), [entry(None, "ok")]);
}

// -----------------------------------------------------------------
// Matcher semantics.
// -----------------------------------------------------------------

#[test]
fn an_absent_or_star_matcher_matches_every_tool() {
    for matcher in [None, Some("*"), Some("  ")] {
        let entry = entry(matcher, "cmd");
        assert!(entry.matches_tool(Some("Bash")), "{matcher:?}");
        assert!(entry.matches_tool(Some("Edit")), "{matcher:?}");
        assert!(entry.matches_tool(None), "{matcher:?}");
    }
}

#[test]
fn a_matcher_is_a_regex_over_the_tool_name() {
    let entry = entry(Some("Edit|Write"), "cmd");
    assert!(entry.matches_tool(Some("Edit")));
    assert!(entry.matches_tool(Some("Write")));
    assert!(!entry.matches_tool(Some("Bash")));
}

#[test]
fn matchers_are_anchored_so_edit_does_not_catch_multiedit() {
    let entry = entry(Some("Edit"), "cmd");
    assert!(entry.matches_tool(Some("Edit")));
    assert!(
        !entry.matches_tool(Some("MultiEdit")),
        "an unanchored match would fire this guard against a tool it never named"
    );
}

#[test]
fn an_uncompilable_matcher_under_matches_rather_than_over_matches() {
    // Failing open here would run someone's guard against tools they never
    // meant to guard, which is worse than not running it.
    let entry = entry(Some("Edit("), "cmd");
    assert!(entry.matches_tool(Some("Edit(")), "falls back to equality");
    assert!(!entry.matches_tool(Some("Edit")));
    assert!(!entry.matches_tool(Some("anything")));
}

#[test]
fn a_tool_scoped_matcher_does_not_fire_on_an_event_without_a_tool() {
    let entry = entry(Some("Bash"), "cmd");
    assert!(!entry.matches_tool(None));
}

#[test]
fn matching_filters_and_preserves_declaration_order() {
    let hooks = parse(
        r#"{"hooks":{"PreToolUse":[
             {"matcher":"Bash","command":"first"},
             {"matcher":"Edit","command":"skipped"},
             {"command":"second"}
           ]}}"#,
    )
    .expect("parses");

    let commands: Vec<&str> = hooks
        .matching("PreToolUse", Some("Bash"))
        .iter()
        .map(|entry| entry.command.as_str())
        .collect();
    assert_eq!(commands, vec!["first", "second"]);
}

// -----------------------------------------------------------------
// Discovery.
// -----------------------------------------------------------------

fn write_hooks(root: &std::path::Path, body: &str) {
    let dir = root.join(".clud");
    fs::create_dir_all(&dir).expect("mkdir .clud");
    fs::write(dir.join("hooks.json"), body).expect("write hooks.json");
}

#[test]
fn discovery_reads_the_repo_declaration_and_records_its_source() {
    let tmp = TemporaryDirectory::new().unwrap();
    write_hooks(tmp.path(), r#"{"hooks":{"Stop":[{"command":"guard"}]}}"#);

    let hooks = discover(tmp.path()).expect("declared");
    assert_eq!(hooks.for_event("Stop"), [entry(None, "guard")]);
    assert_eq!(
        hooks.source,
        Some(tmp.path().join(".clud").join("hooks.json"))
    );
}

#[test]
fn no_file_no_declarations_and_an_empty_one_counts_as_none() {
    let tmp = TemporaryDirectory::new().unwrap();
    assert_eq!(discover(tmp.path()), None);

    write_hooks(tmp.path(), r#"{"hooks":{"Stop":[]}}"#);
    assert_eq!(
        discover(tmp.path()),
        None,
        "declaring nothing is not opting in"
    );
}

#[test]
fn an_unparsable_declaration_is_reported_and_ignored_not_fatal() {
    // A broken declaration must not stop the tool call that triggered the
    // lookup -- that is the wedge this whole feature exists to prevent.
    let tmp = TemporaryDirectory::new().unwrap();
    write_hooks(tmp.path(), "{not json");
    assert_eq!(discover(tmp.path()), None);
}

// --- Tier B source: from_frontend_settings (#967 Phase 4, #966 D4) ---

fn write_settings(root: &Path, rel: &[&str], text: &str) {
    let mut path = root.to_path_buf();
    for segment in rel {
        path.push(segment);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

#[test]
fn frontend_source_reads_the_claude_group_shape() {
    let tmp = TemporaryDirectory::new().unwrap();
    write_settings(
        tmp.path(),
        &[".claude", "settings.json"],
        r#"{
          "permissions": { "additionalDirectories": ["/srv/other"] },
          "hooks": {
            "PreToolUse": [
              {
                "matcher": "Bash",
                "hooks": [
                  { "type": "command", "command": "ci/hooks/check.py", "timeout": 15 },
                  { "type": "stdin", "command": "ignored-kind" }
                ]
              }
            ],
            "Stop": [ { "command": "ci/hooks/on_stop.py" } ]
          }
        }"#,
    );

    let hooks = from_frontend_settings(tmp.path()).expect("declares hooks");
    assert_eq!(
        hooks.for_event("PreToolUse"),
        [HookEntry {
            matcher: Some("Bash".to_string()),
            command: "ci/hooks/check.py".to_string(),
            timeout_secs: 15,
        }],
        "the `type: stdin` handler must not be executed by clud"
    );
    assert_eq!(
        hooks.for_event("Stop"),
        [entry(None, "ci/hooks/on_stop.py")]
    );
}

#[test]
fn frontend_source_reads_the_legacy_direct_shape_and_single_group() {
    let tmp = TemporaryDirectory::new().unwrap();
    write_settings(
        tmp.path(),
        &[".claude", "settings.json"],
        r#"{
          "hooks": {
            "PreToolUse": [
              { "matcher": "Bash", "command": "guard-bash" },
              { "command": "guard-all" }
            ]
          }
        }"#,
    );
    write_settings(
        tmp.path(),
        &[".claude", "settings.local.json"],
        r#"{
          "hooks": {
            "Stop": { "command": "on-stop" }
          }
        }"#,
    );

    let hooks = from_frontend_settings(tmp.path()).expect("declares hooks");
    assert_eq!(
        hooks.for_event("PreToolUse"),
        [entry(Some("Bash"), "guard-bash"), entry(None, "guard-all")]
    );
    assert_eq!(hooks.for_event("Stop"), [entry(None, "on-stop")]);
}

#[test]
fn frontend_source_layers_shared_then_local_without_double_firing() {
    let tmp = TemporaryDirectory::new().unwrap();
    write_settings(
        tmp.path(),
        &[".claude", "settings.json"],
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","command":"same-guard"}]}}"#,
    );
    // The local file extends the shared one, exactly as the harness merges
    // them; a hook in both must fire once.
    write_settings(
        tmp.path(),
        &[".claude", "settings.local.json"],
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","command":"same-guard"},{"command":"local-only"}]}}"#,
    );

    let hooks = from_frontend_settings(tmp.path()).expect("declares hooks");
    assert_eq!(
        hooks.for_event("PreToolUse"),
        [entry(Some("Bash"), "same-guard"), entry(None, "local-only")],
        "the duplicate triple is deduped"
    );
}

#[test]
fn frontend_source_reads_the_codex_shapes() {
    let tmp = TemporaryDirectory::new().unwrap();
    write_settings(
        tmp.path(),
        &[".codex", "hooks.json"],
        r#"{
          "hooks": {
            "PreToolUse": [
              { "matcher": "Bash", "hooks": [ { "type": "command", "command": "codex-guard" } ] }
            ],
            "state": { "whatever": { "trusted_hash": "x" } }
          }
        }"#,
    );

    let hooks = from_frontend_settings(tmp.path()).expect("declares hooks");
    assert_eq!(
        hooks.for_event("PreToolUse"),
        [entry(Some("Bash"), "codex-guard")],
        "hooks.state is codex's trust table, not an event"
    );
}

#[test]
fn frontend_source_accepts_the_codex_root_level_legacy_shape() {
    let tmp = TemporaryDirectory::new().unwrap();
    write_settings(
        tmp.path(),
        &[".codex", "hooks.json"],
        r#"{
          "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "legacy-guard" } ] } ]
        }"#,
    );

    let hooks = from_frontend_settings(tmp.path()).expect("declares hooks");
    assert_eq!(
        hooks.for_event("PreToolUse"),
        [entry(Some("Bash"), "legacy-guard")]
    );
}

#[test]
fn frontend_source_never_treats_claude_root_keys_as_events() {
    let tmp = TemporaryDirectory::new().unwrap();
    write_settings(
        tmp.path(),
        &[".claude", "settings.json"],
        r#"{
          "permissions": { "allow": ["Bash"] },
          "model": "sonnet"
        }"#,
    );

    assert_eq!(
        from_frontend_settings(tmp.path()),
        None,
        "permissions and model are settings, not hook events"
    );
}

#[test]
fn frontend_source_none_when_nothing_declared() {
    let tmp = TemporaryDirectory::new().unwrap();
    assert_eq!(from_frontend_settings(tmp.path()), None);

    write_settings(tmp.path(), &[".claude", "settings.json"], "{}");
    assert_eq!(from_frontend_settings(tmp.path()), None);

    write_settings(tmp.path(), &[".claude", "settings.json"], "{not json");
    assert_eq!(
        from_frontend_settings(tmp.path()),
        None,
        "a broken file is reported and treated as absent, never fatal"
    );
}

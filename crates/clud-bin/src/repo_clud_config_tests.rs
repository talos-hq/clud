use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs;

fn write_settings(root: &Path, body: &str) {
    let dir = root.join(".clud");
    fs::create_dir_all(&dir).expect("mkdir .clud");
    fs::write(dir.join("settings.json"), body).expect("write settings.json");
}

fn write_local_settings(root: &Path, body: &str) {
    let dir = root.join(".clud");
    fs::create_dir_all(&dir).expect("mkdir .clud");
    fs::write(dir.join(LOCAL_SETTINGS_FILE), body).expect("write settings.local.json");
}

fn mark_repo_root(root: &Path) {
    fs::create_dir_all(root.join(".git")).expect("mkdir .git");
}

// -----------------------------------------------------------------
// Issue #525: the repo-local override layer.
// -----------------------------------------------------------------

#[test]
fn a_local_only_settings_file_is_honored() {
    // The reported bug: a rule in the gitignored override file was
    // silently ignored. Note there is deliberately no `settings.json`
    // here — requiring one would keep a local-only override invisible,
    // which is the whole point of the file.
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_local_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"local-rule","match":"playwright","replacement":"npm run test"}]}"#,
    );
    let cfg = discover_repo_clud_config(tmp.path()).expect("local settings must be found");
    assert_eq!(cfg.bad_commands.len(), 1);
    assert_eq!(cfg.bad_commands[0].id.as_deref(), Some("local-rule"));
}

#[test]
fn local_settings_win_over_shared_for_scalars() {
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_settings(tmp.path(), r#"{"rust":{"use_soldr":true}}"#);
    write_local_settings(tmp.path(), r#"{"rust":{"use_soldr":false}}"#);
    let cfg = discover_repo_clud_config(tmp.path()).expect("config");
    assert!(!cfg.rust.use_soldr, "repo-local must win over repo");
}

#[test]
fn shared_rust_fields_fall_through_except_the_rolling_version_policy() {
    // Most fields still fall through from the shared layer. Version is the
    // deliberate exception: any local Rust directive with no version selects
    // rolling latest instead of reviving a lower layer's numeric pin.
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_settings(
        tmp.path(),
        r#"{"rust":{"use_soldr":false,"install":false,"version":"1.90.0"}}"#,
    );
    write_local_settings(tmp.path(), r#"{"rust":{"use_soldr":true}}"#);
    let cfg = discover_repo_clud_config(tmp.path()).expect("config");
    assert!(cfg.rust.use_soldr, "local override applies");
    assert!(!cfg.rust.install, "untouched shared field must survive");
    assert_eq!(cfg.rust.version, None, "local omission selects latest");
}

#[test]
fn bad_command_rules_concatenate_across_the_local_layer() {
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"shared","match":"curl","replacement":"shared repl"}]}"#,
    );
    write_local_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"local","match":"wget","replacement":"local repl"}]}"#,
    );
    let cfg = discover_repo_clud_config(tmp.path()).expect("config");
    let ids: Vec<Option<&str>> = cfg.bad_commands.iter().map(|r| r.id.as_deref()).collect();
    assert_eq!(ids.len(), 2, "both layers' rules must survive: {ids:?}");
    assert!(ids.contains(&Some("local")) && ids.contains(&Some("shared")));
}

#[test]
fn a_local_rule_replaces_the_shared_rule_with_the_same_id() {
    // Dedupe by id is what lets a developer retune one shared rule
    // without restating the whole list.
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"dupe","match":"curl","replacement":"shared repl"}]}"#,
    );
    write_local_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"dupe","match":"curl","replacement":"local repl"}]}"#,
    );
    let cfg = discover_repo_clud_config(tmp.path()).expect("config");
    assert_eq!(cfg.bad_commands.len(), 1, "same id must not duplicate");
    assert_eq!(cfg.bad_commands[0].replacement, "local repl");
}

#[test]
fn a_malformed_local_file_does_not_discard_the_shared_config() {
    // Fail-open, matching the existing behaviour for a malformed
    // settings.json: a broken override must not silently disarm the
    // shared rules it was meant to extend.
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"shared","match":"curl","replacement":"shared repl"}]}"#,
    );
    write_local_settings(tmp.path(), "{ not json");
    let cfg = discover_repo_clud_config(tmp.path()).expect("shared config must survive");
    assert_eq!(cfg.bad_commands.len(), 1);
    assert_eq!(cfg.bad_commands[0].id.as_deref(), Some("shared"));
}

// -----------------------------------------------------------------
// Issue #525 part 2: rule provenance through parse + merge.
// -----------------------------------------------------------------

#[test]
fn parsed_rule_records_its_array_index() {
    let cfg = parse_repo_clud_config(
        r#"{"bad_commands":[{"match":"a","replacement":"x"},{"match":"b","replacement":"y"}]}"#,
    )
    .expect("parses");
    let indices: Vec<usize> = cfg
        .bad_commands
        .iter()
        .map(|r| r.source.as_ref().expect("source").index)
        .collect();
    assert_eq!(indices, vec![0, 1]);
    // No backing file for a string parse.
    assert!(cfg.bad_commands[0].source.as_ref().unwrap().file.is_none());
}

#[test]
fn a_skipped_malformed_rule_does_not_shift_later_indices() {
    // Rule 0 is malformed (missing replacement) and dropped; the surviving
    // rule must still report its real slot, index 1, so the JSON pointer is
    // accurate against the on-disk array.
    let cfg = parse_repo_clud_config(
        r#"{"bad_commands":[{"match":"broken"},{"id":"ok","match":"b","replacement":"y"}]}"#,
    )
    .expect("parses");
    assert_eq!(cfg.bad_commands.len(), 1);
    assert_eq!(cfg.bad_commands[0].source.as_ref().unwrap().index, 1);
}

#[test]
fn reading_from_a_file_stamps_canonical_file_and_layer() {
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_local_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"r","match":"playwright","replacement":"npm test"}]}"#,
    );
    let cfg = discover_repo_clud_config(tmp.path()).expect("config");
    let source = cfg.bad_commands[0].source.as_ref().expect("source");
    assert_eq!(source.layer.as_deref(), Some("repo-local"));
    let file = source.file.as_ref().expect("file stamped");
    assert!(
        file.ends_with("settings.local.json"),
        "unexpected file {file:?}"
    );
    assert_eq!(source.pointer(), "/bad_commands/0");
    assert!(source
        .reference()
        .ends_with("settings.local.json#/bad_commands/0"));
}

#[test]
fn winning_dedupe_rule_keeps_its_own_provenance() {
    // A repo-local rule shadows the repo rule with the same id; the
    // surviving rule must carry the *local* layer, not the shared one.
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"dupe","match":"curl","replacement":"shared"}]}"#,
    );
    write_local_settings(
        tmp.path(),
        r#"{"bad_commands":[{"id":"dupe","match":"curl","replacement":"local"}]}"#,
    );
    let cfg = discover_repo_clud_config(tmp.path()).expect("config");
    assert_eq!(cfg.bad_commands.len(), 1);
    assert_eq!(cfg.bad_commands[0].replacement, "local");
    assert_eq!(
        cfg.bad_commands[0]
            .source
            .as_ref()
            .unwrap()
            .layer
            .as_deref(),
        Some("repo-local"),
        "the winning (local) definition's provenance must be retained"
    );
}

#[test]
fn rule_source_reference_without_a_file_is_pointer_only() {
    let source = RuleSource {
        index: 3,
        file: None,
        layer: None,
    };
    assert_eq!(source.reference(), "#/bad_commands/3");
    assert_eq!(source.pointer(), "/bad_commands/3");
}

// -----------------------------------------------------------------
// Parser tests.
// -----------------------------------------------------------------

#[test]
fn empty_body_returns_default_resolved_config() {
    let cfg = parse_repo_clud_config("").expect("empty body parses");
    assert_eq!(cfg, RepoCludConfig::default());
    assert!(cfg.rust.use_soldr);
    assert!(cfg.rust.install);
    assert_eq!(cfg.rust.version, None);
}

#[test]
fn empty_body_returns_all_none_raw() {
    let raw = parse_raw_repo_clud_config("").expect("parses");
    assert_eq!(raw.rust.use_soldr, None);
    assert_eq!(raw.rust.install, None);
    assert_eq!(raw.rust.version, None);
}

#[test]
fn empty_object_resolves_to_defaults() {
    let cfg = parse_repo_clud_config("{}").expect("parses");
    assert_eq!(cfg, RepoCludConfig::default());
}

#[test]
fn missing_rust_key_resolves_to_defaults() {
    let cfg = parse_repo_clud_config(r#"{"python":{}}"#).expect("parses");
    assert_eq!(cfg, RepoCludConfig::default());
}

#[test]
fn full_rust_object_parses() {
    let cfg =
        parse_repo_clud_config(r#"{"rust":{"use_soldr":true,"install":true,"version":"0.7.55"}}"#)
            .expect("parses");
    assert!(cfg.rust.use_soldr);
    assert!(cfg.rust.install);
    assert_eq!(cfg.rust.version.as_deref(), Some("0.7.55"));
}

#[test]
fn optimize_rust_object_parses_as_activation_config() {
    let cfg = parse_repo_clud_config(
            r#"{"optimize":{"rust":{"use_soldr_shims":false,"install_soldr":false,"soldr_version":"0.7.11"}}}"#,
        )
        .expect("parses");
    assert!(!cfg.rust.use_soldr);
    assert!(!cfg.rust.install);
    assert_eq!(cfg.rust.version.as_deref(), Some("0.7.11"));
}

#[test]
fn omitted_and_latest_soldr_versions_are_the_same_rolling_policy() {
    let omitted = parse_repo_clud_config(r#"{"rust":{"use_soldr":true,"install":true}}"#)
        .expect("omitted version config");
    let latest =
        parse_repo_clud_config(r#"{"rust":{"use_soldr":true,"install":true,"version":"latest"}}"#)
            .expect("latest version config");
    assert_eq!(omitted.rust.version, None);
    assert_eq!(latest.rust.version, omitted.rust.version);
}

#[test]
fn optimize_latest_alias_normalizes_to_rolling_policy() {
    let config = parse_repo_clud_config(r#"{"optimize":{"rust":{"soldr_version":"LATEST"}}}"#)
        .expect("optimize latest version config");
    assert_eq!(config.rust.version, None);
}

#[test]
fn direct_rust_keys_win_over_optimize_rust_keys_in_same_file() {
    let cfg = parse_repo_clud_config(
            r#"{"rust":{"use_soldr":false,"version":"2.0.0"},"optimize":{"rust":{"use_soldr_shims":true,"soldr_version":"1.0.0"}}}"#,
        )
        .expect("parses");
    assert!(!cfg.rust.use_soldr);
    assert_eq!(cfg.rust.version.as_deref(), Some("2.0.0"));
}

#[test]
fn explicit_use_soldr_false_is_honored() {
    let cfg = parse_repo_clud_config(r#"{"rust":{"use_soldr":false}}"#).expect("parses");
    assert!(!cfg.rust.use_soldr);
}

#[test]
fn explicit_install_false_is_honored() {
    let cfg = parse_repo_clud_config(r#"{"rust":{"install":false}}"#).expect("parses");
    assert!(!cfg.rust.install);
    assert!(cfg.rust.use_soldr, "use_soldr should default to true");
}

#[test]
fn empty_version_string_is_treated_as_unset() {
    let cfg = parse_repo_clud_config(r#"{"rust":{"version":""}}"#).expect("parses");
    assert_eq!(cfg.rust.version, None);
}

#[test]
fn whitespace_version_string_is_treated_as_unset() {
    let cfg = parse_repo_clud_config(r#"{"rust":{"version":"   "}}"#).expect("parses");
    assert_eq!(cfg.rust.version, None);
}

#[test]
fn unknown_rust_field_is_ignored_for_forward_compat() {
    let cfg = parse_repo_clud_config(r#"{"rust":{"use_soldr":true,"gc_after_install":true}}"#)
        .expect("parses");
    assert!(cfg.rust.use_soldr);
}

#[test]
fn malformed_json_returns_err() {
    let err = parse_repo_clud_config("{\"rust\":{").unwrap_err();
    assert!(!err.is_empty(), "non-empty error message");
}

// -----------------------------------------------------------------
// Merge tests.
// -----------------------------------------------------------------

#[test]
fn merge_repo_overrides_user_per_field() {
    let user = parse_raw_repo_clud_config(
        r#"{"rust":{"use_soldr":true,"install":true,"version":"1.0.0"}}"#,
    )
    .unwrap();
    let repo =
        parse_raw_repo_clud_config(r#"{"rust":{"use_soldr":false,"version":"2.0.0"}}"#).unwrap();

    let merged = resolve(merge(repo, user));
    assert!(!merged.rust.use_soldr, "repo wins");
    assert!(merged.rust.install, "repo unset → user wins");
    assert_eq!(merged.rust.version.as_deref(), Some("2.0.0"), "repo wins");
}

#[test]
fn repo_omission_overrides_a_user_pin_with_rolling_latest() {
    let user = parse_raw_repo_clud_config(
        r#"{"rust":{"use_soldr":true,"install":true,"version":"1.0.0"}}"#,
    )
    .unwrap();
    let repo = parse_raw_repo_clud_config(r#"{"rust":{"use_soldr":true,"install":true}}"#).unwrap();
    let merged = resolve(merge(repo, user));
    assert_eq!(merged.rust.version, None);
}

#[test]
fn unrelated_repo_settings_do_not_override_a_user_pin() {
    let user =
        parse_raw_repo_clud_config(r#"{"rust":{"use_soldr":true,"version":"1.0.0"}}"#).unwrap();
    let repo = parse_raw_repo_clud_config(r#"{"bash":{"block_cd":true}}"#).unwrap();
    let merged = resolve(merge(repo, user));
    assert_eq!(merged.rust.version.as_deref(), Some("1.0.0"));
}

#[test]
fn merge_repo_optimize_overrides_user_rust_per_field() {
    let user = parse_raw_repo_clud_config(
        r#"{"rust":{"use_soldr":false,"install":false,"version":"1.0.0"}}"#,
    )
    .unwrap();
    let repo = parse_raw_repo_clud_config(
        r#"{"optimize":{"rust":{"use_soldr_shims":true,"soldr_version":"2.0.0"}}}"#,
    )
    .unwrap();

    let merged = resolve(merge(repo, user));
    assert!(merged.rust.use_soldr, "repo optimize wins");
    assert!(
        !merged.rust.install,
        "repo unset falls through to user rust field"
    );
    assert_eq!(merged.rust.version.as_deref(), Some("2.0.0"));
}

#[test]
fn unrelated_user_settings_do_not_enable_global_soldr_activation() {
    let user = parse_raw_repo_clud_config(r#"{"shell":{"disable_powershell":true}}"#).unwrap();
    assert_eq!(resolve_effective_config(None, Some(user)), None);
}

#[test]
fn merge_user_only_provides_defaults_when_repo_missing() {
    let user =
        parse_raw_repo_clud_config(r#"{"rust":{"install":false,"version":"3.0.0"}}"#).unwrap();
    let repo = RawRepoCludConfig::default();

    let merged = resolve(merge(repo, user));
    assert!(merged.rust.use_soldr, "neither set → default true");
    assert!(!merged.rust.install, "user wins when repo unset");
    assert_eq!(merged.rust.version.as_deref(), Some("3.0.0"));
}

#[test]
fn merge_both_empty_resolves_to_baked_defaults() {
    let merged = resolve(merge(
        RawRepoCludConfig::default(),
        RawRepoCludConfig::default(),
    ));
    assert_eq!(merged, RepoCludConfig::default());
}

// -----------------------------------------------------------------
// Repo-level discovery.
// -----------------------------------------------------------------

#[test]
fn discover_finds_at_marked_repo_root() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    mark_repo_root(root);
    write_settings(root, r#"{"rust":{"use_soldr":true,"version":"1.2.3"}}"#);

    let cfg = discover_repo_clud_config(root).expect("found");
    assert!(cfg.rust.use_soldr);
    assert_eq!(cfg.rust.version.as_deref(), Some("1.2.3"));
}

#[test]
fn discover_finds_from_subdirectory() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = tmp.path();
    mark_repo_root(root);
    write_settings(root, r#"{"rust":{"use_soldr":true}}"#);
    let sub = root.join("crates").join("clud-bin").join("src");
    fs::create_dir_all(&sub).unwrap();

    let cfg = discover_repo_clud_config(&sub).expect("found from subdir");
    assert!(cfg.rust.use_soldr);
}

#[test]
fn missing_settings_returns_none() {
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    assert!(discover_repo_clud_config(tmp.path()).is_none());
}

#[test]
fn discover_stops_at_git_root_boundary() {
    let tmp = TemporaryDirectory::new().unwrap();
    let outer = tmp.path();
    let repo = outer.join("repo");
    fs::create_dir_all(&repo).unwrap();
    mark_repo_root(&repo);
    write_settings(outer, r#"{"rust":{"use_soldr":true}}"#);

    assert!(
        discover_repo_clud_config(&repo).is_none(),
        "must not bleed across repo boundary"
    );
}

// Note: a `walk-without-git-dir-anywhere` test was considered but is
// fundamentally fragile — the OS temp dir's ancestors may contain a
// real user-level `~/.clud/settings.json` on the test host, which the
// walk would legitimately pick up. The `missing_settings_returns_none`
// case (which plants a `.git/` boundary explicitly) already covers
// the "no settings found" branch; the no-`.git`-anywhere edge is
// exercised in production by behavior, not by a test.

#[test]
fn malformed_settings_is_warned_and_skipped() {
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_settings(tmp.path(), "{not valid json");
    assert!(discover_repo_clud_config(tmp.path()).is_none());
}

// -----------------------------------------------------------------
// `bad_commands` (zackees/clud#519).
// -----------------------------------------------------------------

#[test]
fn bad_commands_array_parses_from_repo_settings() {
    let cfg = parse_repo_clud_config(
            r#"{"bad_commands":[{"id":"no-raw-playwright","match":"playwright","match_mode":"glob","replacement":"npm run test:integration","reason":"use the blessed pipeline","passthrough_prefixes":["soldr"],"allow_override":true}]}"#,
        )
        .expect("parses");
    assert_eq!(cfg.bad_commands.len(), 1);
    let rule = &cfg.bad_commands[0];
    assert_eq!(rule.id.as_deref(), Some("no-raw-playwright"));
    assert_eq!(rule.pattern, "playwright");
    assert_eq!(rule.match_mode, MatchMode::Glob);
    assert_eq!(rule.replacement, "npm run test:integration");
    assert_eq!(rule.reason, "use the blessed pipeline");
    assert_eq!(rule.passthrough_prefixes, vec!["soldr".to_string()]);
    assert!(rule.allow_override);
}

#[test]
fn bad_commands_array_parses_with_only_required_fields() {
    let cfg = parse_repo_clud_config(
        r#"{"bad_commands":[{"match":"playwright","replacement":"npm run test:integration"}]}"#,
    )
    .expect("parses");
    let rule = &cfg.bad_commands[0];
    assert_eq!(rule.id, None);
    assert_eq!(rule.match_mode, MatchMode::Glob);
    assert!(rule.passthrough_prefixes.is_empty());
    assert!(!rule.allow_override);
    assert_eq!(rule.reason, "");
}

#[test]
fn bad_command_argument_matchers_parse_with_per_pattern_modes() {
    let cfg = parse_repo_clud_config(
            r#"{"bad_commands":[{"match":"kubectl","replacement":"dry run first","arguments":{"ordered":["delete","namespace"],"any":["production",{"match":"^prod-[a-z]+$","match_mode":"regex"}],"none":["--dry-run=server"],"short_flags_all":["f","d"],"any_of":[{"contiguous":["-n","auto"]}]}}]}"#,
        )
        .expect("parses");
    let arguments = cfg.bad_commands[0]
        .arguments
        .as_ref()
        .expect("arguments parsed");
    assert_eq!(arguments.ordered.len(), 2);
    assert_eq!(arguments.any.len(), 2);
    assert_eq!(arguments.any[0].match_mode, MatchMode::Glob);
    assert_eq!(arguments.any[1].match_mode, MatchMode::Regex);
    assert_eq!(arguments.none.len(), 1);
    assert_eq!(arguments.short_flags_all, vec!['f', 'd']);
    assert_eq!(arguments.any_of.len(), 1);
}

#[test]
fn malformed_nested_argument_pattern_skips_only_that_rule() {
    let cfg = parse_repo_clud_config(
            r#"{"bad_commands":[{"match":"git","replacement":"safe","arguments":{"any":[{"match":"(","match_mode":"regex"}]}},{"match":"rm","replacement":"safe"}]}"#,
        )
        .expect("top-level config parses");
    assert_eq!(cfg.bad_commands.len(), 1);
    assert_eq!(cfg.bad_commands[0].pattern, "rm");
}

#[test]
fn bad_pipeline_rules_parse_and_default_stage_patterns_to_glob() {
    let cfg = parse_repo_clud_config(
            r#"{"bad_pipelines":[{"id":"no-download-to-shell","stages":[{"match":"curl"},{"match":"^(?:ba)?sh$","match_mode":"regex"}],"replacement":"download then inspect","reason":"hidden code"}]}"#,
        )
        .expect("parses");
    assert_eq!(cfg.bad_pipelines.len(), 1);
    let rule = &cfg.bad_pipelines[0];
    assert_eq!(rule.stages.len(), 2);
    assert_eq!(rule.stages[0].match_mode, MatchMode::Glob);
    assert_eq!(rule.stages[1].match_mode, MatchMode::Regex);
}

#[test]
fn malformed_pipeline_rule_is_skipped_without_losing_valid_rules() {
    let cfg = parse_repo_clud_config(
            r#"{"bad_pipelines":[{"stages":[{"match":"curl"}],"replacement":"invalid"},{"stages":[{"match":"curl"},{"match":"sh"}],"replacement":"inspect"}]}"#,
        )
        .expect("top-level config parses");
    assert_eq!(cfg.bad_pipelines.len(), 1);
    assert_eq!(cfg.bad_pipelines[0].stages.len(), 2);
}

#[test]
fn bad_pipelines_merge_and_dedupe_by_id_like_bad_commands() {
    let user = parse_raw_repo_clud_config(
            r#"{"bad_pipelines":[{"id":"shared","stages":[{"match":"wget"},{"match":"sh"}],"replacement":"user"},{"id":"user-only","stages":[{"match":"cat"},{"match":"sh"}],"replacement":"user"}]}"#,
        )
        .unwrap();
    let repo = parse_raw_repo_clud_config(
            r#"{"bad_pipelines":[{"id":"shared","stages":[{"match":"curl"},{"match":"sh"}],"replacement":"repo"}]}"#,
        )
        .unwrap();
    let merged = resolve(merge(repo, user));
    assert_eq!(merged.bad_pipelines.len(), 2);
    let shared = merged
        .bad_pipelines
        .iter()
        .find(|rule| rule.id.as_deref() == Some("shared"))
        .unwrap();
    assert_eq!(shared.replacement, "repo");
}

#[test]
fn bad_pipelines_alone_count_as_an_activation_directive() {
    let raw = parse_raw_repo_clud_config(
            r#"{"bad_pipelines":[{"stages":[{"match":"curl"},{"match":"sh"}],"replacement":"inspect"}]}"#,
        )
        .unwrap();
    assert!(has_directive(&raw));
}

#[test]
fn unsupported_wrapper_skips_only_the_malformed_rule() {
    let cfg = parse_repo_clud_config(
            r#"{"bad_commands":[{"match":"rm","through_wrappers":["mystery"],"replacement":"bad"},{"match":"git","replacement":"good"}]}"#,
        )
        .unwrap();
    assert_eq!(cfg.bad_commands.len(), 1);
    assert_eq!(cfg.bad_commands[0].pattern, "git");
}

#[test]
fn typoed_or_empty_argument_matcher_skips_the_containing_rule() {
    for arguments in [
        r#"{"ayn":["--force"]}"#,
        r#"{}"#,
        r#"{"any_of":[{}]}"#,
        r#"{"any":[]}"#,
    ] {
        let json = format!(
            r#"{{"bad_commands":[{{"match":"git","arguments":{arguments},"replacement":"bad"}},{{"match":"rm","replacement":"good"}}]}}"#
        );
        let cfg = parse_repo_clud_config(&json).unwrap();
        assert_eq!(cfg.bad_commands.len(), 1, "{arguments}");
        assert_eq!(cfg.bad_commands[0].pattern, "rm", "{arguments}");
    }
}

#[test]
fn bad_commands_empty_array_is_valid_noop() {
    let cfg = parse_repo_clud_config(r#"{"bad_commands":[]}"#).expect("parses");
    assert!(cfg.bad_commands.is_empty());
}

#[test]
fn bad_commands_absent_key_is_valid_noop() {
    let cfg = parse_repo_clud_config(r#"{"rust":{"use_soldr":true}}"#).expect("parses");
    assert!(cfg.bad_commands.is_empty());
}

#[test]
fn bad_commands_concatenates_user_and_repo_not_override() {
    let user = parse_raw_repo_clud_config(
        r#"{"bad_commands":[{"id":"user-rule","match":"foo","replacement":"bar"}]}"#,
    )
    .unwrap();
    let repo = parse_raw_repo_clud_config(
        r#"{"bad_commands":[{"id":"repo-rule","match":"baz","replacement":"qux"}]}"#,
    )
    .unwrap();
    let merged = resolve(merge(repo, user));
    let ids: Vec<_> = merged
        .bad_commands
        .iter()
        .filter_map(|r| r.id.as_deref())
        .collect();
    assert!(ids.contains(&"user-rule"));
    assert!(ids.contains(&"repo-rule"));
    assert_eq!(merged.bad_commands.len(), 2);
}

#[test]
fn bad_commands_dedupes_by_id_repo_wins() {
    let user = parse_raw_repo_clud_config(
        r#"{"bad_commands":[{"id":"shared","match":"user-pattern","replacement":"user-fix"}]}"#,
    )
    .unwrap();
    let repo = parse_raw_repo_clud_config(
        r#"{"bad_commands":[{"id":"shared","match":"repo-pattern","replacement":"repo-fix"}]}"#,
    )
    .unwrap();
    let merged = resolve(merge(repo, user));
    assert_eq!(merged.bad_commands.len(), 1);
    assert_eq!(merged.bad_commands[0].pattern, "repo-pattern");
}

#[test]
fn bad_commands_rules_without_id_never_dedupe() {
    let user =
        parse_raw_repo_clud_config(r#"{"bad_commands":[{"match":"same","replacement":"a"}]}"#)
            .unwrap();
    let repo =
        parse_raw_repo_clud_config(r#"{"bad_commands":[{"match":"same","replacement":"b"}]}"#)
            .unwrap();
    let merged = resolve(merge(repo, user));
    assert_eq!(merged.bad_commands.len(), 2);
}

#[test]
fn has_directive_true_for_bad_commands_only() {
    let tmp = TemporaryDirectory::new().unwrap();
    let home = tmp.path();
    write_settings(
        home,
        r#"{"bad_commands":[{"match":"playwright","replacement":"npm run test:integration"}]}"#,
    );
    let raw = read_and_parse_raw(&home.join(".clud").join("settings.json"), "user-level")
        .expect("parses");
    assert!(has_directive(&raw));
}

#[test]
fn has_directive_true_for_rust_only_still_works() {
    let raw = parse_raw_repo_clud_config(r#"{"rust":{"use_soldr":true}}"#).unwrap();
    assert!(has_directive(&raw));
}

#[test]
fn has_directive_false_for_empty_bad_commands_and_no_rust() {
    let raw = parse_raw_repo_clud_config(r#"{"bad_commands":[]}"#).unwrap();
    assert!(!has_directive(&raw));
}

#[test]
fn malformed_rule_missing_required_field_warns_and_skips() {
    let cfg = parse_repo_clud_config(
            r#"{"bad_commands":[{"match":"playwright"},{"match":"cypress","replacement":"npm run test:e2e"}]}"#,
        )
        .expect("parses");
    assert_eq!(cfg.bad_commands.len(), 1);
    assert_eq!(cfg.bad_commands[0].pattern, "cypress");
}

#[test]
fn malformed_rule_wrong_json_type_warns_and_skips() {
    let cfg = parse_repo_clud_config(
        r#"{"bad_commands":[{"match":123,"replacement":"npm run test:integration"}]}"#,
    )
    .expect("parses");
    assert!(cfg.bad_commands.is_empty());
}

#[test]
fn malformed_glob_pattern_warns_and_skips() {
    let cfg = parse_repo_clud_config(
        r#"{"bad_commands":[{"match":"play[wright","replacement":"npm run test:integration"}]}"#,
    )
    .expect("parses");
    assert!(cfg.bad_commands.is_empty());
}

#[test]
fn malformed_regex_pattern_warns_and_skips() {
    let cfg = parse_repo_clud_config(
            r#"{"bad_commands":[{"match":"play(wright","match_mode":"regex","replacement":"npm run test:integration"}]}"#,
        )
        .expect("parses");
    assert!(cfg.bad_commands.is_empty());
}

#[test]
fn compile_match_pattern_glob_is_whole_token_exact() {
    let re = compile_match_pattern("play", MatchMode::Glob).unwrap();
    assert!(re.is_match("play"));
    assert!(!re.is_match("playwright"));
    assert!(!re.is_match("playlist-gen"));
}

#[test]
fn compile_match_pattern_glob_wildcard_matches_family() {
    let re = compile_match_pattern("*-e2e-runner", MatchMode::Glob).unwrap();
    assert!(re.is_match("legacy-e2e-runner"));
    assert!(re.is_match("other-e2e-runner"));
    assert!(!re.is_match("e2e-runner-legacy"));
}

#[test]
fn compile_match_pattern_regex_mode_full_token_anchored() {
    let re = compile_match_pattern("^(playwright|pw-cli)$", MatchMode::Regex).unwrap();
    assert!(re.is_match("playwright"));
    assert!(re.is_match("pw-cli"));
    assert!(!re.is_match("playwrightish"));
}

#[test]
fn compile_match_pattern_is_case_insensitive() {
    let re = compile_match_pattern("playwright", MatchMode::Glob).unwrap();
    assert!(re.is_match("PLAYWRIGHT"));
    assert!(re.is_match("Playwright"));
}

// -----------------------------------------------------------------
// Issue #967 Phase 1: the `bash.block_cd` tri-state.
// -----------------------------------------------------------------

#[test]
fn block_cd_defaults_to_auto_when_unset() {
    let config = parse_repo_clud_config(r#"{"rust":{"use_soldr":true}}"#).unwrap();
    assert_eq!(config.bash.block_cd, BlockCd::Auto);
}

#[test]
fn block_cd_accepts_the_documented_spellings() {
    for (body, expected) in [
        (r#"{"bash":{"block_cd":"auto"}}"#, BlockCd::Auto),
        (r#"{"bash":{"block_cd":true}}"#, BlockCd::Always),
        (r#"{"bash":{"block_cd":false}}"#, BlockCd::Never),
        // Hand-edited files grow string spellings of the booleans.
        (r#"{"bash":{"block_cd":"never"}}"#, BlockCd::Never),
        (r#"{"bash":{"block_cd":"ALWAYS"}}"#, BlockCd::Always),
    ] {
        assert_eq!(
            parse_repo_clud_config(body).unwrap().bash.block_cd,
            expected,
            "{body}"
        );
    }
}

#[test]
fn an_unrecognized_block_cd_value_does_not_take_the_rest_of_the_file_down() {
    // `read_and_parse_raw` drops a document it cannot parse, so a strict
    // enum here would let one typo silently disarm the file's command rules.
    let config = parse_repo_clud_config(
        r#"{"bash":{"block_cd":"strictt"},"bad_commands":[{"id":"soldr","match":"cargo","replacement":"soldr cargo"}]}"#,
    )
    .expect("document still parses");
    assert_eq!(config.bash.block_cd, BlockCd::Auto);
    assert_eq!(config.bad_commands.len(), 1, "rules survive the bad value");
}

#[test]
fn block_cd_layers_repo_over_user_like_every_other_scalar() {
    let tmp = TemporaryDirectory::new().unwrap();
    mark_repo_root(tmp.path());
    write_settings(tmp.path(), r#"{"bash":{"block_cd":false}}"#);
    let repo = discover_repo_clud_config(tmp.path()).expect("repo config");
    assert_eq!(repo.bash.block_cd, BlockCd::Never);

    // The repo-local layer wins over the repo layer.
    write_local_settings(tmp.path(), r#"{"bash":{"block_cd":true}}"#);
    let local = discover_repo_clud_config(tmp.path()).expect("repo config");
    assert_eq!(local.bash.block_cd, BlockCd::Always);
}

#[test]
fn a_block_cd_only_file_counts_as_a_directive() {
    // Otherwise a user-level file that sets nothing but this key would be
    // treated as absent and the setting silently ignored.
    let raw = parse_raw_repo_clud_config(r#"{"bash":{"block_cd":true}}"#).unwrap();
    assert!(has_directive(&raw));
}

// #2352: `gh.backend` routes the session's gh shim to ghd.
#[test]
fn gh_backend_defaults_to_gh_and_accepts_ghd() {
    assert_eq!(
        parse_repo_clud_config("{}").unwrap().gh.backend,
        GhBackend::Gh
    );
    for (body, expected) in [
        (r#"{"gh":{"backend":"gh"}}"#, GhBackend::Gh),
        (r#"{"gh":{"backend":"ghd"}}"#, GhBackend::Ghd),
    ] {
        assert_eq!(
            parse_repo_clud_config(body).unwrap().gh.backend,
            expected,
            "{body}"
        );
    }
}

#[test]
fn an_unrecognized_gh_backend_is_dropped_without_losing_the_file() {
    let config = parse_repo_clud_config(
        r#"{"gh":{"backend":"ghdd"},"bad_commands":[{"id":"soldr","match":"cargo","replacement":"soldr cargo"}]}"#,
    )
    .expect("document still parses");
    assert_eq!(config.gh.backend, GhBackend::Gh);
    assert_eq!(config.bad_commands.len(), 1);
}

#[test]
fn gh_backend_layers_repo_over_user() {
    let user = parse_raw_repo_clud_config(r#"{"gh":{"backend":"gh"}}"#).unwrap();
    let repo = parse_raw_repo_clud_config(r#"{"gh":{"backend":"ghd"}}"#).unwrap();
    assert_eq!(
        resolve(merge(repo, user.clone())).gh.backend,
        GhBackend::Ghd
    );
    let silent = parse_raw_repo_clud_config(r#"{"bash":{"block_cd":true}}"#).unwrap();
    assert_eq!(resolve(merge(silent, user)).gh.backend, GhBackend::Gh);
}

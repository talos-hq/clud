//! Unit tests for [`crate::payload_rules`].
//!
//! Ports CLIProxyAPI's `requests.payload` (`config.example.yaml:266-334`).
//! The Python suite (`tests/test_payload_rules.py`) drives the same engine
//! through the hidden `__apply-payload-rules` seam.

use super::*;

fn rules(json: Value) -> PayloadRules {
    parse(&serde_json::to_vec(&json).expect("fixture serializes")).expect("fixture parses")
}

#[test]
fn wildcard_star_is_the_whole_language() {
    assert!(wildcard_matches("claude-*", "claude-sonnet-4"));
    assert!(wildcard_matches("claude-*", "CLAUDE-OPUS"));
    assert!(wildcard_matches("*sonnet*", "x-sonnet-y"));
    assert!(wildcard_matches("*", "anything"));
    assert!(wildcard_matches("gpt-5.?", "gpt-5.?"));
    assert!(!wildcard_matches("claude-*", "gpt-6-sol"));
    assert!(!wildcard_matches("gpt-5", "gpt-5.1"));
    assert!(!wildcard_matches("a?c", "abc"));
}

#[test]
fn default_fills_only_what_the_caller_left_out() {
    let rules = rules(serde_json::json!({
        "default": [
            {"models": [{"name": "claude-*"}],
             "params": {"temperature": 0.2, "max_tokens": 4096}}
        ]
    }));
    let mut payload = serde_json::json!({"model": "claude-sonnet-4", "temperature": 1.0});
    let outcome = apply(&rules, "claude-sonnet-4", "claude", &mut payload);
    assert_eq!(outcome.defaulted, 1);
    assert_eq!(outcome.problems, Vec::<String>::new());
    assert_eq!(payload["temperature"], serde_json::json!(1.0));
    assert_eq!(payload["max_tokens"], serde_json::json!(4096));
}

#[test]
fn default_treats_null_as_missing_but_override_does_not_care() {
    let rules = rules(serde_json::json!({
        "default": [{"params": {"metadata.user_id": "probe"}}],
        "override": [{"params": {"metadata.user_id": "forced"}}],
    }));
    let mut payload = serde_json::json!({"metadata": {"user_id": null}});
    let outcome = apply(&rules, "any", "claude", &mut payload);
    assert_eq!(outcome.defaulted, 1);
    assert_eq!(outcome.overridden, 1);
    assert_eq!(payload["metadata"]["user_id"], serde_json::json!("forced"));
}

#[test]
fn override_replaces_and_runs_after_default() {
    let rules = rules(serde_json::json!({
        "default": [{"models": [{"name": "gpt-*", "protocol": "codex"}],
                     "params": {"reasoning.effort": "low"}}],
        "override": [{"models": [{"name": "gpt-*", "protocol": "codex"}],
                      "params": {"reasoning.effort": "high"}}],
    }));
    let mut payload = serde_json::json!({"reasoning": {"effort": "medium"}});
    let outcome = apply(&rules, "gpt-6-sol", "codex", &mut payload);
    assert_eq!(outcome.defaulted, 0, "already present, so default skipped");
    assert_eq!(outcome.overridden, 1);
    assert_eq!(payload["reasoning"]["effort"], serde_json::json!("high"));
}

#[test]
fn a_protocol_selector_keeps_other_protocols_untouched() {
    let rules = rules(serde_json::json!({
        "override": [{"models": [{"name": "*", "protocol": "claude"}],
                      "params": {"max_tokens": 128000}}]
    }));
    let mut claude = serde_json::json!({"max_tokens": 1000});
    apply(&rules, "claude-opus", "claude", &mut claude);
    assert_eq!(claude["max_tokens"], serde_json::json!(128000));

    let mut codex = serde_json::json!({"max_tokens": 1000});
    let outcome = apply(&rules, "gpt-6-sol", "codex", &mut codex);
    assert_eq!(outcome.overridden, 0);
    assert_eq!(codex["max_tokens"], serde_json::json!(1000));
}

#[test]
fn filter_removes_a_path_and_reports_nothing_when_absent() {
    let rules = rules(serde_json::json!({
        "filter": [{"models": [{"name": "claude-*"}],
                    "params": ["metadata.user_id", "thinking"]}]
    }));
    let mut payload = serde_json::json!({
        "model": "claude-sonnet-4",
        "metadata": {"user_id": "abc"},
        "thinking": {"budget_tokens": 1024}
    });
    let outcome = apply(&rules, "claude-sonnet-4", "claude", &mut payload);
    assert_eq!(outcome.filtered, 2);
    assert_eq!(payload["metadata"], serde_json::json!({}));
    assert!(payload.get("thinking").is_none());

    let outcome = apply(&rules, "claude-sonnet-4", "claude", &mut payload);
    assert_eq!(outcome.filtered, 0);
    assert!(!outcome.changed());
}

#[test]
fn array_indices_are_addressed_and_removal_shifts_them() {
    let rules = rules(serde_json::json!({
        "override": [{"params": {"messages.0.role": "user"}}],
        "filter": [{"params": ["messages.1"]}],
    }));
    let mut payload = serde_json::json!({
        "messages": [
            {"role": "assistant", "content": "a"},
            {"role": "user", "content": "drop me"},
            {"role": "user", "content": "keep me"}
        ]
    });
    let outcome = apply(&rules, "m", "claude", &mut payload);
    assert_eq!((outcome.overridden, outcome.filtered), (1, 1));
    assert_eq!(payload["messages"][0]["role"], serde_json::json!("user"));
    assert_eq!(
        payload["messages"][1]["content"],
        serde_json::json!("keep me")
    );
    assert_eq!(payload["messages"].as_array().map(Vec::len), Some(2));
}

#[test]
fn a_path_inside_a_missing_object_is_created_but_a_missing_message_is_not() {
    let rules = rules(serde_json::json!({
        "override": [{"params": {
            "metadata.client": "codex",
            "messages.7.content": "nope"
        }}]
    }));
    let mut payload = serde_json::json!({"messages": [{"role": "user"}]});
    let outcome = apply(&rules, "m", "claude", &mut payload);
    assert_eq!(outcome.overridden, 1);
    assert_eq!(payload["metadata"]["client"], serde_json::json!("codex"));
    assert_eq!(outcome.problems.len(), 1);
    assert!(
        outcome.problems[0].contains("past the end"),
        "{:?}",
        outcome.problems
    );
}

#[test]
fn a_typo_in_a_rule_key_is_refused_rather_than_ignored() {
    let error = parse(br#"{"overide": []}"#).expect_err("unknown key is refused");
    assert!(error.contains("unknown field"), "{error}");
    assert!(parse(b"[]").is_err(), "a rules document is an object");
    assert!(parse(br#"{"default": [{"models": [{"name": ""}], "params": {}}]}"#).is_err());
    assert!(parse(br#"{"default": [{"params": {"a..b": 1}}]}"#).is_err());
    assert_eq!(
        parse(b"{}").expect("empty rules are valid"),
        PayloadRules::default()
    );
}

#[test]
fn body_bytes_are_untouched_when_nothing_fires() {
    // `CLUD_PAYLOAD_RULES` is unset in the unit-test environment, so this is
    // the pre-existing behavior the launch-scoped gateway depends on.
    let body = br#"{"model":"m","temperature":1.0}"#.to_vec();
    let mut payload: Value = serde_json::from_slice(&body).expect("fixture parses");
    let (rewritten, outcome) = rewrite_body("m", "claude", body.clone(), &mut payload);
    assert_eq!(rewritten, body);
    assert!(!outcome.changed());
}

#[test]
fn from_path_refuses_an_oversized_rules_file() {
    let dir = std::env::temp_dir().join(format!("payload-rules-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("rules.json");
    let big = vec![b' '; MAX_RULES_BYTES as usize + 1];
    std::fs::write(&path, &big).expect("write");
    let error = from_path(&path).expect_err("oversized file refused");
    assert!(error.contains("over the"), "{error}");
    std::fs::write(&path, br#"{"filter": [{"params": ["x"]}]}"#).expect("write");
    let rules = from_path(&path).expect("a small file parses");
    assert_eq!(rules.filter.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_query_selector_targets_one_array_element_by_field() {
    let rules = rules(serde_json::json!({
        "filter": [{"params": ["tools.#(name==\"apply_patch\")"]}],
        "override": [{"params": {
            "tools.#(name==\"WebSearch\").type": "disabled"
        }}]
    }));
    let mut payload = serde_json::json!({
        "tools": [{"name": "WebSearch"}, {"name": "apply_patch"}]
    });
    let outcome = apply(&rules, "claude-sonnet-4", "claude", &mut payload);
    assert_eq!(outcome.filtered, 1);
    assert_eq!(outcome.problems, Vec::<String>::new());
    assert_eq!(
        payload["tools"],
        serde_json::json!([{"name": "WebSearch", "type": "disabled"}])
    );
}

#[test]
fn an_all_matches_chain_removes_the_nested_tool_upstream_filters() {
    // CLIProxyAPI's own example (`config.example.yaml:334`): Codex's
    // `input[]` additional_tools envelope, all matches at every level.
    let rules = rules(serde_json::json!({
        "filter": [{"models": [{"name": "gpt-*", "protocol": "codex"}],
                    "params": [
                        "input.#(type==\"additional_tools\")#.tools.#(name==\"functions\")#.tools.#(name==\"apply_patch\")#"
                    ]}]
    }));
    let mut payload = serde_json::json!({
        "input": [
            {"type": "message"},
            {"type": "additional_tools", "tools": [
                {"name": "functions", "tools": [{"name": "apply_patch"}, {"name": "shell"}]}
            ]}
        ]
    });
    let outcome = apply(&rules, "gpt-6-sol", "codex", &mut payload);
    assert_eq!(outcome.filtered, 1);
    assert_eq!(outcome.problems, Vec::<String>::new());
    assert_eq!(
        payload["input"][1]["tools"][0]["tools"],
        serde_json::json!([{"name": "shell"}])
    );
    assert_eq!(payload["input"][0], serde_json::json!({"type": "message"}));
}

#[test]
fn a_selector_that_matches_nothing_changes_nothing_and_is_not_a_problem() {
    let rules = rules(serde_json::json!({
        "filter": [{"params": ["tools.#(name==\"absent\")"]}],
        "override": [{"params": {"tools.#(name==\"absent\").type": "x"}}]
    }));
    let mut payload = serde_json::json!({"tools": [{"name": "WebSearch"}]});
    let outcome = apply(&rules, "m", "claude", &mut payload);
    assert_eq!((outcome.filtered, outcome.overridden), (0, 0));
    assert_eq!(payload["tools"], serde_json::json!([{"name": "WebSearch"}]));
}

#[test]
fn a_malformed_selector_in_a_write_rule_is_refused_when_the_file_is_loaded() {
    let refused = parse(
        serde_json::to_vec(&serde_json::json!({
            "override": [{"params": {"tools.#(name)": "x"}}]
        }))
        .expect("fixture serializes")
        .as_slice(),
    );
    let reason = refused.expect_err("a malformed write path is refused");
    assert!(reason.contains("must compare one field"), "{reason}");
}

#[test]
fn a_malformed_selector_in_a_filter_is_reported_per_request_and_skipped() {
    // A filter path that cannot be parsed removes nothing; it must not turn a
    // client request into a 400.
    let rules = rules(serde_json::json!({
        "filter": [{"params": ["tools.#(name)"]}],
        "override": [{"params": {"temperature": 0.1}}]
    }));
    let mut payload = serde_json::json!({"tools": [{"name": "WebSearch"}]});
    let outcome = apply(&rules, "m", "claude", &mut payload);
    assert_eq!(outcome.overridden, 1);
    assert_eq!(outcome.filtered, 0);
    assert_eq!(outcome.problems.len(), 1);
    assert!(
        outcome.problems[0].contains("must compare one field"),
        "{:?}",
        outcome.problems
    );
    assert_eq!(payload["temperature"], serde_json::json!(0.1));
}

#[test]
fn header_conditions_require_every_configured_header_to_match() {
    let rules = rules(serde_json::json!({
        "override": [{
            "models": [{"name": "claude-*", "headers": {"X-Client-Tier": "tenant-*-region-*"}}],
            "params": {"metadata.tier_seen": true}
        }]
    }));
    let headers = vec![(
        "x-client-tier".to_string(),
        "tenant-acme-region-eu".to_string(),
    )];
    let mut payload = serde_json::json!({"metadata": {}});
    let outcome = apply_request(
        &rules,
        &Request::with_headers("claude-sonnet-4", "claude", &headers),
        &mut payload,
    );
    assert_eq!(outcome.overridden, 1);
    assert_eq!(payload["metadata"]["tier_seen"], serde_json::json!(true));

    // Header names are matched case-insensitively; the pattern is not the name.
    let wrong = vec![("X-Client-Tier".to_string(), "free".to_string())];
    let mut untouched = serde_json::json!({"metadata": {}});
    let missed = apply_request(
        &rules,
        &Request::with_headers("claude-sonnet-4", "claude", &wrong),
        &mut untouched,
    );
    assert!(!missed.changed());
    assert_eq!(untouched["metadata"], serde_json::json!({}));

    // A request carrying no headers cannot satisfy a header condition.
    let mut absent = serde_json::json!({"metadata": {}});
    assert!(!apply(&rules, "claude-sonnet-4", "claude", &mut absent).changed());
}

#[test]
fn payload_shape_conditions_gate_a_rule() {
    let rules = rules(serde_json::json!({
        "override": [
            {"models": [{"name": "claude-*", "match": [{"metadata.client": "codex"}]}],
             "params": {"metadata.matched": true}},
            {"models": [{"name": "claude-*", "not-match": [{"metadata.mode": "dev"}]}],
             "params": {"metadata.not_matched": true}},
            {"models": [{"name": "claude-*", "exist": ["metadata.user_id"]}],
             "params": {"metadata.existed": true}},
            {"models": [{"name": "claude-*", "not-exist": ["metadata.disable_payload"]}],
             "params": {"metadata.not_existed": true}}
        ]
    }));
    let mut payload = serde_json::json!({
        "metadata": {"client": "codex", "user_id": "u"}
    });
    let outcome = apply(&rules, "claude-sonnet-4", "claude", &mut payload);
    assert_eq!(outcome.overridden, 4);
    assert_eq!(payload["metadata"]["matched"], serde_json::json!(true));
    assert_eq!(payload["metadata"]["existed"], serde_json::json!(true));

    let mut refused = serde_json::json!({
        "metadata": {"client": "claude", "mode": "dev", "disable_payload": 1}
    });
    let missed = apply(&rules, "claude-sonnet-4", "claude", &mut refused);
    assert!(!missed.changed());
    assert_eq!(
        refused["metadata"],
        serde_json::json!({"client": "claude", "mode": "dev", "disable_payload": 1})
    );
}

#[test]
fn a_malformed_condition_path_skips_only_its_own_rule() {
    let rules = rules(serde_json::json!({
        "override": [
            {"models": [{"name": "claude-*", "match": [{"a..b": 1}]}],
             "params": {"metadata.bad": true}},
            {"models": [{"name": "claude-*"}], "params": {"metadata.good": true}}
        ]
    }));
    let mut payload = serde_json::json!({"metadata": {}});
    let outcome = apply(&rules, "claude-sonnet-4", "claude", &mut payload);
    assert_eq!(payload["metadata"], serde_json::json!({"good": true}));
    assert_eq!(outcome.problems.len(), 1);
    assert!(
        outcome.problems[0].contains("empty path segment"),
        "{:?}",
        outcome.problems
    );
}

#[test]
fn a_condition_path_resolves_selectors_and_missing_values_are_false() {
    // `exist` on a selector: the element must be there. A null value counts as
    // missing for `exist`, exactly as it does for a `default` rule's presence
    // check.
    let rules = rules(serde_json::json!({
        "override": [{
            "models": [{"name": "*", "exist": ["tools.#(name==\"apply_patch\").type"]}],
            "params": {"metadata.saw_patch": true}
        }]
    }));
    let mut hit = serde_json::json!({"tools": [{"name": "apply_patch", "type": "freeform"}]});
    assert!(apply(&rules, "m", "claude", &mut hit).changed());
    let mut missed = serde_json::json!({"tools": [{"name": "apply_patch", "type": null}]});
    assert!(!apply(&rules, "m", "claude", &mut missed).changed());
}

#[test]
fn default_raw_sets_parsed_json_only_where_nothing_is() {
    // Upstream keeps raw values as JSON bytes (`sjson.SetRawBytes`), so the
    // operator can paste a document the rules file has no shape for. Here the
    // raw text must arrive as an object, not as a JSON string.
    let rules = rules(serde_json::json!({
        "default-raw": [{
            "models": [{"name": "gemini-*"}],
            "params": {
                "generationConfig.responseJsonSchema": "{\"type\":\"object\"}",
                "generationConfig.thinkingConfig.thinkingBudget": "32768"
            }
        }]
    }));
    let mut payload = serde_json::json!({
        "generationConfig": {"responseJsonSchema": {"type": "string"}}
    });
    let outcome = apply(&rules, "gemini-2.5-pro", "gemini", &mut payload);
    assert_eq!(outcome.defaulted, 1, "only the absent path is written");
    assert_eq!(outcome.problems, Vec::<String>::new());
    assert_eq!(
        payload["generationConfig"]["responseJsonSchema"],
        serde_json::json!({"type": "string"}),
        "an existing value is never replaced by a default"
    );
    assert_eq!(
        payload["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        serde_json::json!(32768)
    );

    let mut other = serde_json::json!({"generationConfig": {}});
    let untouched = apply(&rules, "claude-sonnet-4", "claude", &mut other);
    assert!(
        !untouched.changed(),
        "the model selector still gates raw rules"
    );
}

#[test]
fn override_raw_runs_after_the_typed_override() {
    let rules = rules(serde_json::json!({
        "override": [{"params": {"response_format": "typed"}}],
        "override-raw": [{
            "params": {"response_format": "{\"type\":\"json_schema\"}"}
        }]
    }));
    let mut payload = serde_json::json!({});
    let outcome = apply(&rules, "any", "codex", &mut payload);
    assert_eq!(outcome.overridden, 2);
    assert_eq!(
        payload["response_format"],
        serde_json::json!({"type": "json_schema"}),
        "the raw rule runs last, as upstream's OverrideRaw does"
    );
}

#[test]
fn a_raw_value_that_is_not_json_is_refused_when_the_file_loads() {
    let error = parse(
        serde_json::to_vec(&serde_json::json!({
            "default-raw": [{"params": {"response_format": "{not json"}}]
        }))
        .expect("fixture serializes")
        .as_slice(),
    )
    .expect_err("invalid raw JSON must be refused at load");
    assert!(
        error.contains("raw value is not JSON"),
        "unexpected error: {error}"
    );
}

#[test]
fn from_protocol_narrows_a_rule_to_the_shape_the_client_spoke() {
    // Upstream's `from-protocol` (`config.example.yaml:600`, matched by
    // `payloadFromProtocolMatches`, `payload_helpers.go:436`). One rules file
    // must be able to tell a Claude-shaped caller from an OpenAI-shaped one.
    let rules = rules(serde_json::json!({
        "override": [
            {"models": [{"name": "claude-*", "from-protocol": "openai"}],
             "params": {"max_tokens": 32}},
            {"models": [{"name": "claude-*", "from-protocol": "claude"}],
             "params": {"metadata.via": "messages"}},
        ]
    }));
    let mut payload = serde_json::json!({"max_tokens": 1024});
    let outcome = apply_request(
        &rules,
        &Request::with_source("claude-sonnet-4", "claude", "openai", &[]),
        &mut payload,
    );
    assert_eq!(outcome.overridden, 1);
    assert_eq!(payload["max_tokens"], serde_json::json!(32));
    assert!(
        payload.get("metadata").is_none(),
        "a rule pinned to another source must not fire"
    );

    let mut payload = serde_json::json!({"max_tokens": 1024});
    let outcome = apply_request(
        &rules,
        &Request::with_source("claude-sonnet-4", "claude", "claude", &[]),
        &mut payload,
    );
    assert_eq!(outcome.overridden, 1);
    assert_eq!(payload["max_tokens"], serde_json::json!(1024));
    assert_eq!(payload["metadata"]["via"], serde_json::json!("messages"));
}

#[test]
fn a_pinned_from_protocol_never_fires_for_an_unknown_source() {
    // Upstream returns false when the pattern is set and the source is empty.
    assert!(from_protocol_matches(None, ""));
    assert!(from_protocol_matches(None, "openai"));
    assert!(!from_protocol_matches(Some("openai"), ""));
    assert!(from_protocol_matches(Some("OpenAI"), "openai"));
    // Upstream spells the Responses source three ways; they are one source.
    assert!(from_protocol_matches(Some("openai-response"), "responses"));
    assert!(from_protocol_matches(Some("response"), "openai-responses"));
    assert!(!from_protocol_matches(Some("claude"), "openai"));
}

#[test]
fn an_empty_from_protocol_selector_is_refused_at_load() {
    let error = parse(
        serde_json::to_vec(&serde_json::json!({
            "default": [{"models": [{"name": "claude-*", "from-protocol": " "}], "params": {}}]
        }))
        .expect("fixture serializes")
        .as_slice(),
    )
    .expect_err("an empty from-protocol is a typo");
    assert!(error.contains("empty from-protocol"), "{error}");
}

#[test]
fn settings_own_the_rules_under_the_upstream_key() {
    let document = serde_json::json!({
        "model_contexts": {"windows": {}},
        "requests": {"payload": {
            "default": [{"models": [{"name": "claude-*"}], "params": {"thinking.budget_tokens": 2048}}]
        }}
    });
    let rules = from_settings_document(&document)
        .expect("a well-formed section parses")
        .expect("the key is present");
    let mut payload = serde_json::json!({"model": "claude-sonnet"});
    let outcome = apply(&rules, "claude-sonnet", "claude", &mut payload);
    assert_eq!(outcome.defaulted, 1);
    assert_eq!(payload["thinking"]["budget_tokens"], 2048);
}

#[test]
fn settings_without_the_key_mean_no_rules_rather_than_an_error() {
    for document in [
        serde_json::json!({}),
        serde_json::json!({"requests": {}}),
        serde_json::json!({"request": {"payload": {"default": []}}}),
        serde_json::json!({"requests": {"payload": null}}),
    ] {
        assert_eq!(
            from_settings_document(&document).expect("absence is not an error"),
            None,
            "{document} must not select any rules"
        );
    }
}

#[test]
fn a_settings_section_that_is_not_an_object_is_refused() {
    let error = from_settings_document(&serde_json::json!({"requests": {"payload": []}}))
        .expect_err("a list is not a rules document");
    assert!(error.contains(SETTINGS_KEY), "{error}");
    let error =
        from_settings_document(&serde_json::json!({"requests": {"payload": {"defualt": []}}}))
            .expect_err("a typo must not silently no-op");
    assert!(error.contains(SETTINGS_KEY), "{error}");
}

#[test]
fn an_explicit_rules_file_outranks_the_settings_document() {
    // `resolve_source` is the precedence rule: the env var names the file when
    // it is set and non-empty, and only an unset/empty variable falls through
    // to the user-level settings file.
    assert_eq!(
        source_for(Some("/tmp/rules.json".into()), Some(Path::new("/home/x"))),
        Some(Source::EnvFile(PathBuf::from("/tmp/rules.json")))
    );
    assert_eq!(
        source_for(Some("".into()), Some(Path::new("/home/x"))),
        Some(Source::Settings(
            Path::new("/home/x").join(".clud").join("settings.json")
        ))
    );
    assert_eq!(source_for(None, None), None);
}

//! Compiling declarations into a frontend's native registration (#967 Phase 2b).

use super::*;
use crate::clud_hooks::parse;

fn hooks(text: &str) -> CludHooks {
    parse(text).expect("parses")
}

fn commands_for(fragment: &Value, event: &str) -> Vec<String> {
    fragment["hooks"][event]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .flat_map(|group| {
            group["hooks"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
        })
        .filter_map(|handler| handler["command"].as_str().map(ToOwned::to_owned))
        .collect()
}

#[test]
fn every_declared_event_gets_a_dispatcher_line() {
    let fragment = claude_settings_fragment(
        &hooks(r#"{"hooks":{"PreToolUse":[{"command":"a"}],"Stop":[{"command":"b"}]}}"#),
        false,
    )
    .expect("declared");

    assert_eq!(
        commands_for(&fragment, "PreToolUse"),
        vec!["clud-cmd-scan --event PreToolUse"]
    );
    assert_eq!(
        commands_for(&fragment, "Stop"),
        vec!["clud-cmd-scan --event Stop"]
    );
}

#[test]
fn several_hooks_on_one_event_still_register_a_single_line() {
    // clud dispatches the whole event; one registration is all the frontend
    // needs, and more would run the set repeatedly.
    let fragment = claude_settings_fragment(
        &hooks(r#"{"hooks":{"Stop":[{"command":"a"},{"command":"b"},{"command":"c"}]}}"#),
        false,
    )
    .expect("declared");

    assert_eq!(commands_for(&fragment, "Stop").len(), 1);
}

#[test]
fn the_registered_matcher_is_catch_all_whatever_the_declaration_scopes_to() {
    // The per-hook matcher is applied by clud at dispatch time. Narrowing the
    // registration would silently drop declarations — a line scoped to `Bash`
    // can never deliver an `Edit`-matched hook.
    let fragment = claude_settings_fragment(
        &hooks(r#"{"hooks":{"PreToolUse":[{"matcher":"Edit","command":"a"}]}}"#),
        false,
    )
    .expect("declared");

    assert_eq!(fragment["hooks"]["PreToolUse"][0]["matcher"], "*");
}

#[test]
fn a_repo_that_declares_nothing_compiles_to_nothing() {
    // The signal not to pass `--settings` at all: a repo that has not opted
    // in should see the launch it saw before this feature existed.
    assert!(claude_settings_fragment(&hooks("{}"), false).is_none());
    assert!(claude_settings_fragment(&hooks(r#"{"hooks":{"Stop":[]}}"#), false).is_none());
}

#[test]
fn the_cwd_changed_backstop_line_rides_only_on_frontend_support() {
    // The capability probe answers for the installed client; a negative
    // answer must degrade silently to no line at all (DD-064).
    assert!(claude_settings_fragment(&hooks("{}"), false).is_none());

    let fragment = claude_settings_fragment(&hooks("{}"), true).expect("declared");
    assert_eq!(
        commands_for(&fragment, "CwdChanged"),
        vec!["clud-cmd-scan --event CwdChanged"]
    );
}

#[test]
fn a_repo_declaring_nothing_still_registers_the_backstop_when_supported() {
    // The backstop is clud's own line; it exists for an opted-in repo even if
    // the repo declared no events at all. It is what turns a chdir by an
    // alias into a visible drift warning.
    let fragment = claude_settings_fragment(&hooks("{}"), true).expect("declared");
    assert_eq!(
        commands_for(&fragment, "CwdChanged"),
        vec!["clud-cmd-scan --event CwdChanged"]
    );
}

#[test]
fn a_declared_cwd_changed_hook_and_the_backstop_share_one_line() {
    // A repo declaring the event itself gets the same single dispatcher line;
    // clud's own backstop logic runs inside it alongside the repo's hooks.
    let fragment = claude_settings_fragment(
        &hooks(r#"{"hooks":{"CwdChanged":[{"command":"check-here.py"}]}}"#),
        true,
    )
    .expect("declared");

    assert_eq!(
        commands_for(&fragment, "CwdChanged"),
        vec!["clud-cmd-scan --event CwdChanged"]
    );
}

#[test]
fn the_dispatcher_command_names_the_event() {
    assert_eq!(dispatcher_command("Stop"), "clud-cmd-scan --event Stop");
}

// -----------------------------------------------------------------
// Merging into an existing settings document.
// -----------------------------------------------------------------

#[test]
fn merging_concatenates_per_event_rather_than_replacing() {
    // Mirrors how the harness layers hooks. Replacing would let clud's
    // registration silently displace the bridge's lifecycle hooks, or a
    // settings document the user passed on the command line.
    let mut base = json!({"hooks":{"Stop":[{"matcher":"*","hooks":[{"command":"theirs"}]}]}});
    let overlay =
        claude_settings_fragment(&hooks(r#"{"hooks":{"Stop":[{"command":"a"}]}}"#), false)
            .expect("declared");

    merge_hook_settings(&mut base, &overlay).expect("merges");

    assert_eq!(
        commands_for(&base, "Stop"),
        vec!["theirs", "clud-cmd-scan --event Stop"]
    );
}

#[test]
fn merging_preserves_unrelated_keys_and_events() {
    let mut base = json!({
        "model": "some-model",
        "permissions": {"deny": ["Bash(false *)"]},
        "hooks": {"SessionStart": [{"hooks":[{"command":"theirs"}]}]}
    });
    let overlay =
        claude_settings_fragment(&hooks(r#"{"hooks":{"Stop":[{"command":"a"}]}}"#), false)
            .expect("declared");

    merge_hook_settings(&mut base, &overlay).expect("merges");

    assert_eq!(base["model"], "some-model");
    assert_eq!(base["permissions"]["deny"][0], "Bash(false *)");
    assert_eq!(commands_for(&base, "SessionStart"), vec!["theirs"]);
    assert_eq!(
        commands_for(&base, "Stop"),
        vec!["clud-cmd-scan --event Stop"]
    );
}

#[test]
fn merging_into_a_document_without_hooks_creates_the_section() {
    let mut base = json!({"model": "some-model"});
    let overlay =
        claude_settings_fragment(&hooks(r#"{"hooks":{"Stop":[{"command":"a"}]}}"#), false)
            .expect("declared");

    merge_hook_settings(&mut base, &overlay).expect("merges");

    assert_eq!(
        commands_for(&base, "Stop"),
        vec!["clud-cmd-scan --event Stop"]
    );
}

#[test]
fn a_malformed_target_is_an_error_rather_than_a_silent_drop() {
    // Losing a registration quietly would mean the hooks never fire and
    // nothing says why.
    let overlay =
        claude_settings_fragment(&hooks(r#"{"hooks":{"Stop":[{"command":"a"}]}}"#), false)
            .expect("declared");

    let mut not_an_object = json!([]);
    assert!(merge_hook_settings(&mut not_an_object, &overlay).is_err());

    let mut hooks_not_an_object = json!({"hooks": "nope"});
    assert!(merge_hook_settings(&mut hooks_not_an_object, &overlay).is_err());

    let mut event_not_an_array = json!({"hooks": {"Stop": "nope"}});
    assert!(merge_hook_settings(&mut event_not_an_array, &overlay).is_err());
}

#[test]
fn an_overlay_without_hooks_is_a_no_op() {
    let mut base = json!({"hooks":{"Stop":[{"hooks":[{"command":"theirs"}]}]}});
    let before = base.clone();

    merge_hook_settings(&mut base, &json!({"model": "x"})).expect("merges");

    assert_eq!(base, before);
}

#[test]
fn deletion_safety_fragment_denies_file_tool_edits_of_the_user_settings_file() {
    // #1668: the safe_rm.extra_roots override lives there; only the user
    // writes it.
    let fragment = deletion_safety_fragment();
    let deny: Vec<&str> = fragment["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(deny.contains(&"Edit(~/.clud/settings.json)"), "{deny:?}");
    assert!(!deny.contains(&"Write(~/.clud/settings.json)"), "{deny:?}");
    assert!(
        deny.iter().any(|rule| rule.starts_with("Bash(")),
        "{deny:?}"
    );
}

#[test]
fn deletion_safety_fragment_routes_every_guarded_tool_to_the_dispatcher() {
    // #1949: the /loop first-round stop guard never fired in a launch whose
    // only clud matcher was `Bash`, because the harness filters by matcher
    // before the hook runs. The same filter hid every other non-Bash rule.
    let fragment = deletion_safety_fragment();
    let entries = fragment["hooks"]["PreToolUse"].as_array().unwrap();
    let matcher = regex::Regex::new(&format!(
        "^(?:{})$",
        entries[0]["matcher"].as_str().unwrap()
    ))
    .unwrap();
    for tool in GUARDED_TOOLS {
        assert!(matcher.is_match(tool), "{tool} must reach the dispatcher");
    }
    // #2233 gave `Read` a rule (the unchanged-re-read guard), so the tools
    // with no rule left are the read-free ones.
    for tool in ["Grep", "Glob", "WebFetch", "TodoWrite", "ToolSearch"] {
        assert!(!matcher.is_match(tool), "{tool} has no rule");
    }
    assert_eq!(entries[0]["hooks"][0]["command"], DISPATCHER_BINARY);
}

/// The PreToolUse commands a harness runs for `tool`, filtering entries by
/// matcher the way Claude Code does: empty or `*` matches every tool,
/// anything else is an anchored regex over the tool name.
fn harness_commands_for_tool(settings: &Value, tool: &str) -> Vec<String> {
    let mut commands = Vec::new();
    for entry in settings["hooks"]["PreToolUse"].as_array().unwrap() {
        let matcher = entry["matcher"].as_str().unwrap_or("");
        let matches = matcher.is_empty()
            || matcher == "*"
            || regex::Regex::new(&format!("^(?:{matcher})$"))
                .unwrap()
                .is_match(tool);
        if !matches {
            continue;
        }
        for hook in entry["hooks"].as_array().unwrap() {
            commands.push(hook["command"].as_str().unwrap().to_string());
        }
    }
    commands
}

#[test]
fn a_repo_without_a_catch_all_matcher_still_routes_guarded_tools_to_the_dispatcher() {
    // #1954: unit tests of each rule could not see the harness's matcher
    // filter. Merge clud's fragment into repo settings that register only a
    // Bash guard, as most repos do, and filter as the harness would.
    let mut settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": "repo-guard" }]
            }]
        }
    });
    merge_hook_settings(&mut settings, &deletion_safety_fragment()).unwrap();
    // The /loop stop, a grind planner's Write, a grind Agent spawn, and the
    // unchanged-re-read guard's `Read` (#2233).
    for tool in [
        "EnterPlanMode",
        "ScheduleWakeup",
        "Write",
        "Agent",
        "Bash",
        "Read",
    ] {
        let commands = harness_commands_for_tool(&settings, tool);
        assert!(
            commands.iter().any(|c| c == DISPATCHER_BINARY),
            "{tool} never reaches {DISPATCHER_BINARY}: {commands:?}"
        );
    }
    assert!(harness_commands_for_tool(&settings, "Grep").is_empty());
}

#[test]
fn deletion_safety_fragment_routes_the_users_prompt_to_the_dispatcher() {
    // #1984: the prompt is where the user allows worktrees for a session.
    let fragment = deletion_safety_fragment();
    let event = crate::worktree_consent::USER_PROMPT_SUBMIT_EVENT;
    assert_eq!(
        fragment["hooks"][event][0]["hooks"][0]["command"],
        dispatcher_command(event)
    );
}

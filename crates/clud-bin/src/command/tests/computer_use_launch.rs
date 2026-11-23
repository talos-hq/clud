//! Computer use registration on a launch (DD-233): the `cua` MCP server and
//! its allow rule on first-party Claude launches, permission mode from
//! `--safe`, opt-out and opt-in flags, and Codex overrides only on request.

use super::*;
use crate::args::Command;
use crate::computer_use::{session_env::MODE_ENV, SERVER_NAME};
use std::path::Path;

const EXE: &str = "/opt/clud/clud";

fn first_party() -> ResolvedLaunchTarget {
    ResolvedLaunchTarget {
        routing_mode: RoutingMode::Direct,
        model_provider: ModelProvider::Claude,
        requested_harness: HarnessSelection::Claude,
        effective_harness: Backend::Claude,
        provider_source: PreferenceSource::BuiltInDefault,
        harness_source: PreferenceSource::BuiltInDefault,
    }
}

fn tokens(raw: &[&str], target: ResolvedLaunchTarget) -> (Vec<String>, String) {
    let args = parse(raw);
    let mut instructions = String::new();
    let tokens = super::super::builder::computer_use_claude_tokens(
        &args,
        target,
        Path::new(EXE),
        &mut instructions,
    );
    (tokens, instructions)
}

fn server(tokens: &[String]) -> serde_json::Value {
    let json = tokens
        .iter()
        .find_map(|t| t.strip_prefix("--mcp-config="))
        .expect("cua server registered");
    serde_json::from_str::<serde_json::Value>(json).unwrap()["mcpServers"][SERVER_NAME].clone()
}

#[test]
fn a_default_launch_registers_cua_unrestricted_and_never_prompts() {
    let (tokens, instructions) = tokens(&["clud"], first_party());
    let server = server(&tokens);
    assert_eq!(server["command"], EXE);
    assert_eq!(server["args"], serde_json::json!(["computer-use", "mcp"]));
    assert_eq!(server["env"][MODE_ENV], "unrestricted");
    assert!(tokens.contains(&"--allowedTools=mcp__cua".to_string()));
    assert!(instructions.contains("computer-use"), "{instructions}");
}

#[test]
fn safe_keeps_the_drivers_standard_profile_but_still_never_prompts() {
    let (tokens, _) = tokens(&["clud", "--safe"], first_party());
    assert_eq!(server(&tokens)["env"][MODE_ENV], "standard");
    assert!(tokens.contains(&"--allowedTools=mcp__cua".to_string()));
}

#[test]
fn no_computer_use_leaves_it_out() {
    let (tokens, instructions) = tokens(&["clud", "--no-computer-use"], first_party());
    assert!(tokens.is_empty());
    assert!(instructions.is_empty());
}

#[test]
fn gateway_routes_get_it_only_on_request() {
    let mut gateway = first_party();
    gateway.model_provider = ModelProvider::OpenRouter;
    assert!(tokens(&["clud"], gateway).0.is_empty());
    assert!(tokens(&["clud"], bridge_target()).0.is_empty());
    let (forced, _) = tokens(&["clud", "--computer-use"], gateway);
    assert_eq!(server(&forced)["env"][MODE_ENV], "unrestricted");
}

#[test]
fn codex_registers_only_with_the_flag() {
    let args = parse(&["clud", "--codex"]);
    let mut instructions = String::new();
    let none = super::super::builder::computer_use_codex_overrides(
        &args,
        Some(Path::new(EXE)),
        &mut instructions,
    );
    assert!(none.is_empty());
    assert!(instructions.is_empty());
    let args = parse(&["clud", "--codex", "--computer-use", "--safe"]);
    let overrides = super::super::builder::computer_use_codex_overrides(
        &args,
        Some(Path::new(EXE)),
        &mut instructions,
    );
    assert!(instructions.contains("clud-computer-use"));
    assert_eq!(overrides.first().map(String::as_str), Some("-c"));
    assert!(overrides.contains(&format!("mcp_servers.cua.command=\"{EXE}\"")));
    assert!(overrides
        .iter()
        .any(|o| o == &format!("mcp_servers.cua.env={{{MODE_ENV}=\"standard\"}}")));
}

#[test]
fn computer_use_flags_and_subcommand_parse_as_clud_options() {
    let args = parse(&["clud", "--no-computer-use", "-p", "hi"]);
    assert!(args.no_computer_use);
    assert!(args.passthrough.is_empty(), "{:?}", args.passthrough);
    let args = parse(&["clud", "computer-use", "status", "--json"]);
    assert!(matches!(
        args.command,
        Some(Command::ComputerUse {
            action: crate::computer_use::Action::Status { json: true }
        })
    ));
    assert!(args.passthrough.is_empty(), "{:?}", args.passthrough);
}

#[test]
fn unit_test_plans_stay_free_of_host_paths() {
    // `computer_use_exe` is None under cfg(test), so exact-argv tests stay pure.
    let plan = plan(&["clud"]);
    assert!(!plan.command.iter().any(|arg| arg.contains("mcp__cua")));
}

#[test]
fn a_headless_one_shot_skips_it_unless_asked_but_loops_keep_it() {
    // `claude -p` waits for every pending MCP server before its first turn.
    assert!(tokens(&["clud", "-p", "hi"], first_party()).0.is_empty());
    let (forced, _) = tokens(&["clud", "--computer-use", "-p", "hi"], first_party());
    assert!(!forced.is_empty());
    assert!(!tokens(&["clud", "loop", "do the thing"], first_party())
        .0
        .is_empty());
}

//! OpenCode's argv and child-local Messages-provider configuration.
//!
//! The model provider remains a clud provider; OpenCode is only the harness.
//! No user auth/config files are written and no Claude hook adapter is applied.

use crate::args::Args;
use crate::backend::{Backend, ModelProvider, ResolvedLaunchTarget};
use crate::command::LaunchPlan;
use crate::provider_catalog::ResolvedModelSelection;
use serde_json::{json, Value};

/// Claude's catalog IDs name Claude Code aliases, which the Messages API
/// does not accept. Reviewed API IDs: platform.claude.com/docs/en/models/overview
/// (2026-11-02). Other providers already carry their API wire IDs.
pub(crate) fn wire_model(
    provider: ModelProvider,
    selection: Option<&ResolvedModelSelection>,
) -> Option<&str> {
    let wire = crate::route_plan::direct_wire_model(provider, selection);
    if provider != ModelProvider::Claude {
        return wire;
    }
    Some(match wire {
        None | Some("sonnet") => "claude-sonnet-5-5",
        Some("opus") => "claude-opus-5-5",
        Some("haiku") => "claude-haiku-5-5",
        Some("fable") => "claude-fable-5-1",
        Some(wire) => wire,
    })
}

/// Reject options whose semantics OpenCode does not implement for clud.
pub fn unsupported_option(args: &Args) -> Option<&'static str> {
    if args.message.is_some() {
        Some("OpenCode does not support --message; use -p for a prompt")
    } else if matches!(args.resume, Some(None)) {
        Some("OpenCode requires --resume=<session>; use --continue for the last session")
    } else if args.effort.is_some() || args.context_window.is_some() {
        Some("OpenCode does not support clud --effort or --context-window; pass native options after --")
    } else if args.unattended || args.allow_plan_mode {
        Some("OpenCode does not support clud --unattended or --allow-plan-mode")
    } else if !args.codex_config_overrides.is_empty() || !args.allow_model.is_empty() {
        Some("OpenCode does not support Codex config overrides or --allow-model")
    } else {
        std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .and_then(|base| normalize_base(&base).err())
    }
}

/// Emit OpenCode's flags before any generated prompt or native passthrough.
pub fn append_launch_args(
    args: &Args,
    target: ResolvedLaunchTarget,
    selection: Option<&ResolvedModelSelection>,
    command: &mut Vec<String>,
) {
    if target.effective_harness != Backend::OpenCode {
        return;
    }
    if crate::command::has_noninteractive_prompt(args, Backend::OpenCode) {
        command.extend(["run".into(), "--format".into(), "json".into()]);
    }
    if !args.safe {
        command.push("--auto".into());
    }
    if args.continue_session {
        command.push("--continue".into());
    } else if let Some(Some(session)) = &args.resume {
        command.extend(["--session".into(), session.clone()]);
    }
    if let Some(wire) = wire_model(target.model_provider, selection) {
        command.extend(["--model".into(), format!("clud/{wire}")]);
    }
}

fn object_entry<'a>(parent: &'a mut Value, name: &str) -> Result<&'a mut Value, String> {
    let map = parent
        .as_object_mut()
        .ok_or("OpenCode config must be a JSON object")?;
    let entry = map.entry(name).or_insert_with(|| json!({}));
    if !entry.is_object() {
        return Err(format!("OpenCode config {name} must be a JSON object"));
    }
    Ok(entry)
}

fn normalize_base(base: &str) -> Result<String, &'static str> {
    let base = url::Url::parse(base).map_err(|_| "invalid OpenCode base URL")?;
    if !matches!(base.scheme(), "http" | "https")
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("OpenCode base URL must be HTTP(S) without credentials, query or fragment");
    }
    let base = base.as_str().trim_end_matches('/');
    Ok(if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    })
}

/// Fill gaps in canonical served metadata for the native defaults this
/// adapter selects. Sources and review date are recorded in opencode.md.
pub(crate) fn context_window(wire: &str) -> Option<u32> {
    crate::server_settings::effective_context_window(wire).or_else(|| {
        match wire.strip_suffix("[1m]").unwrap_or(wire) {
            "claude-sonnet-5-5" | "claude-opus-5-5" | "claude-haiku-5-5" | "claude-fable-5-1" => {
                Some(1_000_000)
            }
            "deepseek-flash" | "deepseek-v4-flash" | "deepseek-v4-pro" | "kimi-k3" => {
                Some(1_048_576)
            }
            _ => None,
        }
    })
}

fn configure_limits(model: &mut Value, context: Option<u32>) -> Result<(), String> {
    let limits = object_entry(model, "limit")?;
    if limits.get("context").is_none() {
        limits["context"] = json!(context.ok_or("unknown OpenCode model: supply positive limit.context and limit.output in OPENCODE_CONFIG_CONTENT")?);
    }
    if limits.get("output").is_none() {
        if context.is_none() {
            return Err(
                "unknown OpenCode model: supply positive limit.output in OPENCODE_CONFIG_CONTENT"
                    .into(),
            );
        }
        // This is a request budget below the reviewed native maxima, not a
        // claim that the provider's maximum output is only 32k.
        limits["output"] = json!(32_000);
    }
    let context = limits["context"].as_u64().filter(|value| *value > 0);
    let output = limits["output"].as_u64().filter(|value| *value > 0);
    if !matches!((context, output), (Some(context), Some(output)) if output < context) {
        return Err(
            "OpenCode model limits must be positive integers with output below context".into(),
        );
    }
    Ok(())
}

fn configure(
    existing: Option<&str>,
    wire: &str,
    base: &str,
    key: &str,
    context: Option<u32>,
) -> Result<String, String> {
    let mut config: Value = existing
        .map_or_else(|| Ok(json!({})), serde_json::from_str)
        .map_err(|_| "OPENCODE_CONFIG_CONTENT must contain valid JSON".to_string())?;
    let base = normalize_base(base)?;
    let provider = object_entry(object_entry(&mut config, "provider")?, "clud")?;
    provider["npm"] = json!("@ai-sdk/anthropic");
    provider["name"] = json!("clud");
    let options = object_entry(provider, "options")?;
    options["baseURL"] = json!(base);
    options["apiKey"] = json!(key);
    let model = object_entry(object_entry(provider, "models")?, wire)?;
    model["name"] = json!(wire);
    configure_limits(model, context)?;
    config["model"] = json!(format!("clud/{wire}"));
    config["small_model"] = json!(format!("clud/{wire}"));
    config["enabled_providers"] = json!(["clud"]);
    serde_json::to_string(&config).map_err(|_| "could not encode OpenCode configuration".into())
}

/// Build only a child environment overlay. Credentials never enter argv or the plan.
pub fn prepare_child(plan: &LaunchPlan, env: &mut Vec<(String, String)>) -> Result<(), String> {
    use crate::provider_auth::SecretStore as _;
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(name).ok())
    };
    let route = crate::route_plan::route_of(plan, &crate::route_plan::Ambient::from_child_env(env));
    let wire = route
        .slots
        .main
        .as_ref()
        .map(|slot| slot.wire_id.as_str())
        .ok_or("OpenCode requires an explicit or reviewed default model")?;
    let base = route
        .upstreams
        .first()
        .map(|upstream| upstream.base_url.as_str())
        .ok_or("OpenCode requires a Messages API upstream")?;
    let descriptor = crate::provider_registry::descriptor_for(plan.model_provider());
    let key = if let Some(descriptor) = descriptor {
        lookup(descriptor.api_key_env).or_else(|| {
            crate::provider_auth::NativeSecretStore::new_for(descriptor.vault_service, descriptor.vault_account)
                .ok()?.get().ok().flatten()
        })
    } else if plan.model_provider() == ModelProvider::Claude {
        lookup("ANTHROPIC_API_KEY").or_else(|| lookup("ANTHROPIC_AUTH_TOKEN"))
    } else {
        None
    }.filter(|key| !key.trim().is_empty())
        .ok_or("OpenCode requires this provider's API key; configure its environment variable or clud auth")?;
    let config = configure(
        lookup("OPENCODE_CONFIG_CONTENT").as_deref(),
        wire,
        base,
        &key,
        route.context.max_tokens,
    )?;
    env.retain(|(name, _)| !name.eq_ignore_ascii_case("OPENCODE_CONFIG_CONTENT"));
    env.push(("OPENCODE_CONFIG_CONTENT".into(), config));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(options: &[&str]) -> LaunchPlan {
        let raw = ["clud", "--claude", "--harness", "opencode"]
            .into_iter()
            .chain(options.iter().copied())
            .map(str::to_string)
            .collect();
        let args = Args::parse_from_raw(raw);
        let target =
            crate::backend::resolve_launch_target(true, false, false, args.harness, None, None)
                .unwrap();
        crate::command::build_launch_plan_for_target(&args, target, "opencode")
    }

    #[test]
    fn prompt_resume_and_safe_mode_use_opencode_grammar() {
        let unsafe_plan = plan(&["-p", "read this", "--resume=ses_123"]);
        assert_eq!(
            &unsafe_plan.command[..4],
            ["opencode", "run", "--format", "json"]
        );
        assert!(unsafe_plan.command.contains(&"--auto".into()));
        assert!(unsafe_plan
            .command
            .windows(2)
            .any(|pair| pair == ["--session", "ses_123"]));
        assert!(!unsafe_plan.command.contains(&"-p".into()));
        assert_eq!(unsafe_plan.command.last().unwrap(), "read this");
        assert_eq!(
            unsafe_plan.launch_mode,
            crate::backend::LaunchMode::Subprocess
        );
        let safe_plan = plan(&["--safe", "--continue"]);
        assert!(!safe_plan.command.contains(&"run".into()));
        assert!(!safe_plan.command.contains(&"--auto".into()));
        assert!(safe_plan.command.contains(&"--continue".into()));
    }

    #[test]
    fn harness_preferences_round_trip_and_unsupported_routes_fail() {
        use crate::backend::{HarnessSelection, RoutingMode};
        assert_eq!(
            HarnessSelection::from_settings_str("opencode"),
            Some(HarnessSelection::OpenCode)
        );
        assert_eq!(
            Backend::from_settings_str("opencode"),
            Some(Backend::OpenCode)
        );
        assert_eq!(
            serde_json::to_string(&HarnessSelection::OpenCode).unwrap(),
            "\"opencode\""
        );
        for (mode, provider) in [
            (RoutingMode::Unified, ModelProvider::DeepSeek),
            (RoutingMode::Direct, ModelProvider::Codex),
        ] {
            assert!(crate::backend::resolve_routed_launch_target(
                mode,
                Some(provider),
                Some(HarnessSelection::OpenCode),
                None,
                None
            )
            .is_err());
        }
    }

    #[test]
    fn overlay_preserves_user_settings_and_uses_only_the_selected_provider() {
        let config = configure(
            Some(r#"{"theme":"catppuccin","permission":{"bash":"deny"},"provider":{"other":{}}}"#),
            "deepseek-chat",
            "http://127.0.0.1:1234/anthropic/",
            "client-canary",
            Some(1_048_576),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&config).unwrap();
        assert_eq!(value["theme"], "catppuccin");
        assert_eq!(value["permission"]["bash"], "deny");
        assert_eq!(value["enabled_providers"], json!(["clud"]));
        assert_eq!(
            value["provider"]["clud"]["options"]["baseURL"],
            "http://127.0.0.1:1234/anthropic/v1"
        );
        assert_eq!(
            value["provider"]["clud"]["options"]["apiKey"],
            "client-canary"
        );
        assert!(value["provider"]["other"].is_object());
        assert_eq!(
            value["provider"]["clud"]["models"]["deepseek-chat"]["limit"]["context"],
            1_048_576
        );
        assert_eq!(
            value["provider"]["clud"]["models"]["deepseek-chat"]["limit"]["output"],
            32_000
        );
    }

    #[test]
    fn unknown_models_require_limits_and_explicit_limits_are_preserved() {
        assert!(configure(None, "unknown", "https://api.example/v1", "secret", None).is_err());
        let existing = r#"{"provider":{"clud":{"models":{"unknown":{"limit":{"context":16000,"output":4096}}}}}}"#;
        let configured = configure(
            Some(existing),
            "unknown",
            "https://api.example/v1",
            "secret",
            None,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&configured).unwrap();
        assert_eq!(
            value["provider"]["clud"]["models"]["unknown"]["limit"]["context"],
            16000
        );
        assert_eq!(
            value["provider"]["clud"]["models"]["unknown"]["limit"]["output"],
            4096
        );
        for limits in [
            json!({"context":0,"output":1}),
            json!({"context":16000,"output":0}),
        ] {
            let mut model = json!({"limit": limits});
            assert!(configure_limits(&mut model, Some(1_000_000)).is_err());
        }
    }

    #[test]
    fn malformed_config_and_credential_bearing_urls_fail_without_echoing_secrets() {
        for existing in ["[]", "{", r#"{"provider":null}"#] {
            assert!(configure(
                Some(existing),
                "model",
                "https://api.example/v1",
                "secret",
                None
            )
            .is_err());
        }
        let error = configure(
            None,
            "model",
            "https://user:secret@api.example/v1",
            "secret",
            None,
        )
        .unwrap_err();
        assert!(!error.contains("secret"));
    }
}

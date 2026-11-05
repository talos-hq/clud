//! Session-scoped inference routing. Never consult or persist provider credentials.
use std::collections::BTreeMap;
use std::path::Path;

use crate::args::Args;
use crate::backend::{Backend, HarnessSelection, ModelProvider};
use zeroize::Zeroizing;

const ORIGIN: &str = "CLUD_REMOTE_PROXY_ORIGIN";
const TOKEN: &str = "CLUD_REMOTE_PROXY_TOKEN";
const MODEL: &str = "CLUD_REMOTE_PROXY_MODEL";

pub struct SessionProxy {
    origin: String,
    token: Zeroizing<String>,
    model: String,
}

impl SessionProxy {
    pub fn new(origin: &str, token: &str, model: &str) -> Result<Self, String> {
        let origin = super::transport::base_url(origin)?;
        if !token.starts_with("ct_")
            || token.len() < 20
            || token.len() > 256
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("invalid remote session bearer".into());
        }
        if model.trim().is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
            return Err("invalid remote proxy model".into());
        }
        Ok(Self {
            origin: origin.to_string(),
            token: Zeroizing::new(token.into()),
            model: model.into(),
        })
    }

    pub fn from_process() -> Result<Option<Self>, String> {
        let origin = std::env::var(ORIGIN).ok();
        let token = std::env::var(TOKEN).ok();
        let model = std::env::var(MODEL).ok();
        match (origin, token, model) {
            (None, None, None) => Ok(None),
            (Some(origin), Some(token), Some(model)) => {
                Self::new(&origin, &token, &model).map(Some)
            }
            _ => Err("incomplete remote proxy context; refusing local credential fallback".into()),
        }
    }

    pub fn environment(
        &self,
        ambient: Vec<(String, String)>,
        config: &Path,
    ) -> Vec<(String, String)> {
        let mut env: BTreeMap<_, _> = ambient.into_iter().filter(|(key, _)| !scrub(key)).collect();
        for (key, value) in [
            (ORIGIN, self.origin.clone()),
            (TOKEN, self.token.to_string()),
            (MODEL, self.model.clone()),
            ("ANTHROPIC_BASE_URL", format!("{}api/proxy", self.origin)),
            ("ANTHROPIC_AUTH_TOKEN", self.token.to_string()),
            ("OPENAI_BASE_URL", format!("{}api/proxy/v1", self.origin)),
            ("OPENAI_API_KEY", self.token.to_string()),
            ("ANTHROPIC_MODEL", self.model.clone()),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", self.model.clone()),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", self.model.clone()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", self.model.clone()),
            ("CLAUDE_CODE_SUBAGENT_MODEL", self.model.clone()),
            (
                "ANTHROPIC_CONFIG_DIR",
                config.join("anthropic").to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR",
                config.join("claude").to_string_lossy().into_owned(),
            ),
            (
                "CODEX_HOME",
                config.join("codex").to_string_lossy().into_owned(),
            ),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into()),
            ("CLAUDE_CODE_USE_BEDROCK", "0".into()),
            ("CLAUDE_CODE_USE_VERTEX", "0".into()),
            ("CLAUDE_CODE_USE_FOUNDRY", "0".into()),
            ("TERM", "xterm-256color".into()),
        ] {
            env.insert(key.into(), value);
        }
        env.into_iter().collect()
    }

    /// Mark the session's own Claude configuration as onboarded. The first-run
    /// wizard is not needed: the session bearer is the only credential, and
    /// this file holds none.
    pub fn seed_agent_config(config: &Path) -> std::io::Result<()> {
        let claude = config.join("claude");
        std::fs::create_dir_all(&claude)?;
        std::fs::write(
            claude.join(".claude.json"),
            r#"{"hasCompletedOnboarding":true}"#,
        )
    }

    pub fn runtime_environment(
        &self,
        env: Vec<(String, String)>,
    ) -> Result<Vec<(String, String)>, String> {
        let config = std::env::var("CLAUDE_CONFIG_DIR")
            .map_err(|_| "remote session has no isolated credential configuration")?;
        let parent = Path::new(&config)
            .parent()
            .ok_or("invalid remote configuration directory")?;
        Ok(self.environment(env, parent))
    }
}

/// True inside a consented remote shell, where nothing should stop to ask for
/// local launch preferences.
pub fn active() -> bool {
    std::env::var_os(ORIGIN).is_some()
}

fn scrub(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.ends_with("_API_KEY")
        || key.ends_with("_AUTH_TOKEN")
        || key.ends_with("_OAUTH_TOKEN")
        || key.starts_with("ANTHROPIC_")
        || key.starts_with("OPENAI_")
        || key.starts_with("OPENROUTER_")
        || matches!(
            key.as_str(),
            ORIGIN
                | TOKEN
                | MODEL
                | "CLAUDE_CODE_API_KEY_HELPER"
                | "CLAUDE_CODE_CUSTOM_HEADERS"
                | "CLAUDE_CODE_USE_BEDROCK"
                | "CLAUDE_CODE_USE_VERTEX"
                | "CLAUDE_CODE_USE_FOUNDRY"
                | "CLAUDE_CONFIG_DIR"
                | "CODEX_HOME"
                | "CODEX_AUTH_JSON"
                | "ENV"
                | "BASH_ENV"
                | "ZDOTDIR"
                | "CLUD_PROVIDER"
                | "CLUD_SUBSCRIPTION_PROVIDER"
                | "CLUD_ALLOW_METERED"
                | "CLUD_METERED_PROVIDER"
        )
}

/// Select an explicit direct harness before any credential-based picker runs.
pub fn configure_launch(args: &mut Args) -> Result<(), String> {
    let Some(proxy) = SessionProxy::from_process()? else {
        return Ok(());
    };
    if args.command.is_some() {
        return Err("remote proxy supports foreground agent launches only".into());
    }
    configure_for(args, proxy)
}

fn configure_for(args: &mut Args, proxy: SessionProxy) -> Result<(), String> {
    if args
        .explicit_model_provider()
        .is_some_and(|p| p != ModelProvider::Claude)
        || args.harness.is_some_and(|h| h != HarnessSelection::Claude)
        || args.unified
        || args.mode.is_some()
        || args.detach
        || args.detachable
        || args.experimental_daemon_centralized
        || args.inline_api_key.is_some()
        || args.passthrough.iter().any(|arg| {
            arg == "--settings"
                || arg.starts_with("--settings=")
                || arg == "--setting-sources"
                || arg.starts_with("--setting-sources=")
        })
    {
        return Err(
            "remote sessions require the foreground Claude harness using the session proxy".into(),
        );
    }
    args.provider = Some(ModelProvider::Claude);
    args.harness = Some(HarnessSelection::Claude);
    args.no_daemon = true;
    // No apiKeyHelper or stored provider configuration from user/project settings.
    args.passthrough
        .extend(["--setting-sources".into(), String::new()]);
    if args.model.is_none() {
        args.model = Some(proxy.model);
    }
    Ok(())
}

pub fn validate_plan(plan: &crate::command::LaunchPlan) -> Result<(), String> {
    if plan.effective_harness() != Backend::Claude
        || plan.model_provider() != ModelProvider::Claude
        || plan.routing_mode != crate::backend::RoutingMode::Direct
    {
        return Err("remote proxy cannot use a local provider route".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    const BEARER: &str = "ct_ephemeral_session_bearer_only_123456";

    #[test]
    fn credential_overlay_replaces_local_provider_sources_without_persisting() {
        let proxy = SessionProxy::new("https://clud.bot", BEARER, "openrouter/auto").unwrap();
        let config = tempfile::tempdir().unwrap();
        let env: BTreeMap<_, _> = proxy
            .environment(
                vec![
                    ("PATH".into(), "/bin".into()),
                    ("HOME".into(), "/original-home".into()),
                    ("OPENROUTER_API_KEY".into(), "own-openrouter-key".into()),
                    ("anthropic_auth_token".into(), "own-subscription".into()),
                    ("ANTHROPIC_API_KEY".into(), "own-anthropic-key".into()),
                    ("CLAUDE_CODE_OAUTH_TOKEN".into(), "own-oauth".into()),
                    (
                        "ANTHROPIC_CUSTOM_HEADERS".into(),
                        "Authorization: own-key".into(),
                    ),
                    ("CLAUDE_CODE_USE_BEDROCK".into(), "1".into()),
                ],
                config.path(),
            )
            .into_iter()
            .collect();
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], BEARER);
        assert_eq!(env["OPENAI_API_KEY"], BEARER);
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://clud.bot/api/proxy");
        assert_eq!(env["OPENAI_BASE_URL"], "https://clud.bot/api/proxy/v1");
        assert_eq!(env["HOME"], "/original-home");
        assert_eq!(env["PATH"], "/bin");
        assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "0");
        assert!(!env.keys().any(|k| k == "OPENROUTER_API_KEY"
            || k == "ANTHROPIC_API_KEY"
            || k == "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!serde_json::to_string(&env).unwrap().contains("own-"));
        assert!(Path::new(&env["CLAUDE_CONFIG_DIR"]).starts_with(config.path()));
        assert_eq!(std::fs::read_dir(config.path()).unwrap().count(), 0);
    }

    #[test]
    fn seeded_config_skips_onboarding_and_holds_no_credential() {
        let config = tempfile::tempdir().unwrap();
        SessionProxy::seed_agent_config(config.path()).unwrap();
        let text = std::fs::read_to_string(config.path().join("claude/.claude.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value, serde_json::json!({"hasCompletedOnboarding": true}));
    }

    #[test]
    fn invalid_capabilities_fail_without_echoing_secrets() {
        for origin in [
            "http://clud.bot",
            "https://user:pass@clud.bot",
            "https://clud.bot/other",
            "https://clud.bot/?key=x",
        ] {
            assert!(SessionProxy::new(origin, BEARER, "m").is_err());
        }
        for token in ["", "own-secret", "ct_bad\nsecret_abcdefghijklmnop"] {
            let error = SessionProxy::new("https://clud.bot", token, "m")
                .err()
                .unwrap();
            assert!(!error.contains(token) || token.is_empty());
        }
        assert!(SessionProxy::new("http://127.0.0.1:8080", BEARER, "m").is_ok());
        assert!(SessionProxy::new("https://clud.bot", BEARER, "m\nbad").is_err());
    }

    #[test]
    fn nested_launch_bypasses_provider_selection_and_refuses_fallback_routes() {
        let proxy = || SessionProxy::new("https://clud.bot", BEARER, "openrouter/auto").unwrap();
        let mut args = Args::parse_from(["clud", "--safe", "-p", "hello"]);
        configure_for(&mut args, proxy()).unwrap();
        assert_eq!(args.explicit_model_provider(), Some(ModelProvider::Claude));
        assert_eq!(args.harness, Some(HarnessSelection::Claude));
        assert!(args.no_daemon);
        assert!(args.safe);
        assert_eq!(args.model.as_deref(), Some("openrouter/auto"));
        for flag in [
            "--codex",
            "--openrouter",
            "--unified",
            "--detach",
            "--detachable",
        ] {
            let mut args = Args::parse_from(["clud", flag]);
            assert!(
                configure_for(&mut args, proxy()).is_err(),
                "{flag} must not escape the session route"
            );
        }
    }
}

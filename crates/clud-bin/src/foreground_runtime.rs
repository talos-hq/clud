//! Foreground child runtime for provider/harness cross-routes (issue #626).

use crate::backend::{Backend, ModelProvider, RoutingMode};
use crate::clud_settings::CodexCliLoginImport;
use crate::codex_bridge::{BridgeConfig, BridgeError, BridgeHandle, UNIFIED_GATEWAY_TOKEN_HEADER};
use crate::codex_model::ModelSpec;
use crate::command::LaunchPlan;
use crate::selector::{self, Key, Note, Row, Selector, Step, View};
use crate::subprocess::ManagedSubprocess;
use running_process::pty::NativePtyProcess;
use std::fmt;
use std::io::Write;
#[cfg(test)]
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

// #1855: the timeouts, the scrub lists and the bridge's injected role rows are
// routing *decisions*, so they live in `route_plan` beside the resolver that
// owns them. These aliases keep this module's render call sites reading the
// same names they always did.
use crate::route_plan::{
    CODEX_BRIDGE_OPUS_MODEL as CODEX_VIA_CLAUDE_OPUS_MODEL,
    CODEX_BRIDGE_SONNET_MODEL as CODEX_VIA_CLAUDE_SONNET_MODEL,
    DIRECT_TIMEOUT_MS as DIRECT_API_TIMEOUT_MS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexCliImportChoice {
    Import,
    NotNow,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexCliImportMode {
    Initial,
    RepairSameAccount,
    SwitchAccount,
    AmbiguousAccount,
}

fn codex_cli_import_mode(
    current: Option<&crate::codex_auth::SubscriptionCredentials>,
    native: &crate::codex_auth::SubscriptionCredentials,
) -> CodexCliImportMode {
    let Some(current) = current else {
        return CodexCliImportMode::Initial;
    };
    match (
        current.account_id.as_deref(),
        native.account_id.as_deref(),
        current.email.as_deref(),
        native.email.as_deref(),
    ) {
        (Some(left_id), Some(right_id), Some(left_email), Some(right_email))
            if left_id == right_id && left_email.eq_ignore_ascii_case(right_email) =>
        {
            CodexCliImportMode::RepairSameAccount
        }
        (Some(left_id), Some(right_id), _, _) if left_id != right_id => {
            CodexCliImportMode::SwitchAccount
        }
        (_, _, Some(left_email), Some(right_email))
            if !left_email.eq_ignore_ascii_case(right_email) =>
        {
            CodexCliImportMode::SwitchAccount
        }
        _ => CodexCliImportMode::AmbiguousAccount,
    }
}

#[derive(Debug)]
struct CodexCliImportSelector {
    email: String,
    account_id: Option<String>,
    mode: CodexCliImportMode,
    selected: usize,
}

impl CodexCliImportSelector {
    fn new(email: Option<String>) -> Self {
        Self {
            email: email.unwrap_or_else(|| "your Codex CLI account".to_string()),
            account_id: None,
            mode: CodexCliImportMode::Initial,
            selected: 0,
        }
    }

    fn for_replacement(
        record: &crate::codex_auth::SubscriptionCredentials,
        mode: CodexCliImportMode,
    ) -> Self {
        Self {
            email: record
                .email
                .clone()
                .unwrap_or_else(|| "an unknown Codex CLI user".to_string()),
            account_id: record.account_id.clone(),
            mode,
            selected: 0,
        }
    }

    fn choice(&self) -> CodexCliImportChoice {
        [
            CodexCliImportChoice::Import,
            CodexCliImportChoice::NotNow,
            CodexCliImportChoice::Never,
        ][self.selected]
    }
}

impl Selector for CodexCliImportSelector {
    type Outcome = CodexCliImportChoice;

    fn view(&self, _elapsed: std::time::Duration) -> View {
        let action = match self.mode {
            CodexCliImportMode::Initial => ("Yes, import it", "copies into clud's own store"),
            CodexCliImportMode::RepairSameAccount => {
                ("Yes, repair clud login", "replaces the rejected clud copy")
            }
            CodexCliImportMode::SwitchAccount => (
                "Yes, switch accounts",
                "changes the account used by this bridge",
            ),
            CodexCliImportMode::AmbiguousAccount => (
                "Yes, replace clud login",
                "account identity could not be confirmed",
            ),
        };
        let labels = [
            action,
            ("Not now", "this launch only"),
            ("No, don't ask again", "persisted"),
        ];
        let title = match self.mode {
            CodexCliImportMode::Initial => format!(
                "Codex CLI login found for {}. Use it for the Claude harness bridge?",
                self.email
            ),
            CodexCliImportMode::RepairSameAccount => format!(
                "Clud's Codex login cannot refresh. Repair it from the Codex CLI login for {}?",
                self.email
            ),
            CodexCliImportMode::SwitchAccount => format!(
                "Clud's Codex login cannot refresh. Switch this bridge to {} (account {})?",
                self.email,
                self.account_id.as_deref().unwrap_or("unknown")
            ),
            CodexCliImportMode::AmbiguousAccount => format!(
                "Clud's Codex login cannot refresh. Replace it with the Codex CLI login for {}? Account identity cannot be confirmed.",
                self.email
            ),
        };
        View {
            title,
            hints: vec!["Up/Down move, Enter select, Esc not now".to_string()],
            gap: false,
            rows: labels
                .into_iter()
                .enumerate()
                .map(|(index, (label, note))| Row {
                    current: self.selected == index,
                    marker: String::new(),
                    label: label.to_string(),
                    note: Note::Inline(note.to_string()),
                })
                .collect(),
            footer: vec![
                "Refreshes stay in clud's copy; the Codex CLI may later need a re-login."
                    .to_string(),
            ],
        }
    }

    fn on_key(&mut self, key: Key) -> Step<Self::Outcome> {
        match key {
            Key::Up => {
                self.selected = self.selected.checked_sub(1).unwrap_or(2);
                Step::Redraw
            }
            Key::Down => {
                self.selected = (self.selected + 1) % 3;
                Step::Redraw
            }
            Key::Enter => Step::Done(self.choice()),
            Key::Escape => Step::Done(CodexCliImportChoice::NotNow),
            Key::Space | Key::Char(_) => Step::Stay,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    Subprocess,
    Pty,
}

/// Narrow environment-aware spawn seam shared by subprocess and PTY paths.
/// Tests record the exact child overlay without installing a Claude binary;
/// production adapters below delegate to the existing running-process types.
pub trait SpawnAdapter<Output> {
    type Error;

    fn spawn(
        &self,
        mode: SpawnMode,
        command: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<Output, Self::Error>;
}

pub struct ForegroundRuntime {
    env: Vec<(String, String)>,
    bridge: Option<BridgeHandle>,
    claude_settings: Option<ClaudeSettings>,
    startup_notices: Vec<String>,
}

struct ClaudeSettings {
    value: String,
    replaces_user_argument: bool,
    _temp_file: tempfile::NamedTempFile,
}

impl ForegroundRuntime {
    /// Validate the shared launch-admission requirements without creating a
    /// listener, settings file, child environment, or harness process. Daemon
    /// submission calls this before creating a durable session; `start` calls
    /// it again at the worker/foreground boundary to cover later credential
    /// loss and every non-daemon launch mode.
    pub fn preflight(plan: &LaunchPlan) -> Result<(), BridgeError> {
        if is_codex_via_claude(plan) && !is_unified(plan) {
            preflight_codex_bridge_credentials()?;
        }
        Ok(())
    }

    pub fn start(plan: &LaunchPlan, mut env: Vec<(String, String)>) -> Result<Self, BridgeError> {
        // The inherited-pin line leads every launch's notices: a boundary the
        // user did not type deserves to be seen first (#1257).
        let pin_notices =
            model_pin_notices(plan, std::io::IsTerminal::is_terminal(&std::io::stderr()));
        // `dsh` owns its provider configuration and credentials. A native
        // DeepSeek Harness launch needs no clud vault or bridge setup at all.
        if plan.effective_harness() == Backend::OpenCode {
            crate::opencode_harness::prepare_child(plan, &mut env)
                .map_err(BridgeError::Settings)?;
            return Ok(Self {
                env,
                bridge: None,
                claude_settings: None,
                startup_notices: pin_notices,
            });
        }
        if plan.effective_harness() == Backend::DeepSeek {
            apply_route_context(&mut env, plan);
            prepare_dsh_child(plan, &mut env)?;
            for notice in &pin_notices {
                eprintln!("{notice}");
            }
            return Ok(Self {
                env,
                bridge: None,
                claude_settings: None,
                startup_notices: pin_notices,
            });
        }
        // Prefer a descriptor resolved from the plan's provider so the store
        // is built with that provider's own vault identifiers. Unified plans
        // (provider `Claude`, no descriptor) fall back to the DeepSeek-scoped
        // constructor: `start_with_secret_store`'s unified branch still needs
        // to probe for a DeepSeek key even though DeepSeek is not this plan's
        // routed provider.
        let store = match crate::provider_registry::descriptor_for(plan.model_provider()) {
            Some(descriptor) => crate::provider_auth::NativeSecretStore::new_for(
                descriptor.vault_service,
                descriptor.vault_account,
            ),
            None => crate::provider_auth::NativeSecretStore::new(),
        }
        .map_err(|_| BridgeError::AnthropicCompatCredentials)?;
        // This is the shared pre-child admission boundary for subprocess,
        // PTY, detached, and worker launches. It must precede BridgeHandle
        // construction so refusal cannot bind a listener or expose settings.
        Self::preflight(plan)?;
        // #1675: the env as the launch received it, before any overlay, so
        // the launch-context record can tell an ambient value from ours.
        let ambient = env.clone();
        let mut runtime = Self::start_with_secret_store(plan, env, &store)?;
        runtime.apply_initial_effort(plan)?;
        runtime.apply_attribution(plan)?;
        runtime.apply_session_history(plan);
        if plan.effective_harness() == Backend::Claude && !cfg!(test) {
            crate::launch_context::record_launch(
                &launch_context_plan_facts(plan),
                &ambient,
                &mut runtime.env,
            );
        }
        if !pin_notices.is_empty() {
            let mut notices = pin_notices;
            notices.append(&mut runtime.startup_notices);
            runtime.startup_notices = notices;
        }
        for notice in &runtime.startup_notices {
            eprintln!("{notice}");
        }
        Ok(runtime)
    }

    /// Routing core, seamed on the secret-store dependency so tests can
    /// exercise the DeepSeek direct route without touching the host's real
    /// native credential vault. `start` is the sole production entry point.
    #[expect(
        clippy::too_many_lines,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    fn start_with_secret_store(
        plan: &LaunchPlan,
        mut env: Vec<(String, String)>,
        store: &dyn crate::provider_auth::SecretStore,
    ) -> Result<Self, BridgeError> {
        // Every launch check runs once, here, against the route and before
        // any backend branch: a refusal must not depend on which renderer was
        // about to run (#1862).
        let route =
            crate::route_plan::route_of(plan, &crate::route_plan::Ambient::from_child_env(&env));
        if let Some(refusal) = crate::route_plan::first_refusal(
            &route,
            &crate::openrouter_catalog::catalog_cached_or_embedded(),
        ) {
            return Err(BridgeError::Model(refusal));
        }
        let (bridge, claude_settings, mut startup_notices) = if is_unified(plan) {
            let integration_upstreams =
                crate::codex_bridge::unified_integration_upstreams_from_process();
            // Optional routes must never block native Claude. Resolve only
            // availability metadata here; the actual credentials stay inside
            // the launch-scoped bridge and are not serialized into the plan.
            // #901: fixed fixture keys unless the test vault holds the real
            // (test) credentials, so logout is observable end to end.
            let fixture_keys =
                integration_upstreams.is_some() && !crate::provider_auth::test_vault_active();
            let kimi_fake = integration_has_kimi(&integration_upstreams);
            // One key lookup, driven by the route's own credential sources:
            // the vault identifiers come from each provider's registry
            // descriptor, so this path names no vault constant itself.
            let key_for = |provider: ModelProvider| -> Option<String> {
                if fixture_keys {
                    return Some(format!("clud-test-{}-key", provider.as_str()));
                }
                // Subscription-first: a metered key joins the gateway only
                // with the operator's two-override consent for it.
                if !crate::metered_policy::process_allows_metered(provider) {
                    return None;
                }
                // Integration runs probe Kimi only when a Kimi fake exists, so
                // a stored key can never reach the real Moonshot endpoint from
                // a test.
                if provider == ModelProvider::Kimi
                    && integration_upstreams.is_some()
                    && (fixture_keys || !kimi_fake)
                {
                    return None;
                }
                // The injected store is this launch's provider-scoped one, and
                // is what a direct DeepSeek launch reads through too.
                if provider == ModelProvider::DeepSeek {
                    return store.get().ok().flatten();
                }
                let descriptor = crate::provider_registry::descriptor_for(provider)?;
                let vault = crate::provider_auth::NativeSecretStore::new_for(
                    descriptor.vault_service,
                    descriptor.vault_account,
                )
                .ok()?;
                let source = route
                    .upstreams
                    .iter()
                    .find(|upstream| upstream.provider == provider)
                    .map(|upstream| upstream.credential.clone())?;
                crate::route_plan::read_credential(&source, &vault)
                    .ok()
                    .flatten()
            };
            let deepseek_key = key_for(ModelProvider::DeepSeek);
            let openrouter_key = key_for(ModelProvider::OpenRouter);
            let kimi_key = key_for(ModelProvider::Kimi);
            let codex_available = integration_upstreams.is_some()
                || crate::codex_upstream::ResolvedCredentials::resolve_default().is_ok();
            let mut startup_notices = unified_startup_notices(
                codex_available,
                deepseek_key.is_some(),
                kimi_key.is_some() || (fixture_keys && kimi_fake),
            );
            if openrouter_key.is_none() {
                startup_notices.push(
                    "[clud] unified: OpenRouter is not configured; \
                     run `clud auth login openrouter` to add its route"
                        .to_string(),
                );
            }
            // An unroutable rung fails the launch rather than a turn: by the
            // time a request is in flight the user has already waited, and the
            // error would arrive wrapped in the harness's API-error framing.
            let failover = crate::failover::FailoverLadder::parse(
                plan.failover.as_deref().unwrap_or_default(),
                plan.failover_allow_metered,
            )
            .map_err(|error| BridgeError::Failover(error.to_string()))?;
            if !failover.withheld_for_consent().is_empty() {
                startup_notices.push(
                    "[clud] failover: metered rungs are listed but will not be taken; \
                     pass --failover-allow-metered to consent"
                        .to_string(),
                );
            }
            if crate::route_plan::discovery_is_disabled(&env) {
                return Err(BridgeError::DiscoveryDisabled);
            }
            let unified =
                crate::route_plan::render_unified_config(&route, codex_available, &key_for)
                    .with_failover(failover);
            let unified = integration_upstreams
                .as_ref()
                .map_or(unified.clone(), |upstreams| {
                    unified.with_integration_test_upstreams(upstreams)
                });
            let unified = unified
                .with_launch_model(
                    route.provider,
                    route.slots.main.as_ref().map(|slot| slot.wire_id.as_str()),
                )
                .with_redaction(plan.redact_secrets)
                .map_err(|error| BridgeError::Random(error.to_string()))?;
            if route.redaction.enabled {
                startup_notices.push(format!(
                    "[clud] secret redaction on ({}): secrets are tokenized before leaving this machine and restored in responses",
                    route.redaction.reason,
                ));
            }
            let config = BridgeConfig::default()
                .with_unified_gateway(unified)
                // #1257: the gateway refuses any model outside the launch's
                // allowlist, so the pin holds even though discovery is on.
                .with_allowed_models(route.allowlist.clone());
            let config = integration_upstreams
                .as_ref()
                .map_or(config.clone(), |upstreams| {
                    config.with_integration_test_codex_upstream(upstreams)
                });
            let bridge = BridgeHandle::start(config)?;
            // The user's own headers survive below the gateway's, so read them
            // before the overlay's scrub removes the key.
            let ambient_headers = env
                .iter()
                .find(|(key, _)| env_key_eq(key, "ANTHROPIC_CUSTOM_HEADERS"))
                .map(|(_, value)| value.trim().to_string())
                .filter(|value| !value.is_empty());
            crate::route_plan::render_unified_env(
                &route,
                bridge.base_url(),
                bridge.bearer_token(),
                ambient_headers.as_deref(),
            )
            .apply(&mut env);
            let settings = merged_unified_context_lifecycle_settings(plan, &bridge)?;
            (Some(bridge), Some(settings), startup_notices)
        } else if is_codex_via_claude(plan) {
            // A selection that does not parse fails the launch rather than
            // the first turn: by the time a request is in flight the user has
            // already waited, and the message would arrive wrapped in the
            // harness's own API-error framing.
            if crate::route_plan::discovery_is_disabled(&env) {
                return Err(BridgeError::DiscoveryDisabled);
            }
            let render = crate::route_plan::render_codex_bridge(
                &route,
                crate::route_plan::CodexBridgeRequest {
                    default_model: codex_selection_from_plan(plan)?,
                    selected_model_cli_id: plan
                        .model_selection
                        .as_ref()
                        .and_then(|selection| selection.model.as_deref()),
                },
            );
            let bridge = BridgeHandle::start(
                BridgeConfig::default()
                    .with_default_model(render.default_model)
                    .with_selected_model_cli_id(render.selected_model_cli_id)
                    .with_allowed_models(render.allowed_models),
            )?;
            crate::route_plan::render_codex_bridge_env(
                &route,
                bridge.base_url(),
                bridge.bearer_token(),
            )
            .apply(&mut env);
            let settings = merged_context_lifecycle_settings(plan, &bridge)?;
            (Some(bridge), Some(settings), Vec::new())
        } else if is_anthropic_compat_via_claude(plan) {
            // `is_anthropic_compat_via_claude` only returns true when a
            // descriptor resolves, so this `expect` cannot fail in practice;
            // it documents that invariant rather than silently defaulting.
            let descriptor = crate::provider_registry::descriptor_for(plan.model_provider())
                .expect("is_anthropic_compat_via_claude already proved a descriptor resolves");
            // The route decides where the credential comes from; the read is
            // the one place a secret enters this launch.
            let source = route
                .upstreams
                .first()
                .map(|upstream| upstream.credential.clone())
                .ok_or(BridgeError::AnthropicCompatCredentials)?;
            let secret = crate::route_plan::read_credential(&source, store)
                .map_err(|_| BridgeError::AnthropicCompatCredentials)?
                .ok_or(BridgeError::AnthropicCompatCredentials)?;
            refuse_blocked_free_model(descriptor, plan, &secret)?;
            crate::route_plan::render_direct_env(&route, &secret).apply(&mut env);
            apply_upstream_routing(&mut env, descriptor, route.upstream_routing.as_ref())?;
            // #1257: discovery is off under a pin, so say why once instead of
            // leaving the picker silently short of gateway rows.
            let mut notices = Vec::new();
            if !plan.allowed_models.is_empty() && descriptor.enable_gateway_model_discovery {
                notices.push(format!(
                    "[clud] gateway model discovery is off for this launch; models are pinned to: {}",
                    plan.allowed_models.join(", ")
                ));
            }
            // #1270: clud cannot monitor a direct request. Report the
            // effective per-request value, including any ambient override,
            // without claiming this is a stream-idle watchdog.
            let effective_timeout = env
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("API_TIMEOUT_MS"))
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| DIRECT_API_TIMEOUT_MS.to_string());
            notices.push(format!(
                "[clud] direct {} route: Claude Code API timeout {} ms (set API_TIMEOUT_MS to override); clud does not monitor this request",
                descriptor.display_name, effective_timeout
            ));
            (None, declared_hooks_settings(plan)?, notices)
        } else {
            (None, declared_hooks_settings(plan)?, Vec::new())
        };
        // Route-independent: a vision-less model warns on every launch shape
        // that can reach it, direct or through the unified gateway (#1200).
        if let Some(notice) = image_capability_notice(plan) {
            startup_notices.push(notice);
        }
        // #967 Phase 2b: tell the hook binary that compiled dispatcher lines
        // are registered for this session, so the bare `clud-cmd-scan` line
        // stops running declared hooks itself and each one fires exactly once.
        if claude_settings.is_some() && declared_hook_fragment(plan).is_some() {
            env.push((
                crate::clud_hooks_compile::DISPATCH_ENV.to_string(),
                "1".to_string(),
            ));
        }
        // #1984: this launch registers the prompt hook that records worktree
        // consent, so its hooks enforce it. Sessions clud did not launch this
        // way never see the marker and are not gated.
        // A nested launch decides afresh, like the unsafe marker.
        env.retain(|(key, _)| key != crate::worktree_consent::GATE_ENV);
        if plan.effective_harness().settings_surface().accepts_hooks() && !plan.unsafe_mode {
            env.push((
                crate::worktree_consent::GATE_ENV.to_string(),
                "1".to_string(),
            ));
        }
        // #967 Phase 3b: carry roots the hook cannot rediscover -- `--add-dir`
        // targets and `permissions.additionalDirectories` appear in no hook
        // payload.
        if let Some(entry) = hook_roots_env_value(plan) {
            env.push(entry);
        }
        set_env(
            &mut env,
            crate::loop_plan_mode::ALLOW_ENV,
            if plan.allow_plan_mode { "1" } else { "0" },
        );
        apply_route_context(&mut env, plan);
        Ok(Self {
            env,
            bridge,
            claude_settings,
            startup_notices,
        })
    }

    /// [`Self::start`] plus a chained Claude `statusLine` that shows clud's
    /// toasts (#1189). `None` behaves exactly like `start`. The `bool` is
    /// whether the status line was actually composed into the launch (#2395).
    pub fn start_with_statusline(
        plan: &LaunchPlan,
        env: Vec<(String, String)>,
        statusline: Option<&crate::toast::launch::StatuslineInjection>,
        status_writer: Option<&std::sync::Arc<crate::toast::statusline::StatusStateWriter>>,
    ) -> Result<(Self, bool), BridgeError> {
        let mut runtime = Self::start(plan, env)?;
        if let (Some(bridge), Some(writer)) = (runtime.bridge.as_ref(), status_writer) {
            bridge.set_status_usage_writer(std::sync::Arc::clone(writer));
        }
        let mut injected = false;
        if let Some(injection) = statusline {
            let home = crate::home::user_home();
            injected = runtime.inject_statusline(plan, injection, home.as_deref())?;
        }
        Ok((runtime, injected))
    }

    /// Compose clud's `statusLine` into this launch's Claude settings (#1189).
    ///
    /// The user's effective status line (explicit `--settings`, then project,
    /// then `~/.claude`) is chained, not replaced: `clud statusline` runs it
    /// first and appends the toast. The setting reaches Claude through the
    /// same single launch-scoped `--settings` source hooks use — merged into
    /// that file when one exists, or into the user's own `--settings`
    /// document, which then replaces the user's argument.
    ///
    /// This is a deliberate exception to "a repo that has not opted in sees an
    /// identical launch": with toasts enabled every Claude launch carries
    /// `--settings`. `[foreground.toasts] claude_statusline = false` restores
    /// the old argv (DD-071).
    pub(crate) fn inject_statusline(
        &mut self,
        plan: &LaunchPlan,
        injection: &crate::toast::launch::StatuslineInjection,
        home: Option<&Path>,
    ) -> Result<bool, BridgeError> {
        use crate::toast::statusline;

        if plan.effective_harness() != Backend::Claude {
            return Ok(false);
        }
        let project_dir = plan
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut injected = false;
        let compose = |document: &mut serde_json::Value| -> bool {
            let user = statusline::discover_user_statusline(Some(&*document), &project_dir, home);
            let chain = statusline::chained_command(user.as_ref());
            let Some(command) = statusline::statusline_command(
                &injection.exe,
                injection.session_pid,
                &injection.state_dir,
                chain.as_deref(),
                crate::platform::host::WINDOWS,
            ) else {
                return false;
            };
            let Some(object) = document.as_object_mut() else {
                return false;
            };
            object.insert(
                "statusLine".to_string(),
                statusline::compose_setting(user.as_ref(), command),
            );
            injected = true;
            true
        };
        self.merge_launch_settings(plan, compose)?;
        Ok(injected)
    }

    /// Settings work on Claude clients that predate the `--effort` flag.
    /// Unlike an environment override, this leaves interactive effort editable.
    fn apply_initial_effort(&mut self, plan: &LaunchPlan) -> Result<(), BridgeError> {
        if plan.effective_harness() != Backend::Claude {
            return Ok(());
        }
        let Some(effort) = settings_effort(&plan.command) else {
            return Ok(());
        };
        self.merge_launch_settings(plan, |document| {
            document["effortLevel"] = serde_json::json!(effort);
            true
        })
    }

    /// Apply the plan's commit/PR attribution choice (#1317) to this
    /// launch's Claude settings. clud hides Claude Code's attribution unless
    /// the launch opted in; keys the user's own `--settings` set are kept.
    pub(crate) fn apply_attribution(&mut self, plan: &LaunchPlan) -> Result<(), BridgeError> {
        if plan.effective_harness() != Backend::Claude {
            return Ok(());
        }
        let coauthor = plan.coauthor.clone();
        self.merge_launch_settings(plan, |document| {
            crate::attribution::merge_into(&coauthor, document)
        })
    }

    /// Register `clud session-hook` for this Claude-harness launch (#922), so
    /// the per-cwd session index records it under the route clud resolved.
    /// Appended next to the user's own hooks. Best effort: a launch never
    /// fails because its session could not be indexed.
    pub(crate) fn apply_session_history(&mut self, plan: &LaunchPlan) {
        if plan.effective_harness() != Backend::Claude {
            return;
        }
        let (Ok(exe), Ok(state_dir)) =
            (std::env::current_exe(), crate::daemon::default_state_dir())
        else {
            return;
        };
        let route = crate::session_history::hook::route_arg(
            plan.model_provider(),
            plan.routing_mode == crate::backend::RoutingMode::Unified,
        );
        let recovery = crate::session_history::launch::take_pending_recovery();
        let Some(fragment) = crate::session_history::hook::settings_fragment(
            &exe,
            &state_dir,
            route,
            recovery.as_deref(),
            crate::platform::host::WINDOWS,
        ) else {
            eprintln!(
                "[clud] note: session history hooks skipped: clud's path cannot be quoted safely"
            );
            return;
        };
        let merged = self.merge_launch_settings(plan, |document| {
            crate::clud_hooks_compile::merge_hook_settings(document, &fragment).is_ok()
        });
        if let Err(error) = merged {
            eprintln!("[clud] warning: session history hooks not registered: {error}");
        }
    }

    /// Run `compose` over this launch's single Claude `--settings` document:
    /// the launch-scoped file when one exists, otherwise the user's own
    /// `--settings` (or an empty document), written to a new launch-scoped
    /// file that replaces the user's argument. Nothing is written when
    /// `compose` reports no change.
    fn merge_launch_settings(
        &mut self,
        plan: &LaunchPlan,
        compose: impl FnOnce(&mut serde_json::Value) -> bool,
    ) -> Result<(), BridgeError> {
        if let Some(existing) = &self.claude_settings {
            let text = std::fs::read_to_string(&existing.value).map_err(|error| {
                BridgeError::Settings(format!(
                    "failed to read launch-scoped Claude settings: {error}"
                ))
            })?;
            let mut document: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
                BridgeError::Settings(format!(
                    "failed to parse launch-scoped Claude settings: {error}"
                ))
            })?;
            if compose(&mut document) {
                std::fs::write(&existing.value, document.to_string()).map_err(|error| {
                    BridgeError::Settings(format!(
                        "failed to write launch-scoped Claude settings: {error}"
                    ))
                })?;
            }
            return Ok(());
        }

        let (mut document, replaces_user_argument) = match user_settings_argument(&plan.command)? {
            Some(argument) => (read_user_settings(argument, plan.cwd.as_deref())?, true),
            None => (serde_json::json!({}), false),
        };
        if compose(&mut document) {
            self.claude_settings = Some(write_launch_scoped_settings(
                document,
                replaces_user_argument,
            )?);
        }
        Ok(())
    }

    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    #[cfg(test)]
    pub fn has_bridge(&self) -> bool {
        self.bridge.is_some()
    }

    /// How many turns the harness asked this launch's bridge to serve, or
    /// `None` when the launch is not bridge-routed. Feeds
    /// [`crate::launch_log::silent_bridge_reason`] (#998).
    pub fn bridge_turn_requests(&self) -> Option<usize> {
        self.bridge.as_ref().map(BridgeHandle::turn_requests)
    }

    #[cfg(test)]
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        self.bridge.as_ref().map(BridgeHandle::socket_addr)
    }

    #[cfg(test)]
    pub fn base_url(&self) -> Option<&str> {
        self.bridge.as_ref().map(BridgeHandle::base_url)
    }

    #[cfg(test)]
    pub fn bearer_token(&self) -> Option<&str> {
        self.bridge.as_ref().map(BridgeHandle::bearer_token)
    }

    pub fn spawn_with<Output, Adapter: SpawnAdapter<Output>>(
        &self,
        adapter: &Adapter,
        mode: SpawnMode,
        mut command: Vec<String>,
        cwd: Option<String>,
    ) -> Result<Output, Adapter::Error> {
        if let Some(settings) = &self.claude_settings {
            // Claude accepts a single --settings source. Compose an explicit
            // user source during startup, then replace it here so neither the
            // user's settings nor the lifecycle hooks can shadow the other.
            if settings.replaces_user_argument {
                remove_user_settings_argument(&mut command);
            }
            remove_settings_effort(&mut command);
            command.insert(1, settings.value.clone());
            command.insert(1, "--settings".to_string());
        }
        adapter.spawn(mode, command, cwd, self.env.clone())
    }

    pub fn spawn_subprocess(
        &self,
        command: Vec<String>,
        cwd: Option<PathBuf>,
        capture_stdout: bool,
        creation_flags: Option<u32>,
        dashboard_owns_stdin: bool,
    ) -> Result<ManagedSubprocess, String> {
        let adapter = NativeSubprocessAdapter {
            capture_stdout,
            creation_flags,
            dashboard_owns_stdin,
        };
        self.spawn_with(
            &adapter,
            SpawnMode::Subprocess,
            command,
            cwd.map(|path| path.to_string_lossy().into_owned()),
        )
    }

    pub fn spawn_pty(
        &self,
        command: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    ) -> Result<NativePtyProcess, running_process::pty::PtyError> {
        let adapter = NativePtyAdapter { rows, cols };
        self.spawn_with(&adapter, SpawnMode::Pty, command, cwd)
    }
}

/// Interactive-only admission for the direct Codex-via-Claude bridge. Daemon
/// admission and worker startup deliberately keep using [`ForegroundRuntime::preflight`]:
/// an answer to this selector is an explicit foreground user choice, never a
/// daemon-side credential fallback.
pub fn admit_codex_bridge_with_cli_import(
    plan: &LaunchPlan,
    interactive: bool,
) -> Result<(), BridgeError> {
    if !is_codex_via_claude(plan) || is_unified(plan) {
        return Ok(());
    }
    let preflight = crate::codex_upstream::ResolvedCredentials::preflight_default();
    match preflight {
        Ok(()) => Ok(()),
        Err(error) if should_offer_codex_cli_login_import(error, interactive) => {
            import_codex_cli_login(error)?;
            ForegroundRuntime::preflight(plan)
        }
        Err(error) => Err(BridgeError::CodexBridgeCredentials(error)),
    }
}

fn should_offer_codex_cli_login_import(
    error: crate::codex_upstream::CodexBridgeCredentialError,
    interactive: bool,
) -> bool {
    interactive
        && matches!(
            error,
            crate::codex_upstream::CodexBridgeCredentialError::Missing
                | crate::codex_upstream::CodexBridgeCredentialError::RefreshRejected
        )
}

fn codex_bridge_credential_failure(
    error: crate::codex_upstream::CodexBridgeCredentialError,
) -> BridgeError {
    BridgeError::CodexBridgeCredentials(error)
}

fn import_codex_cli_login(
    failure: crate::codex_upstream::CodexBridgeCredentialError,
) -> Result<(), BridgeError> {
    let home = crate::clud_settings::home_dir_path()
        .map_err(|_| codex_bridge_credential_failure(failure))?;
    let preference = crate::clud_settings::load_codex_cli_login_import_at(&home)
        .map_err(|_| codex_bridge_credential_failure(failure))?;
    if preference == Some(CodexCliLoginImport::Never) {
        return Err(codex_bridge_credential_failure(failure));
    }
    let credentials = crate::codex_upstream::CodexCliCredentials::from_codex_home()
        .map_err(|_| codex_bridge_credential_failure(failure))?
        .subscription_record();
    let current =
        crate::codex_auth::load_at(&home).map_err(|_| codex_bridge_credential_failure(failure))?;
    let mode = codex_cli_import_mode(current.as_ref(), &credentials);
    let choice = match preference {
        Some(CodexCliLoginImport::Always) if mode == CodexCliImportMode::Initial => {
            CodexCliImportChoice::Import
        }
        Some(CodexCliLoginImport::Never) => unreachable!("handled above"),
        Some(CodexCliLoginImport::Always) | None => {
            let mut selector = if mode == CodexCliImportMode::Initial {
                CodexCliImportSelector::new(credentials.email.clone())
            } else {
                CodexCliImportSelector::for_replacement(&credentials, mode)
            };
            selector::run(&mut std::io::stderr(), &mut selector)
                .unwrap_or(CodexCliImportChoice::NotNow)
        }
    };
    apply_codex_cli_import_choice_at(&home, current.as_ref(), &credentials, choice, failure)
}

fn apply_codex_cli_import_choice_at(
    home: &Path,
    expected: Option<&crate::codex_auth::SubscriptionCredentials>,
    credentials: &crate::codex_auth::SubscriptionCredentials,
    choice: CodexCliImportChoice,
    failure: crate::codex_upstream::CodexBridgeCredentialError,
) -> Result<(), BridgeError> {
    match choice {
        CodexCliImportChoice::Import => {
            match crate::codex_auth::replace_if_unchanged_at(home, expected, credentials) {
                Ok(true) => Ok(()),
                Ok(false) | Err(_) => Err(codex_bridge_credential_failure(failure)),
            }
        }
        CodexCliImportChoice::NotNow => Err(codex_bridge_credential_failure(failure)),
        CodexCliImportChoice::Never => {
            crate::clud_settings::save_codex_cli_login_import_at(home, CodexCliLoginImport::Never)
                .map_err(|_| codex_bridge_credential_failure(failure))?;
            Err(codex_bridge_credential_failure(failure))
        }
    }
}

// Unit routing tests intentionally construct bridge runtimes without a host
// credential. The compiled CLI regression exercises the production boundary
// with a real isolated process/home; keep its resolver separate from those
// structural unit tests so they never accidentally consult developer state.
#[cfg(not(test))]
fn preflight_codex_bridge_credentials() -> Result<(), BridgeError> {
    crate::codex_upstream::ResolvedCredentials::preflight_default()
        .map_err(BridgeError::CodexBridgeCredentials)
}

#[cfg(test)]
fn preflight_codex_bridge_credentials() -> Result<(), BridgeError> {
    Ok(())
}

impl fmt::Debug for ForegroundRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForegroundRuntime")
            .field("bridge_active", &self.bridge.is_some())
            .field("environment_entries", &self.env.len())
            .finish()
    }
}

/// #1829: hand the dsh child clud's provider key, its private Node and, for
/// OpenRouter, the overlay its `--patch` argument names.
fn prepare_dsh_child(
    plan: &LaunchPlan,
    env: &mut Vec<(String, String)>,
) -> Result<(), BridgeError> {
    // The route decides the model, the key variable and the base URL; dsh
    // itself owns everything else about its provider configuration.
    let route = crate::route_plan::route_of(plan, &crate::route_plan::Ambient::from_child_env(env));
    let render = crate::route_plan::render_dsh(&route);
    let facts = crate::dsh_harness::ChildFacts {
        executable: plan.command.first().map(String::as_str).unwrap_or("dsh"),
        provider: render.provider,
        wire_model: render.wire_model.as_deref(),
        base_url: render.base_url.as_deref(),
    };
    let vault = |provider: ModelProvider| {
        use crate::provider_auth::SecretStore as _;
        let descriptor = crate::provider_registry::descriptor_for(provider)?;
        crate::provider_auth::NativeSecretStore::new_for(
            descriptor.vault_service,
            descriptor.vault_account,
        )
        .ok()?
        .get()
        .ok()
        .flatten()
    };
    crate::dsh_harness::prepare_child(
        &facts,
        crate::dsh_harness::managed_home().as_deref(),
        env,
        &|name| std::env::var(name).ok(),
        &vault,
    )
    .map_err(|error| BridgeError::Settings(format!("DeepSeek Harness setup failed: {error}")))
}

pub fn with_foreground_runtime<ResultValue>(
    plan: &LaunchPlan,
    env: Vec<(String, String)>,
    run: impl FnOnce(&ForegroundRuntime) -> ResultValue,
) -> Result<ResultValue, BridgeError> {
    let runtime = ForegroundRuntime::start(plan, env)?;
    Ok(run(&runtime))
}

/// Route, harness, provider and wire model for the launch-context record
/// (#1675), classified by the same predicates `start_with_secret_store`
/// branches on. `--dry-run` previews the record from this too.
pub fn launch_context_plan_facts(plan: &LaunchPlan) -> crate::launch_context::PlanFacts {
    use crate::launch_context::Route;
    let selected = plan
        .model_selection
        .as_ref()
        .and_then(|selection| selection.wire_model.clone());
    let (route, wire_model) = if is_unified(plan) {
        (Route::UnifiedGateway, selected)
    } else if is_codex_via_claude(plan) {
        (Route::CodexBridge, selected)
    } else if is_anthropic_compat_via_claude(plan) {
        let model = direct_wire_model(plan.model_provider(), plan.model_selection.as_ref());
        (Route::Direct, model.map(str::to_string))
    } else {
        (Route::Native, selected)
    };
    crate::launch_context::PlanFacts {
        route,
        harness: plan
            .effective_harness()
            .as_model_provider()
            .as_str()
            .to_string(),
        provider: plan.model_provider().as_str().to_string(),
        wire_model,
    }
}

/// The wire model the direct overlay launches: the selection's, else the
/// provider's reviewed catalog default.
/// `--provider-only`: hand OpenRouter's `provider` routing object to Claude
/// Code through `CLAUDE_CODE_EXTRA_BODY`, keeping any other keys already set.
fn apply_upstream_routing(
    env: &mut Vec<(String, String)>,
    descriptor: &crate::provider_registry::AnthropicCompatProvider,
    routing: Option<&crate::route_plan::ProviderRouting>,
) -> Result<(), BridgeError> {
    let Some(routing) = routing.filter(|_| descriptor.provider == ModelProvider::OpenRouter) else {
        return Ok(());
    };
    let key = crate::openrouter_routing::EXTRA_BODY_ENV;
    let existing = env
        .iter()
        .find(|(candidate, _)| env_key_eq(candidate, key))
        .map(|(_, value)| value.clone());
    let body = crate::openrouter_routing::policy_body(existing.as_deref(), routing)
        .map_err(BridgeError::Model)?;
    set_env(env, key, &body);
    Ok(())
}

/// #1833: a free OpenRouter id whose every endpoint the workspace guardrail
/// excludes would fail on the first turn with a bare 400. Probe it once, only
/// for an id the catalog proved free (so the probe never bills), and refuse
/// the launch with the fix instead.
fn refuse_blocked_free_model(
    descriptor: &crate::provider_registry::AnthropicCompatProvider,
    plan: &LaunchPlan,
    secret: &str,
) -> Result<(), BridgeError> {
    if descriptor.provider != ModelProvider::OpenRouter {
        return Ok(());
    }
    let Some(wire) = plan
        .model_selection
        .as_ref()
        .and_then(|selection| selection.wire_model.as_deref())
    else {
        return Ok(());
    };
    let catalog = crate::openrouter_catalog::catalog_cached_or_embedded();
    if crate::openrouter_free::check(wire, &catalog) != crate::openrouter_free::FreeCheck::Free {
        return Ok(());
    }
    match crate::openrouter_free::probe(wire, secret, &catalog) {
        crate::openrouter_free::ProbeVerdict::Blocked(message) => Err(BridgeError::Model(message)),
        crate::openrouter_free::ProbeVerdict::Proceed => Ok(()),
    }
}

fn direct_wire_model(
    provider: ModelProvider,
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
) -> Option<&str> {
    selection
        .and_then(|selection| selection.wire_model.as_deref())
        .or_else(|| {
            crate::provider_catalog::reviewed_default_model(provider).map(|row| row.wire_id)
        })
}

fn is_codex_via_claude(plan: &LaunchPlan) -> bool {
    plan.model_provider() == ModelProvider::Codex && plan.effective_harness() == Backend::Claude
}

/// True when the plan routes a descriptor-backed Anthropic-compatible
/// provider (DeepSeek today, Kimi in #937 Phase 3) directly through the
/// Claude harness -- as opposed to Claude native, the Codex translation
/// bridge, or unified-gateway routing.
fn is_anthropic_compat_via_claude(plan: &LaunchPlan) -> bool {
    plan.effective_harness() == Backend::Claude
        && crate::provider_registry::descriptor_for(plan.model_provider()).is_some()
}

fn is_unified(plan: &LaunchPlan) -> bool {
    plan.routing_mode == RoutingMode::Unified && plan.effective_harness() == Backend::Claude
}

fn integration_has_kimi(
    upstreams: &Option<crate::codex_bridge::UnifiedIntegrationUpstreams>,
) -> bool {
    upstreams
        .as_ref()
        .is_some_and(crate::codex_bridge::UnifiedIntegrationUpstreams::has_kimi)
}

fn unified_startup_notices(
    codex_available: bool,
    deepseek_available: bool,
    kimi_available: bool,
) -> Vec<String> {
    let mut notices = Vec::new();
    if !codex_available {
        notices.push(
            "[clud] unified gateway: Codex models unavailable; set OPENAI_API_KEY or run `clud auth login codex`"
                .to_string(),
        );
    }
    if !deepseek_available {
        notices.push(
            "[clud] unified gateway: DeepSeek models unavailable; run `clud auth login deepseek`"
                .to_string(),
        );
    }
    if !kimi_available {
        notices.push(
            "[clud] unified gateway: Kimi models unavailable; run `clud auth login kimi`"
                .to_string(),
        );
    }
    notices
}

/// Machine-readable launch identity for bundled skills. This is child-local,
/// route-owned data: a user-provided value never decides which models a skill
/// is allowed to delegate to.
const ROUTE_CONTEXT_ENV: &str = "CLUD_ROUTE_CONTEXT";

/// The catalog row a launch will actually bill, when clud knows it.
///
/// The selection's `model` (a catalog CLI ID) is checked first, because the
/// wire spelling is not always the row's `wire_id`: an auto-context DeepSeek
/// selection is billed as `deepseek-v4-pro` while that row's `wire_id` carries
/// the `[1m]` suffix, so a wire-only lookup would miss it (#1200 review).
///
/// A selection that names no catalog row yields `None` -- clud has no
/// capability data for a model it does not know, and a model-less selection
/// (one that pins only effort) is no exception. Only a plan with *no*
/// selection at all falls through to the descriptor's reviewed default, which
/// is the row `apply_anthropic_compat_overlay` bills in that case.
fn billed_catalog_row(plan: &LaunchPlan) -> Option<crate::provider_catalog::CatalogModel> {
    if let Some(selection) = plan.model_selection.as_ref() {
        return selection
            .model
            .as_deref()
            .and_then(crate::provider_catalog::model_by_cli_id)
            .or_else(|| {
                selection
                    .wire_model
                    .as_deref()
                    .and_then(crate::provider_catalog::model_by_wire_id)
            });
    }
    crate::provider_registry::descriptor_for(plan.model_provider())
        .and_then(|descriptor| crate::provider_catalog::reviewed_default_model(descriptor.provider))
}

/// The startup line for an *inherited* pin (#1257): with no `--model` on the
/// command line the launch pinned every slot to the previous model selection,
/// so a cost boundary exists that the user did not type. Reported exactly in
/// that case -- an explicit `--model`/`--allow-model` needs no announcement.
///
/// Green only on a TTY, the same rule as the other `[clud]` launch notices;
/// plain text otherwise so logs and CI stay machine-readable.
fn model_pin_notices(plan: &LaunchPlan, color: bool) -> Vec<String> {
    if !plan.pinned_from_previous_selection || plan.allowed_models.is_empty() {
        return Vec::new();
    }
    let text = format!(
        "[clud] info: no --model given; pinned to previous model selection: {}",
        plan.allowed_models.join(", ")
    );
    vec![if color {
        format!("\x1b[32m{text}\x1b[0m")
    } else {
        text
    }]
}

/// The image-capability warning for a launch whose model cannot accept images,
/// or `None` when there is nothing to warn about (#1200).
///
/// Silent, not loud: DeepSeek's endpoint replaces an image block with a
/// literal `[Unsupported Image]` placeholder and still answers `200`, so the
/// user sees a model that claims it cannot see the picture and no failure
/// anywhere. Naming the offending model is the whole point -- the harness
/// reports the reply, and only this line connects it to the model choice.
fn image_capability_notice(plan: &LaunchPlan) -> Option<String> {
    // Only a Claude-harness launch routes through an Anthropic-compat overlay
    // or the unified gateway. A native harness (`--harness deepseek`) owns its
    // own provider configuration and never reads this catalog, so telling that
    // session about a dropped image would describe a request clud does not
    // send. The docs scope the warning the same way.
    if plan.effective_harness() != Backend::Claude {
        return None;
    }
    let entry = billed_catalog_row(plan)?;
    if entry.supports_images {
        return None;
    }
    // The same invariant `apply_anthropic_compat_overlay` asserts: a
    // descriptor-backed provider has a reviewed default to name. A row marked
    // as dropping images is unpublishable without one, and
    // `deepseek_pro_is_the_only_row_verified_to_drop_images` pins that.
    let alternative = crate::provider_catalog::reviewed_default_model(entry.provider)
        .expect(
            "a row marked as dropping images must belong to a provider with a reviewed default \
             to name as the alternative -- add a `provider_default: true` row in provider_catalog.rs",
        )
        .cli_id;
    Some(format!(
        "[clud] {} cannot accept images: pasted screenshots are dropped upstream with no error. \
         Use `--model {alternative}` for image work.",
        entry.cli_id
    ))
}

/// Maps an OpenRouter published capability to Claude Code's own capability
/// vocabulary (#1528). `Some("<wire>=per_turn_effort")` is the value the
/// harness expects in `CLAUDE_CODE_MODEL_CAPABILITIES`; `None` means inject
/// nothing, which is also the answer for a row OpenRouter never tagged with
/// `reasoning_effort` and for the shipped-dark flip.
///
/// Pure and catalog-injected so a test can drive any row and either flip state
/// without a release, a network read, or a live endpoint.
pub(crate) fn per_turn_effort_capability(
    catalog: &crate::openrouter_catalog::Catalog,
    wire_model: &str,
    enabled: bool,
) -> Option<String> {
    if !enabled {
        return None;
    }
    let row = catalog.model_by_id(wire_model)?;
    row.supports_reasoning_effort
        .then(|| format!("{wire_model}=per_turn_effort"))
}

fn codex_selection_from_plan(plan: &LaunchPlan) -> Result<Option<ModelSpec>, BridgeError> {
    if let Some(selection) = plan
        .model_selection
        .as_ref()
        .filter(|selection| selection.provider == ModelProvider::Codex)
    {
        if let Some(model) = selection.wire_model.clone() {
            return Ok(Some(ModelSpec {
                model,
                effort: selection.effort,
            }));
        }
    }
    plan.codex_model
        .as_deref()
        .map(ModelSpec::parse)
        .transpose()
        .map_err(|error| BridgeError::Model(error.to_string()))
}

fn apply_route_context(env: &mut Vec<(String, String)>, plan: &LaunchPlan) {
    let codex_via_claude = is_codex_via_claude(plan);
    let context = serde_json::json!({
        "version": 1,
        "model_provider": plan.model_provider().as_str(),
        "harness": plan.effective_harness().as_model_provider().as_str(),
        "routing_mode": plan.routing_mode.as_str(),
        "delegation": if codex_via_claude {
            serde_json::json!({
                "cost_policy": "workers_use_sonnet; reserve_opus_for_planning_review_and_integration",
                "roles": {
                    "planner": "opus",
                    "reviewer": "opus",
                    "integrator": "opus",
                    "worker": "sonnet"
                },
                "resolved_models": {
                    "opus": CODEX_VIA_CLAUDE_OPUS_MODEL,
                    "sonnet": CODEX_VIA_CLAUDE_SONNET_MODEL
                }
            })
        } else {
            serde_json::json!({
                "cost_policy": "prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed",
                "roles": "use_harness_native_model_selection"
            })
        }
    });
    set_env(env, ROUTE_CONTEXT_ENV, &context.to_string());
}

fn set_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    env.retain(|(candidate, _)| !env_key_eq(candidate, key));
    env.push((key.to_string(), value.to_string()));
}

fn context_lifecycle_settings(
    bridge: &BridgeHandle,
    header_name: &str,
    header_value: &str,
    allowed_env: &str,
) -> serde_json::Value {
    let compact_url = format!("{}/_clud/context/compact", bridge.base_url());
    let compact_finished_url = format!("{}/_clud/context/compact-finished", bridge.base_url());
    let clear_url = format!("{}/_clud/context/clear", bridge.base_url());
    let hook = |url: String| {
        let mut headers = serde_json::Map::new();
        headers.insert(
            header_name.to_string(),
            serde_json::Value::String(header_value.to_string()),
        );
        serde_json::json!({
            "type": "http",
            "url": url,
            "headers": headers,
            "allowedEnvVars": [allowed_env]
        })
    };
    serde_json::json!({
        "hooks": {
            "PreCompact": [{
                "matcher": "manual|auto",
                "hooks": [hook(compact_url)]
            }],
            "SessionStart": [
                {
                    "matcher": "clear",
                    "hooks": [hook(clear_url)]
                },
                {
                    "matcher": "compact",
                    "hooks": [hook(compact_finished_url)]
                }
            ]
        }
    })
}

fn merged_context_lifecycle_settings(
    plan: &LaunchPlan,
    bridge: &BridgeHandle,
) -> Result<ClaudeSettings, BridgeError> {
    merged_context_lifecycle_settings_with(
        plan,
        bridge,
        "Authorization",
        "Bearer $ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_AUTH_TOKEN",
    )
}

fn merged_unified_context_lifecycle_settings(
    plan: &LaunchPlan,
    bridge: &BridgeHandle,
) -> Result<ClaudeSettings, BridgeError> {
    merged_context_lifecycle_settings_with(
        plan,
        bridge,
        UNIFIED_GATEWAY_TOKEN_HEADER,
        "$CLUD_GATEWAY_TOKEN",
        "CLUD_GATEWAY_TOKEN",
    )
}

fn merged_context_lifecycle_settings_with(
    plan: &LaunchPlan,
    bridge: &BridgeHandle,
    header_name: &str,
    header_value: &str,
    allowed_env: &str,
) -> Result<ClaudeSettings, BridgeError> {
    let mut settings = context_lifecycle_settings(bridge, header_name, header_value, allowed_env);
    if let Some(fragment) = declared_hook_fragment(plan) {
        crate::clud_hooks_compile::merge_hook_settings(&mut settings, &fragment)
            .map_err(BridgeError::Settings)?;
    }
    compose_launch_settings(plan, settings)
}

/// Directories the user granted this session that no hook payload mentions.
///
/// `--add-dir` reaches the harness as passthrough argv, and
/// `permissions.additionalDirectories` lives in a settings file the hook has
/// no reason to read. Both widen what the session may touch, so containment
/// has to know about them — and the only place that knows is the launch.
fn harvested_roots(plan: &LaunchPlan, repo_root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = add_dir_arguments(&plan.command)
        .into_iter()
        .map(|raw| resolve_against_cwd(&raw, plan.cwd.as_deref()))
        .collect();
    found.extend(additional_directories(repo_root));
    found
}

/// Every `--add-dir` value in `command`, in both spellings.
///
/// The flag takes one or more directories, so values are consumed until the
/// next flag. Scanning stops at `--`, past which the tokens belong to whatever
/// the user is invoking rather than to the harness.
fn add_dir_arguments(command: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    let mut index = 1;
    while index < command.len() {
        let argument = &command[index];
        if argument == "--" {
            break;
        }
        if let Some(value) = argument.strip_prefix("--add-dir=") {
            if !value.is_empty() {
                found.push(value.to_string());
            }
            index += 1;
            continue;
        }
        if argument == "--add-dir" {
            index += 1;
            while index < command.len() {
                let value = &command[index];
                if value.starts_with('-') || value == "--" {
                    break;
                }
                found.push(value.clone());
                index += 1;
            }
            continue;
        }
        index += 1;
    }
    found
}

/// `permissions.additionalDirectories` from the repo's own Claude settings.
///
/// Read directly rather than through `hook_health`, which parses only hook
/// entries. Both the shared and the gitignored local file count, since either
/// can widen the session.
fn additional_directories(repo_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for name in ["settings.json", "settings.local.json"] {
        let path = repo_root.join(".claude").join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let entries = document
            .get("permissions")
            .and_then(|permissions| permissions.get("additionalDirectories"))
            .and_then(serde_json::Value::as_array);
        for entry in entries.into_iter().flatten() {
            if let Some(raw) = entry.as_str().map(str::trim).filter(|raw| !raw.is_empty()) {
                found.push(resolve_against(repo_root, raw));
            }
        }
    }
    found
}

fn resolve_against_cwd(raw: &str, cwd: Option<&str>) -> PathBuf {
    let base = cwd.map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        PathBuf::from,
    );
    resolve_against(&base, raw)
}

fn resolve_against(base: &Path, raw: &str) -> PathBuf {
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    }
}

/// The registry this session's hooks should see, encoded for the child.
///
/// Harvested directories are registered as `extern`: the parent's own
/// project guards have no more business running against a granted sibling
/// directory than against a checkout clud cloned, and misfiring there is the
/// #841 ENOENT class. They differ from `.extern-repos/` clones in *trust* —
/// the user named these at launch — which Phase 4 has to distinguish when it
/// gates running a foreign repo's own hooks.
fn hook_roots_env_value(plan: &LaunchPlan) -> Option<(String, String)> {
    let cwd = plan
        .cwd
        .as_deref()
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let repo_root = crate::block_bad_cmd::nearest_repo_root_public(&cwd)?;
    let harvested = harvested_roots(plan, &repo_root);
    if harvested.is_empty() {
        // Nothing the hook cannot work out for itself; leave the env alone.
        return None;
    }
    let encoded: Vec<serde_json::Value> = harvested
        .iter()
        .map(|path| {
            serde_json::json!({
                "kind": crate::clud_hook_roots::RootKind::Extern.as_str(),
                "path": path.to_string_lossy(),
            })
        })
        .collect();
    Some((
        crate::clud_hook_roots::HOOK_ROOTS_ENV.to_string(),
        serde_json::Value::Array(encoded).to_string(),
    ))
}

/// Compose `--settings` for a launch with no bridge, when the repo declares
/// hooks and the frontend can accept them.
///
/// Claude only: codex has no argument surface for hooks — `-c` overrides
/// `config.toml` values and hooks live in a separate `hooks.json` with no
/// flag pointing at an alternate one. Codex keeps the PreToolUse coverage its
/// already-installed `clud-cmd-scan` line gives it.
fn declared_hooks_settings(plan: &LaunchPlan) -> Result<Option<ClaudeSettings>, BridgeError> {
    if !plan.effective_harness().settings_surface().accepts_hooks() {
        return Ok(None);
    }
    let mut fragment = serde_json::json!({"hooks": {}});
    if let Some(declared) = declared_hook_fragment(plan) {
        crate::clud_hooks_compile::merge_hook_settings(&mut fragment, &declared)
            .map_err(BridgeError::Settings)?;
    }
    compose_launch_settings(plan, fragment).map(Some)
}

/// The registration for whatever the repo declares in `.clud/hooks.json`, or
/// `None` when it declares nothing (#967 Phase 2b).
///
/// `None` is the signal to leave the launch alone entirely: a repo that has
/// not opted in should see the argv it saw before this feature existed.
fn declared_hook_fragment(plan: &LaunchPlan) -> Option<serde_json::Value> {
    let cwd = plan
        .cwd
        .as_deref()
        .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from);
    let repo_root = crate::block_bad_cmd::nearest_repo_root_public(&cwd)?;
    let hooks = crate::clud_hooks::discover(&repo_root)?;
    // Phase 5: clud's own `CwdChanged` backstop line rides on frontend
    // support, probed once per launch against the resolved backend binary.
    // Only a harness whose settings surface accepts hooks consumes the
    // fragment (the bridge wraps Claude; see `SettingsSurface`), so probe only
    // there — a failed probe degrades silently to no line (DD-064).
    let cwd_changed_supported = plan.effective_harness().settings_surface().accepts_hooks()
        && plan
            .command
            .first()
            .map(|binary| {
                crate::backend_bootstrap::probe_claude_cwd_changed_support(binary.as_ref())
            })
            .unwrap_or(false);
    crate::clud_hooks_compile::claude_settings_fragment(&hooks, cwd_changed_supported)
}

/// Whether this launch's client takes clud's `WorktreeCreate` hook: a harness
/// whose settings surface accepts hooks, on Claude Code 2.1.50 or newer.
fn claude_takes_worktree_hooks(plan: &LaunchPlan) -> bool {
    // Unit tests never probe whatever `claude` the machine has installed.
    !cfg!(test)
        && plan.effective_harness().settings_surface().accepts_hooks()
        && plan.command.first().is_some_and(|binary| {
            crate::backend_bootstrap::probe_claude_worktree_hooks_support(binary.as_ref())
        })
}

/// Hand `generated` to Claude as a launch-scoped `--settings` source.
///
/// When the user supplied their own `--settings`, clud merges into *their*
/// document and replaces the argument, because Claude accepts only one such
/// source and whichever came second would otherwise shadow the other.
fn compose_launch_settings(
    plan: &LaunchPlan,
    settings: serde_json::Value,
) -> Result<ClaudeSettings, BridgeError> {
    let (document, replaces_user_argument) = compose_launch_settings_document(plan, settings)?;
    write_launch_scoped_settings(document, replaces_user_argument)
}

fn compose_launch_settings_document(
    plan: &LaunchPlan,
    settings: serde_json::Value,
) -> Result<(serde_json::Value, bool), BridgeError> {
    let (mut settings, replaces) =
        compose_launch_settings_document_with(plan, settings, claude_takes_worktree_hooks(plan))?;
    crate::clud_hooks_compile::consolidate_scanner_hooks(&mut settings);
    let cwd = plan
        .cwd
        .as_deref()
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let repo = crate::block_bad_cmd::nearest_repo_root_public(&cwd).unwrap_or(cwd);
    let home = crate::hook_health::hook_home_dir();
    if !plan
        .command
        .iter()
        .any(|arg| arg == "--setting-sources" || arg.starts_with("--setting-sources="))
    {
        crate::scanner_hook_migration::remove_inherited(&mut settings, &repo, home.as_deref());
    }
    Ok((settings, replaces))
}

/// [`compose_launch_settings_document`] with the worktree-hook decision
/// made by the caller, so tests need not probe a real client.
fn compose_launch_settings_document_with(
    plan: &LaunchPlan,
    mut settings: serde_json::Value,
    worktree_hooks: bool,
) -> Result<(serde_json::Value, bool), BridgeError> {
    let safety = if plan.unsafe_mode {
        crate::clud_hooks_compile::settings_authority_fragment()
    } else {
        crate::clud_hooks_compile::deletion_safety_fragment()
    };
    crate::clud_hooks_compile::merge_hook_settings(&mut settings, &safety)
        .map_err(BridgeError::Settings)?;
    merge_deletion_denies(&mut settings, &safety)?;
    // #1962/#1971: build Claude Code's worktrees under `~/.clud/worktrees`.
    if worktree_hooks {
        let worktrees = crate::clud_hooks_compile::worktree_hooks_fragment();
        crate::clud_hooks_compile::merge_hook_settings(&mut settings, &worktrees)
            .map_err(BridgeError::Settings)?;
    }
    let Some(user_argument) = user_settings_argument(&plan.command)? else {
        return Ok((settings, false));
    };
    let mut user_settings = read_user_settings(user_argument, plan.cwd.as_deref())?;
    let user_root = user_settings.as_object_mut().ok_or_else(|| {
        BridgeError::Settings("Claude --settings must contain a JSON object".to_string())
    })?;
    let user_hooks = user_root
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            BridgeError::Settings("Claude --settings hooks must be a JSON object".to_string())
        })?;
    let mut settings = settings;
    let generated_hooks = settings["hooks"]
        .as_object_mut()
        .expect("generated hooks are an object");
    for (event, generated_entries) in generated_hooks {
        let user_entries = user_hooks
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                BridgeError::Settings(format!(
                    "Claude --settings hook event {event} must be a JSON array"
                ))
            })?;
        user_entries.extend(
            generated_entries
                .as_array()
                .expect("generated hook event is an array")
                .iter()
                .cloned(),
        );
    }
    merge_deletion_denies(&mut user_settings, &settings)?;
    Ok((user_settings, true))
}

/// Ordinary effort levels accepted by older Claude settings schemas.
fn settings_effort(command: &[String]) -> Option<&str> {
    command.windows(2).find_map(|pair| {
        (pair[0] == "--effort" && matches!(pair[1].as_str(), "low" | "medium" | "high" | "xhigh"))
            .then_some(pair[1].as_str())
    })
}

fn remove_settings_effort(command: &mut Vec<String>) {
    if settings_effort(command).is_some() {
        if let Some(index) = command.iter().position(|arg| arg == "--effort") {
            command.drain(index..index + 2);
        }
    }
}

/// Render launch settings and effort compatibility without creating a file.
pub fn dry_run_claude_command(plan: &LaunchPlan) -> Result<Vec<String>, String> {
    let (mut settings, _) = compose_launch_settings_document(plan, serde_json::json!({}))
        .map_err(|error| error.to_string())?;
    if let Some(effort) = settings_effort(&plan.command) {
        settings["effortLevel"] = serde_json::json!(effort);
    }
    let mut command = plan.command.clone();
    remove_settings_effort(&mut command);
    remove_user_settings_argument(&mut command);
    command.insert(1, settings.to_string());
    command.insert(1, "--settings".to_string());
    Ok(command)
}

fn merge_deletion_denies(
    base: &mut serde_json::Value,
    overlay: &serde_json::Value,
) -> Result<(), BridgeError> {
    let generated = overlay
        .pointer("/permissions/deny")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let root = base
        .as_object_mut()
        .ok_or_else(|| BridgeError::Settings("settings must be a JSON object".into()))?;
    let permissions = root
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| BridgeError::Settings("settings permissions must be an object".into()))?;
    let deny = permissions
        .entry("deny")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(|| {
            BridgeError::Settings("settings permissions.deny must be an array".into())
        })?;
    for value in generated {
        if !deny.contains(&value) {
            deny.push(value);
        }
    }
    Ok(())
}

fn write_launch_scoped_settings(
    mut settings: serde_json::Value,
    replaces_user_argument: bool,
) -> Result<ClaudeSettings, BridgeError> {
    crate::clud_hooks_compile::consolidate_scanner_hooks(&mut settings);
    let mut file = tempfile::Builder::new()
        .prefix("clud-claude-settings-")
        .suffix(".json")
        .tempfile()
        .map_err(|error| {
            BridgeError::Settings(format!(
                "failed to create launch-scoped Claude settings: {error}"
            ))
        })?;
    file.write_all(settings.to_string().as_bytes())
        .map_err(|error| {
            BridgeError::Settings(format!(
                "failed to write launch-scoped Claude settings: {error}"
            ))
        })?;
    let value = file.path().to_string_lossy().into_owned();
    Ok(ClaudeSettings {
        value,
        replaces_user_argument,
        _temp_file: file,
    })
}

fn user_settings_argument(command: &[String]) -> Result<Option<&str>, BridgeError> {
    let mut found = None;
    let mut index = 1;
    while index < command.len() {
        let argument = &command[index];
        if argument == "--" {
            break;
        }
        let value = if argument == "--settings" {
            index += 1;
            Some(
                command
                    .get(index)
                    .ok_or_else(|| {
                        BridgeError::Settings("Claude --settings is missing its value".to_string())
                    })?
                    .as_str(),
            )
        } else {
            argument.strip_prefix("--settings=")
        };
        if let Some(value) = value {
            if found.replace(value).is_some() {
                return Err(BridgeError::Settings(
                    "Claude --settings may only be supplied once".to_string(),
                ));
            }
        }
        index += 1;
    }
    Ok(found)
}

fn read_user_settings(argument: &str, cwd: Option<&str>) -> Result<serde_json::Value, BridgeError> {
    let contents = if argument.trim_start().starts_with('{') {
        argument.to_string()
    } else {
        let supplied = PathBuf::from(argument);
        let path = if supplied.is_absolute() {
            supplied
        } else {
            PathBuf::from(cwd.unwrap_or(".")).join(supplied)
        };
        std::fs::read_to_string(&path).map_err(|error| {
            BridgeError::Settings(format!(
                "failed to read Claude --settings file {}: {error}",
                path.display()
            ))
        })?
    };
    serde_json::from_str(&contents).map_err(|error| {
        BridgeError::Settings(format!("failed to parse Claude --settings JSON: {error}"))
    })
}

fn remove_user_settings_argument(command: &mut Vec<String>) {
    let mut index = 1;
    while index < command.len() {
        if command[index] == "--" {
            return;
        }
        if command[index] == "--settings" {
            command.remove(index);
            if index < command.len() {
                command.remove(index);
            }
            return;
        }
        if command[index].starts_with("--settings=") {
            command.remove(index);
            return;
        }
        index += 1;
    }
}

pub(crate) fn env_key_eq(left: &str, right: &str) -> bool {
    if crate::platform::host::WINDOWS {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

struct NativeSubprocessAdapter {
    dashboard_owns_stdin: bool,
    capture_stdout: bool,
    creation_flags: Option<u32>,
}

impl SpawnAdapter<ManagedSubprocess> for NativeSubprocessAdapter {
    type Error = String;

    fn spawn(
        &self,
        mode: SpawnMode,
        command: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<ManagedSubprocess, Self::Error> {
        debug_assert_eq!(mode, SpawnMode::Subprocess);
        let start = if self.dashboard_owns_stdin {
            ManagedSubprocess::start_without_stdin
        } else {
            ManagedSubprocess::start
        };
        start(
            command,
            cwd.map(PathBuf::from),
            env,
            self.capture_stdout,
            self.creation_flags,
        )
    }
}

struct NativePtyAdapter {
    rows: u16,
    cols: u16,
}

impl SpawnAdapter<NativePtyProcess> for NativePtyAdapter {
    type Error = running_process::pty::PtyError;

    fn spawn(
        &self,
        mode: SpawnMode,
        command: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<NativePtyProcess, Self::Error> {
        debug_assert_eq!(mode, SpawnMode::Pty);
        NativePtyProcess::new(command, cwd, Some(env), self.rows, self.cols, None)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::backend::{Backend, HarnessSelection, LaunchMode, ModelProvider, PreferenceSource};
    use crate::command::LaunchPlan;
    use crate::graphics::GraphicsConfig;
    use std::cell::RefCell;
    use std::net::TcpListener;

    /// Injectable fake so routing tests never touch the host's real native
    /// credential vault.
    struct FakeSecretStore(Option<String>);

    struct UnreadSecretStore;

    impl crate::provider_auth::SecretStore for UnreadSecretStore {
        fn get(&self) -> Result<Option<String>, crate::provider_auth::SecretStoreError> {
            panic!("a refused privacy policy must not read credentials")
        }
        fn set(&self, _secret: &str) -> Result<(), crate::provider_auth::SecretStoreError> {
            panic!("a refused privacy policy must not write credentials")
        }
        fn delete(&self) -> Result<(), crate::provider_auth::SecretStoreError> {
            panic!("a refused privacy policy must not delete credentials")
        }
    }

    #[test]
    fn unsupported_privacy_policies_refuse_before_credential_access() {
        use crate::backend::RoutingMode::{Direct, Unified};

        for (provider, harness, mode) in [
            (ModelProvider::Claude, Backend::Claude, Direct),
            (ModelProvider::Kimi, Backend::Claude, Direct),
            (ModelProvider::Claude, Backend::Claude, Unified),
            (ModelProvider::OpenRouter, Backend::DeepSeek, Direct),
        ] {
            for (zdr, no_data_collection) in [(true, false), (false, true)] {
                let mut plan = plan(provider, harness);
                plan.routing_mode = mode;
                plan.zdr = zdr;
                plan.no_data_collection = no_data_collection;
                let result = ForegroundRuntime::start_with_secret_store(
                    &plan,
                    Vec::new(),
                    &UnreadSecretStore,
                );
                assert!(
                    matches!(result, Err(BridgeError::Model(reason)) if reason.contains("OpenRouter"))
                );
            }
        }
    }

    impl crate::provider_auth::SecretStore for FakeSecretStore {
        fn get(&self) -> Result<Option<String>, crate::provider_auth::SecretStoreError> {
            Ok(self.0.clone())
        }
        fn set(&self, _secret: &str) -> Result<(), crate::provider_auth::SecretStoreError> {
            unreachable!("routing tests never write to the store")
        }
        fn delete(&self) -> Result<(), crate::provider_auth::SecretStoreError> {
            unreachable!("routing tests never write to the store")
        }
    }

    pub(crate) fn plan(provider: ModelProvider, harness: Backend) -> LaunchPlan {
        LaunchPlan {
            unsafe_mode: false,
            allow_plan_mode: false,
            command: vec![harness.executable_name().to_string()],
            iterations: 1,
            backend: harness,
            routing_mode: crate::backend::RoutingMode::Direct,
            model_provider: Some(provider),
            requested_harness: Some(match harness {
                Backend::Claude => HarnessSelection::Claude,
                Backend::Codex => HarnessSelection::Codex,
                Backend::DeepSeek => HarnessSelection::DeepSeek,
                Backend::OpenCode => HarnessSelection::OpenCode,
            }),
            effective_harness: Some(harness),
            provider_source: Some(PreferenceSource::Cli),
            harness_source: Some(PreferenceSource::Cli),
            launch_mode: LaunchMode::Subprocess,
            cwd: None,
            graphics: GraphicsConfig::default(),
            repeat_schedule: None,
            task_summary: None,
            loop_markers: None,
            stream_json_progress: false,
            prompt_summary: false,
            codex_model: None,
            model_selection: None,
            failover: None,
            failover_allow_metered: false,
            allowed_models: Vec::new(),
            provider_only: Vec::new(),
            zdr: false,
            no_data_collection: false,
            redact_secrets: None,
            pinned_from_previous_selection: false,
            coauthor: crate::attribution::Coauthor::default(),
            route: None,
        }
    }

    #[test]
    fn native_deepseek_harness_needs_no_clud_bridge_or_vault() {
        let runtime = ForegroundRuntime::start(
            &plan(ModelProvider::DeepSeek, Backend::DeepSeek),
            vec![("UNCHANGED".to_string(), "yes".to_string())],
        )
        .unwrap();
        assert!(runtime.bridge.is_none());
        assert!(runtime
            .env()
            .starts_with(&[("UNCHANGED".to_string(), "yes".to_string())]));
        assert!(lookup(runtime.env(), "CLUD_ROUTE_CONTEXT").is_some());
    }

    pub(crate) fn lookup<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn unified_overlay_preserves_claude_credentials_and_enables_discovery() {
        let mut route = plan(ModelProvider::Claude, Backend::Claude);
        route.routing_mode = RoutingMode::Unified;
        let base = vec![
            (
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "claude-oauth".to_string(),
            ),
            ("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "xhigh".to_string()),
            (
                "ANTHROPIC_CUSTOM_HEADERS".to_string(),
                "X-Existing: retained".to_string(),
            ),
        ];
        let runtime = ForegroundRuntime::start_with_secret_store(
            &route,
            base.clone(),
            &FakeSecretStore(Some("deepseek-secret".to_string())),
        )
        .unwrap();
        let env = runtime.env();
        assert!(runtime.has_bridge());
        assert_eq!(lookup(env, "ANTHROPIC_AUTH_TOKEN"), Some("claude-oauth"));
        assert_eq!(lookup(env, "ANTHROPIC_BASE_URL"), runtime.base_url());
        assert_eq!(
            lookup(env, "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some("1")
        );
        let headers = lookup(env, "ANTHROPIC_CUSTOM_HEADERS").unwrap();
        assert!(headers.contains("X-Existing: retained"));
        assert!(headers.contains(UNIFIED_GATEWAY_TOKEN_HEADER));
        assert!(headers.contains(runtime.bearer_token().unwrap()));
        assert!(headers.starts_with(UNIFIED_GATEWAY_TOKEN_HEADER));
        assert_eq!(lookup(env, "CLUD_GATEWAY_TOKEN"), runtime.bearer_token());
        assert_eq!(lookup(env, "CLAUDE_CODE_EFFORT_LEVEL"), Some("xhigh"));
        assert_eq!(lookup(&base, "ANTHROPIC_AUTH_TOKEN"), Some("claude-oauth"));
    }

    #[test]
    fn unified_overlay_does_not_inject_a_global_effort_default() {
        let mut route = plan(ModelProvider::Claude, Backend::Claude);
        route.routing_mode = RoutingMode::Unified;
        let runtime = ForegroundRuntime::start_with_secret_store(
            &route,
            Vec::new(),
            &FakeSecretStore(Some("deepseek-secret".to_string())),
        )
        .unwrap();
        assert_eq!(lookup(runtime.env(), "CLAUDE_CODE_EFFORT_LEVEL"), None);
    }

    /// DD-059: the direct Anthropic-compat overlay neither scrubs an ambient
    /// `CLAUDE_CODE_EFFORT_LEVEL` nor injects its own pin, so `/effort` stays
    /// the live session control -- mirroring the unified overlay's rule. The
    /// old code overwrote the ambient value with the selection's effort (or
    /// a `max` fallback when none was selected).
    #[test]
    fn anthropic_compat_overlay_preserves_ambient_effort_and_injects_no_default() {
        let mut route = plan(ModelProvider::DeepSeek, Backend::Claude);
        // A legacy `@high` selection must not re-pin the user's ambient value.
        route.model_selection = crate::provider_catalog::resolve(
            Some(ModelProvider::DeepSeek),
            Some("deepseek-v4-pro@high"),
            None,
            None,
        )
        .unwrap();
        let runtime = ForegroundRuntime::start_with_secret_store(
            &route,
            vec![("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "xhigh".to_string())],
            &FakeSecretStore(Some("deepseek-secret".to_string())),
        )
        .unwrap();
        assert_eq!(
            lookup(runtime.env(), "CLAUDE_CODE_EFFORT_LEVEL"),
            Some("xhigh")
        );
        assert!(!runtime.has_bridge());

        let empty = ForegroundRuntime::start_with_secret_store(
            &route,
            Vec::new(),
            &FakeSecretStore(Some("deepseek-secret".to_string())),
        )
        .unwrap();
        assert_eq!(lookup(empty.env(), "CLAUDE_CODE_EFFORT_LEVEL"), None);
    }

    #[test]
    fn unified_missing_provider_notices_are_sanitized_and_actionable() {
        let notices = unified_startup_notices(false, false, false);
        assert_eq!(notices.len(), 3);
        assert!(notices[0].contains("clud auth login codex"));
        assert!(notices[1].contains("clud auth login deepseek"));
        assert!(notices[2].contains("clud auth login kimi"));
        assert!(!notices.join(" ").to_ascii_lowercase().contains("secret"));
        assert!(unified_startup_notices(true, true, true).is_empty());
        // One missing provider never takes the others' notices with it.
        assert_eq!(
            unified_startup_notices(true, true, false),
            vec!["[clud] unified gateway: Kimi models unavailable; run `clud auth login kimi`"]
        );
    }

    fn deepseek_plan_with(model: &str) -> LaunchPlan {
        let mut route = plan(ModelProvider::DeepSeek, Backend::Claude);
        route.model_selection = Some(
            crate::provider_catalog::resolve(
                Some(ModelProvider::DeepSeek),
                Some(model),
                None,
                None,
            )
            .unwrap()
            .unwrap(),
        );
        route
    }

    /// #1200: a Pro launch must say, once, that the model cannot accept
    /// images. The degradation is silent -- the endpoint answers `200` and the
    /// model replies that it cannot see the picture -- so this line is the
    /// only thing connecting that reply to the model choice.
    #[test]
    fn deepseek_pro_launch_warns_that_images_are_dropped_upstream() {
        let runtime = ForegroundRuntime::start_with_secret_store(
            &deepseek_plan_with("deepseek-v4-pro"),
            Vec::new(),
            &FakeSecretStore(Some("deepseek-secret".to_string())),
        )
        .unwrap();
        let notices = runtime.startup_notices.join(" ");
        assert!(notices.contains("deepseek-v4-pro"), "{notices}");
        assert!(notices.contains("cannot accept images"), "{notices}");
        assert!(notices.contains("--model deepseek-flash"), "{notices}");
    }

    /// #1200 review finding: an auto-context selection is billed as the same
    /// model under a suffix-free spelling (`deepseek-v4-pro`, not
    /// `deepseek-v4-pro[1m]` -- `AUTO_OR_1M_CONTEXT`), so the notice has to
    /// resolve the catalog row rather than the literal wire string. Missing
    /// this spelling is the same silent drop #1200 exists to surface, on a
    /// supported `--context-window` value.
    #[test]
    fn deepseek_pro_auto_context_launch_still_warns_about_images() {
        let mut route = plan(ModelProvider::DeepSeek, Backend::Claude);
        route.model_selection = Some(
            crate::provider_catalog::resolve(
                Some(ModelProvider::DeepSeek),
                Some("deepseek-v4-pro"),
                None,
                Some("auto"),
            )
            .unwrap()
            .unwrap(),
        );
        let runtime = ForegroundRuntime::start_with_secret_store(
            &route,
            Vec::new(),
            &FakeSecretStore(Some("deepseek-secret".to_string())),
        )
        .unwrap();
        assert!(
            runtime
                .startup_notices
                .join(" ")
                .contains("cannot accept images"),
            "{:?}",
            runtime.startup_notices
        );
    }

    /// The warning is evidence-backed, not a family-wide caveat: Flash reads
    /// images correctly on the same endpoint, so a session on it must stay
    /// quiet.
    #[test]
    fn deepseek_flash_launch_says_nothing_about_images() {
        let runtime = ForegroundRuntime::start_with_secret_store(
            &deepseek_plan_with("deepseek-flash"),
            Vec::new(),
            &FakeSecretStore(Some("deepseek-secret".to_string())),
        )
        .unwrap();
        assert!(
            !runtime
                .startup_notices
                .join(" ")
                .contains("cannot accept images"),
            "{:?}",
            runtime.startup_notices
        );
    }

    /// The native DeepSeek harness owns its provider configuration and never
    /// reads the Anthropic-compat overlay, so a Pro selection there must stay
    /// silent: the notice would describe a request clud never sends.
    #[test]
    fn native_deepseek_harness_says_nothing_about_images() {
        let mut route = plan(ModelProvider::DeepSeek, Backend::DeepSeek);
        route.model_selection = Some(
            crate::provider_catalog::resolve(
                Some(ModelProvider::DeepSeek),
                Some("deepseek-v4-pro"),
                None,
                None,
            )
            .unwrap()
            .unwrap(),
        );
        let runtime = ForegroundRuntime::start_with_secret_store(
            &route,
            Vec::new(),
            &FakeSecretStore(Some("deepseek-secret".to_string())),
        )
        .unwrap();
        assert!(
            runtime.startup_notices.is_empty(),
            "{:?}",
            runtime.startup_notices
        );
    }

    /// No descriptor resolves for Claude, so the native route can never carry
    /// a provider-capability warning.
    #[test]
    fn claude_launch_says_nothing_about_images() {
        let runtime = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::Claude, Backend::Claude),
            Vec::new(),
            &FakeSecretStore(Some("claude-secret".to_string())),
        )
        .unwrap();
        assert!(
            runtime.startup_notices.is_empty(),
            "{:?}",
            runtime.startup_notices
        );
    }

    #[test]
    fn unified_mode_refuses_disabled_model_discovery() {
        let mut route = plan(ModelProvider::Claude, Backend::Claude);
        route.routing_mode = RoutingMode::Unified;
        let error = ForegroundRuntime::start_with_secret_store(
            &route,
            vec![(
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
                "1".to_string(),
            )],
            &FakeSecretStore(None),
        )
        .unwrap_err();
        assert!(matches!(error, BridgeError::DiscoveryDisabled));
    }

    #[test]
    fn cross_route_overlay_is_child_local_secret_safe_and_honors_defaults() {
        let base = vec![
            ("UNCHANGED".to_string(), "yes".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "ambient-key".to_string()),
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                "ambient-opus".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME".to_string(),
                "ambient-opus-name".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
                "ambient-sonnet".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME".to_string(),
                "ambient-sonnet-name".to_string(),
            ),
            ("API_TIMEOUT_MS".to_string(), "custom-timeout".to_string()),
        ];
        let runtime =
            ForegroundRuntime::start(&plan(ModelProvider::Codex, Backend::Claude), base.clone())
                .unwrap();
        let env = runtime.env();
        assert_eq!(lookup(env, "UNCHANGED"), Some("yes"));
        assert_eq!(lookup(env, "ANTHROPIC_API_KEY"), None);
        assert_eq!(
            lookup(env, "ANTHROPIC_BASE_URL"),
            Some(runtime.base_url().unwrap())
        );
        assert_eq!(
            lookup(env, "ANTHROPIC_AUTH_TOKEN"),
            Some(runtime.bearer_token().unwrap())
        );
        assert_eq!(lookup(env, "API_TIMEOUT_MS"), Some("custom-timeout"));
        assert_eq!(
            lookup(env, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some(CODEX_VIA_CLAUDE_OPUS_MODEL)
        );
        assert_eq!(
            lookup(env, "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME"),
            Some("Codex Sol (OpenAI)")
        );
        assert_eq!(
            lookup(env, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some(CODEX_VIA_CLAUDE_SONNET_MODEL)
        );
        assert_eq!(
            lookup(env, "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"),
            Some("Codex Terra (OpenAI)")
        );
        let route: serde_json::Value = serde_json::from_str(
            lookup(env, ROUTE_CONTEXT_ENV).expect("all child launches carry route context"),
        )
        .unwrap();
        assert_eq!(route["model_provider"], "codex");
        assert_eq!(route["harness"], "claude");
        assert_eq!(route["delegation"]["roles"]["planner"], "opus");
        assert_eq!(
            route["delegation"]["resolved_models"]["opus"],
            CODEX_VIA_CLAUDE_OPUS_MODEL
        );
        assert_eq!(
            lookup(env, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
            None
        );
        assert_eq!(
            lookup(env, "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some("1")
        );
        assert_eq!(
            lookup(env, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some("1050000")
        );
        assert_eq!(lookup(&base, "ANTHROPIC_API_KEY"), Some("ambient-key"));
        assert_eq!(
            lookup(&base, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some("ambient-opus")
        );
    }

    #[test]
    fn route_context_describes_native_launches_without_bridge_aliases() {
        let runtime =
            ForegroundRuntime::start(&plan(ModelProvider::Codex, Backend::Codex), Vec::new())
                .unwrap();
        let route: serde_json::Value = serde_json::from_str(
            lookup(runtime.env(), ROUTE_CONTEXT_ENV)
                .expect("all child launches carry route context"),
        )
        .unwrap();
        assert_eq!(route["model_provider"], "codex");
        assert_eq!(route["harness"], "codex");
        assert_eq!(
            route["delegation"]["cost_policy"],
            "prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed"
        );
        assert!(route["delegation"].get("resolved_models").is_none());
    }

    /// The discovery catalog replaces the old scalar custom-row extension.
    #[test]
    fn direct_codex_discovery_scrubs_the_legacy_custom_picker_row() {
        let runtime = ForegroundRuntime::start(
            &plan(ModelProvider::Codex, Backend::Claude),
            vec![(
                "ANTHROPIC_CUSTOM_MODEL_OPTION".to_string(),
                "stale-row".to_string(),
            )],
        )
        .unwrap();
        assert_eq!(lookup(runtime.env(), "ANTHROPIC_CUSTOM_MODEL_OPTION"), None);
    }

    /// Discovery cannot work while the harness's network kill switch is set.
    #[test]
    fn direct_codex_refuses_disabled_model_discovery() {
        let error = ForegroundRuntime::start(
            &plan(ModelProvider::Codex, Backend::Claude),
            vec![(
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
                "1".to_string(),
            )],
        )
        .unwrap_err();
        assert!(matches!(error, BridgeError::DiscoveryDisabled));
    }

    /// New typed plans no longer depend on the compatibility selector field.
    #[test]
    fn normalized_selection_supersedes_the_legacy_plan_field() {
        let mut route = plan(ModelProvider::Codex, Backend::Claude);
        route.codex_model = Some("tera".to_string());
        route.model_selection = crate::provider_catalog::resolve(
            Some(ModelProvider::Codex),
            Some("luna"),
            Some("high"),
            None,
        )
        .unwrap();
        ForegroundRuntime::start(&route, Vec::new()).unwrap();
    }

    /// A bad selection fails the launch, not the first turn: by the time a
    /// request is in flight the user has waited, and the message arrives
    /// wrapped in the harness's own API-error framing.
    #[test]
    fn an_unparseable_selection_fails_the_launch_with_the_valid_names() {
        let mut plan = plan(ModelProvider::Codex, Backend::Claude);
        plan.codex_model = Some("tera".to_string());
        let error = ForegroundRuntime::start(&plan, Vec::new()).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("tera"), "{rendered}");
        assert!(rendered.contains("terra"), "{rendered}");
    }

    /// The direct route for the overlay tests: the same decision the
    /// production builder makes, from the inputs those tests already have.
    fn direct_route(
        descriptor: &'static crate::provider_registry::AnthropicCompatProvider,
        selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
        allowed: &[String],
        env: &[(String, String)],
    ) -> crate::route_plan::ResolvedRoute {
        let mut plan = plan(descriptor.provider, Backend::Claude);
        plan.model_selection = selection.cloned();
        plan.allowed_models = allowed.to_vec();
        crate::route_plan::resolve(&plan, &crate::route_plan::Ambient::from_child_env(env))
    }

    fn deepseek_descriptor() -> &'static crate::provider_registry::AnthropicCompatProvider {
        crate::provider_registry::descriptor_for(ModelProvider::DeepSeek)
            .expect("DeepSeek must have an Anthropic-compat descriptor")
    }

    /// GOLDEN (issue #937 Phase 2 Lane 2A): the frozen pre-refactor baseline
    /// for the default (no selection) case, captured against
    /// `apply_deepseek_overlay` before it became
    /// `apply_anthropic_compat_overlay`, updated for exactly one documented
    /// delta -- the new `ANTHROPIC_DEFAULT_FABLE_MODEL` pin (#936
    /// "Generalization" -> 1d). Every other pair is byte-identical to the
    /// pre-refactor baseline that was confirmed green before this function
    /// was touched. Second delta (DD-059): the `CLAUDE_CODE_EFFORT_LEVEL`
    /// pin is gone -- effort travels on the harness's `--effort` flag and the
    /// overlay neither injects nor scrubs it. Third delta (#1263): the overlay
    /// now pushes `API_TIMEOUT_MS` like every other route, so a direct launch
    /// no longer inherits the harness's undocumented client timeout.
    #[test]
    fn golden_anthropic_compat_overlay_default_selection() {
        let mut env = Vec::new();
        crate::route_plan::render_direct_env(
            &direct_route(deepseek_descriptor(), None, &[], &env),
            "ds-golden-secret",
        )
        .apply(&mut env);
        assert_eq!(lookup(&env, "CLAUDE_CODE_EFFORT_LEVEL"), None);
        let mut pairs = env.clone();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (
                    "ANTHROPIC_AUTH_TOKEN".to_string(),
                    "ds-golden-secret".to_string()
                ),
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "https://api.deepseek.com/anthropic".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_FABLE_MODEL".to_string(),
                    "deepseek-flash[1m]".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                    "deepseek-flash[1m]".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                    "deepseek-flash[1m]".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
                    "deepseek-flash[1m]".to_string()
                ),
                (
                    "ANTHROPIC_MODEL".to_string(),
                    "deepseek-flash[1m]".to_string()
                ),
                ("API_TIMEOUT_MS".to_string(), "600000".to_string()),
                (
                    "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
                    "786432".to_string()
                ),
                (
                    "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
                    "deepseek-flash[1m]".to_string()
                ),
            ]
        );
    }

    /// GOLDEN: a selection whose wire model has no `[1m]` suffix, so no
    /// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` is set. Same deltas as above: only
    /// the new `ANTHROPIC_DEFAULT_FABLE_MODEL` pin is added relative to the
    /// confirmed pre-refactor baseline, and no `CLAUDE_CODE_EFFORT_LEVEL`
    /// pin is emitted even for an explicitly selected effort (DD-059). The
    /// `API_TIMEOUT_MS` push (#1263) is present here too -- it is emitted
    /// unconditionally by the overlay, not only for the default selection.
    #[test]
    fn golden_anthropic_compat_overlay_auto_context_selection_has_no_compact_window() {
        let selection = crate::provider_catalog::resolve(
            Some(ModelProvider::DeepSeek),
            Some("deepseek-v4-pro"),
            Some("high"),
            Some("auto"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(selection.wire_model.as_deref(), Some("deepseek-v4-pro"));
        let mut env = Vec::new();
        crate::route_plan::render_direct_env(
            &direct_route(deepseek_descriptor(), Some(&selection), &[], &env),
            "ds-golden-secret",
        )
        .apply(&mut env);
        let mut pairs = env.clone();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (
                    "ANTHROPIC_AUTH_TOKEN".to_string(),
                    "ds-golden-secret".to_string()
                ),
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "https://api.deepseek.com/anthropic".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_FABLE_MODEL".to_string(),
                    "deepseek-v4-pro".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                    "deepseek-flash[1m]".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                    "deepseek-v4-pro".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
                    "deepseek-v4-pro".to_string()
                ),
                ("ANTHROPIC_MODEL".to_string(), "deepseek-v4-pro".to_string()),
                ("API_TIMEOUT_MS".to_string(), "600000".to_string()),
                (
                    "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
                    "deepseek-flash[1m]".to_string()
                ),
            ]
        );
        assert_eq!(lookup(&env, "CLAUDE_CODE_EFFORT_LEVEL"), None);
    }

    /// GOLDEN: the second documented delta -- the widened scrub const now
    /// removes `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_DEFAULT_FABLE_MODEL`,
    /// and the legacy `*_NAME` forms. Before this phase's refactor, an
    /// identically-shaped test (`frozen_baseline_deepseek_overlay_does_not_yet_scrub_the_widened_keys`,
    /// since replaced by this one) proved these keys survived unscrubbed;
    /// that was the review finding #936/#937 document as the intended
    /// hardening delta.
    #[test]
    fn golden_anthropic_compat_overlay_scrubs_the_widened_keys() {
        let base = vec![
            (
                "ANTHROPIC_SMALL_FAST_MODEL".to_string(),
                "ambient-fast".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_FABLE_MODEL".to_string(),
                "ambient-fable".to_string(),
            ),
            (
                "ANTHROPIC_MODEL_NAME".to_string(),
                "ambient-model-name".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME".to_string(),
                "ambient-opus-name".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME".to_string(),
                "ambient-sonnet-name".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME".to_string(),
                "ambient-haiku-name".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME".to_string(),
                "ambient-fable-name".to_string(),
            ),
        ];
        let mut env = base.clone();
        crate::route_plan::render_direct_env(
            &direct_route(deepseek_descriptor(), None, &[], &env),
            "ds-golden-secret",
        )
        .apply(&mut env);
        for (key, _) in &base {
            assert_eq!(
                lookup(&env, key),
                if key == "ANTHROPIC_DEFAULT_FABLE_MODEL" {
                    // This slot is scrubbed AND re-set by the overlay itself
                    // (the new FABLE pin), so its post-overlay value is the
                    // overlay's own model, not `None` and not the ambient value.
                    Some("deepseek-flash[1m]")
                } else {
                    None
                },
                "{key} must no longer carry its ambient value after the widened scrub"
            );
        }
    }

    #[test]
    fn deepseek_overlay_replaces_conflicting_profile_values_without_parent_mutation() {
        let base = vec![
            ("anthropic_api_key".to_string(), "ambient-key".to_string()),
            ("ANTHROPIC_MODEL".to_string(), "ambient-model".to_string()),
            (
                "ANTHROPIC_CUSTOM_MODEL_OPTION".to_string(),
                "ambient-picker".to_string(),
            ),
            ("UNCHANGED".to_string(), "yes".to_string()),
        ];
        let mut child = base.clone();
        crate::route_plan::render_direct_env(
            &direct_route(deepseek_descriptor(), None, &[], &child),
            "ds-test-secret",
        )
        .apply(&mut child);

        assert_eq!(lookup(&child, "UNCHANGED"), Some("yes"));
        assert_eq!(lookup(&child, "anthropic_api_key"), None);
        assert_eq!(lookup(&child, "ANTHROPIC_CUSTOM_MODEL_OPTION"), None);
        assert_eq!(
            lookup(&child, "ANTHROPIC_BASE_URL"),
            Some("https://api.deepseek.com/anthropic")
        );
        assert_eq!(
            lookup(&child, "ANTHROPIC_AUTH_TOKEN"),
            Some("ds-test-secret")
        );
        assert_eq!(
            lookup(&child, "ANTHROPIC_MODEL"),
            Some("deepseek-flash[1m]")
        );
        assert_eq!(
            lookup(&child, "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("deepseek-flash[1m]")
        );
        assert_eq!(
            lookup(&child, "CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            Some("786432")
        );
        assert_eq!(lookup(&base, "anthropic_api_key"), Some("ambient-key"));
    }

    #[test]
    fn deepseek_overlay_applies_explicit_model_context_and_effort() {
        let selection = crate::provider_catalog::resolve(
            Some(ModelProvider::DeepSeek),
            Some("deepseek-v4-pro"),
            Some("high"),
            Some("auto"),
        )
        .unwrap()
        .unwrap();
        let mut env = Vec::new();
        crate::route_plan::render_direct_env(
            &direct_route(deepseek_descriptor(), Some(&selection), &[], &env),
            "ds-test-secret",
        )
        .apply(&mut env);
        assert_eq!(lookup(&env, "ANTHROPIC_MODEL"), Some("deepseek-v4-pro"));
        assert_eq!(lookup(&env, "CLAUDE_CODE_EFFORT_LEVEL"), None);
        assert_eq!(lookup(&env, "CLAUDE_CODE_AUTO_COMPACT_WINDOW"), None);
    }

    #[test]
    fn native_routes_receive_the_original_environment_byte_for_byte() {
        let base = vec![
            ("ANTHROPIC_BASE_URL".to_string(), "user-url".to_string()),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), "user-token".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "user-key".to_string()),
        ];
        for route in [
            plan(ModelProvider::Claude, Backend::Claude),
            plan(ModelProvider::Codex, Backend::Codex),
        ] {
            let runtime = ForegroundRuntime::start(&route, base.clone()).unwrap();
            assert!(runtime.env().starts_with(&base));
            assert!(lookup(runtime.env(), "CLUD_ROUTE_CONTEXT").is_some());
            assert!(!runtime.has_bridge());
        }
    }

    /// Issue #880: every route `ForegroundRuntime::start` can resolve to,
    /// exercised through the one dispatch point rather than the overlay
    /// helpers in isolation. DeepSeek must never create a `BridgeHandle` --
    /// its route is the direct child-overlay path, not the loopback bridge.
    #[test]
    fn every_provider_harness_route_gets_exactly_the_expected_bridge_state() {
        let base = vec![("UNRELATED".to_string(), "kept".to_string())];
        let store = FakeSecretStore(Some("ds-routing-secret".to_string()));

        let native_claude = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::Claude, Backend::Claude),
            base.clone(),
            &store,
        )
        .unwrap();
        assert!(!native_claude.has_bridge());
        assert!(native_claude.env().starts_with(&base));
        assert!(lookup(native_claude.env(), "CLUD_ROUTE_CONTEXT").is_some());

        let native_codex = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::Codex, Backend::Codex),
            base.clone(),
            &store,
        )
        .unwrap();
        assert!(!native_codex.has_bridge());
        assert!(native_codex.env().starts_with(&base));
        assert!(lookup(native_codex.env(), "CLUD_ROUTE_CONTEXT").is_some());

        let codex_bridge = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::Codex, Backend::Claude),
            base.clone(),
            &store,
        )
        .unwrap();
        assert!(codex_bridge.has_bridge());

        let deepseek_direct = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::DeepSeek, Backend::Claude),
            base.clone(),
            &store,
        )
        .unwrap();
        assert!(
            !deepseek_direct.has_bridge(),
            "DeepSeek must route directly, never through BridgeHandle"
        );
        assert_eq!(
            lookup(deepseek_direct.env(), "ANTHROPIC_AUTH_TOKEN"),
            Some("ds-routing-secret")
        );
        assert_eq!(lookup(deepseek_direct.env(), "UNRELATED"), Some("kept"));
    }

    /// #1270: every descriptor-backed direct route uses the same documented
    /// per-request timeout; the notice reports that exact effective value.
    #[test]
    fn direct_anthropic_compat_launch_pushes_and_reports_an_api_timeout() {
        let store = FakeSecretStore(Some("ds-timeout-secret".to_string()));
        for (provider, name) in [
            (ModelProvider::DeepSeek, "DeepSeek"),
            (ModelProvider::Kimi, "Kimi"),
            (ModelProvider::OpenRouter, "OpenRouter"),
        ] {
            let runtime = ForegroundRuntime::start_with_secret_store(
                &plan(provider, Backend::Claude),
                Vec::new(),
                &store,
            )
            .unwrap();
            assert_eq!(lookup(runtime.env(), "API_TIMEOUT_MS"), Some("600000"));
            let notice = runtime
                .startup_notices
                .iter()
                .find(|notice| notice.contains("API timeout"))
                .unwrap();
            assert_eq!(notice, &format!("[clud] direct {name} route: Claude Code API timeout 600000 ms (set API_TIMEOUT_MS to override); clud does not monitor this request"));
        }
    }

    /// The push is a *default*, so a user who has already chosen a client
    /// timeout keeps it, and the notice reports the value actually in effect
    /// rather than restating the built-in (the precedence DD-059 established
    /// for `CLAUDE_CODE_EFFORT_LEVEL`, applied to #1263).
    #[test]
    fn direct_anthropic_compat_launch_keeps_an_ambient_api_timeout() {
        let store = FakeSecretStore(Some("ds-timeout-secret".to_string()));
        let runtime = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::DeepSeek, Backend::Claude),
            vec![("API_TIMEOUT_MS".to_string(), "45000".to_string())],
            &store,
        )
        .unwrap();
        assert_eq!(lookup(runtime.env(), "API_TIMEOUT_MS"), Some("45000"));
        let notice = runtime
            .startup_notices
            .iter()
            .find(|notice| notice.contains("API timeout"))
            .unwrap();
        assert_eq!(notice, "[clud] direct DeepSeek route: Claude Code API timeout 45000 ms (set API_TIMEOUT_MS to override); clud does not monitor this request");
    }

    #[test]
    fn deepseek_route_without_a_stored_credential_fails_the_launch() {
        let store = FakeSecretStore(None);
        let error = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::DeepSeek, Backend::Claude),
            Vec::new(),
            &store,
        )
        .unwrap_err();
        assert!(matches!(&error, BridgeError::AnthropicCompatCredentials));
    }

    type RecordedEnvironment = Vec<(String, String)>;
    type RecordedSpawn = (SpawnMode, Vec<String>, RecordedEnvironment);

    #[derive(Default)]
    struct RecordingAdapter {
        calls: RefCell<Vec<RecordedSpawn>>,
    }

    impl SpawnAdapter<()> for RecordingAdapter {
        type Error = std::io::Error;

        fn spawn(
            &self,
            mode: SpawnMode,
            command: Vec<String>,
            _cwd: Option<String>,
            env: Vec<(String, String)>,
        ) -> std::io::Result<()> {
            self.calls.borrow_mut().push((mode, command, env));
            Ok(())
        }
    }

    #[test]
    fn subprocess_and_pty_adapters_receive_the_same_overlay() {
        let runtime =
            ForegroundRuntime::start(&plan(ModelProvider::Codex, Backend::Claude), Vec::new())
                .unwrap();
        let adapter = RecordingAdapter::default();
        runtime
            .spawn_with(&adapter, SpawnMode::Subprocess, vec!["claude".into()], None)
            .unwrap();
        runtime
            .spawn_with(&adapter, SpawnMode::Pty, vec!["claude".into()], None)
            .unwrap();
        let calls = adapter.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, SpawnMode::Subprocess);
        assert_eq!(calls[1].0, SpawnMode::Pty);
        assert_eq!(calls[0].1, calls[1].1);
        assert_eq!(calls[0].2, calls[1].2);
        assert!(lookup(&calls[0].2, "ANTHROPIC_AUTH_TOKEN").is_some());
    }

    #[test]
    fn deepseek_subprocess_and_pty_receive_the_same_secret_child_overlay() {
        let mut env = vec![("ANTHROPIC_API_KEY".to_string(), "ambient-key".to_string())];
        crate::route_plan::render_direct_env(
            &direct_route(deepseek_descriptor(), None, &[], &env),
            "ds-test-secret",
        )
        .apply(&mut env);
        let runtime = ForegroundRuntime {
            env,
            bridge: None,
            claude_settings: None,
            startup_notices: Vec::new(),
        };
        let adapter = RecordingAdapter::default();
        runtime
            .spawn_with(&adapter, SpawnMode::Subprocess, vec!["claude".into()], None)
            .unwrap();
        runtime
            .spawn_with(&adapter, SpawnMode::Pty, vec!["claude".into()], None)
            .unwrap();

        let calls = adapter.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].2, calls[1].2);
        assert_eq!(
            lookup(&calls[0].2, "ANTHROPIC_AUTH_TOKEN"),
            Some("ds-test-secret")
        );
        assert_eq!(lookup(&calls[0].2, "ANTHROPIC_API_KEY"), None);
        assert!(!runtime.has_bridge());
    }

    #[test]
    fn bridge_spawns_register_authenticated_context_lifecycle_hooks() {
        let runtime =
            ForegroundRuntime::start(&plan(ModelProvider::Codex, Backend::Claude), Vec::new())
                .unwrap();
        let adapter = RecordingAdapter::default();
        runtime
            .spawn_with(
                &adapter,
                SpawnMode::Subprocess,
                vec!["claude".into(), "-p".into(), "hello".into()],
                None,
            )
            .unwrap();

        let calls = adapter.calls.borrow();
        let command = &calls[0].1;
        let settings_index = command
            .iter()
            .position(|argument| argument == "--settings")
            .expect("the bridge launch must register session-local lifecycle hooks");
        let settings_path = PathBuf::from(&command[settings_index + 1]);
        assert!(settings_path.is_absolute());
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("the settings file must remain live"),
        )
        .expect("--settings must point to valid JSON");
        let base_url = runtime.base_url().unwrap();
        assert_eq!(settings["hooks"]["PreCompact"][0]["matcher"], "manual|auto");
        assert_eq!(
            settings["hooks"]["PreCompact"][0]["hooks"][0]["url"],
            format!("{base_url}/_clud/context/compact")
        );
        assert_eq!(settings["hooks"]["SessionStart"][0]["matcher"], "clear");
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["hooks"][0]["url"],
            format!("{base_url}/_clud/context/clear")
        );
        assert_eq!(settings["hooks"]["SessionStart"][1]["matcher"], "compact");
        assert_eq!(
            settings["hooks"]["SessionStart"][1]["hooks"][0]["url"],
            format!("{base_url}/_clud/context/compact-finished")
        );
        // clud's own session-history hook (#922) rides alongside, unmatched.
        assert!(settings["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry.get("matcher").is_none()
                && entry["hooks"][0]["command"]
                    .as_str()
                    .is_some_and(|c| c.contains(" session-hook "))));
        for event in ["PreCompact", "SessionStart"] {
            for entry in settings["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry.get("matcher").is_some())
            {
                let hook = &entry["hooks"][0];
                assert_eq!(hook["type"], "http");
                assert_eq!(
                    hook["headers"]["Authorization"],
                    "Bearer $ANTHROPIC_AUTH_TOKEN"
                );
                assert_eq!(
                    hook["allowedEnvVars"],
                    serde_json::json!(["ANTHROPIC_AUTH_TOKEN"])
                );
            }
        }
        assert!(
            !command.join(" ").contains(runtime.bearer_token().unwrap()),
            "the launch-scoped bearer must stay in the environment, not argv"
        );
        assert!(
            !command.join(" ").contains(base_url),
            "the launch-private bridge URL must stay out of argv"
        );
    }

    #[test]
    fn bridge_merges_inline_user_settings_with_context_lifecycle_hooks() {
        let mut route = plan(ModelProvider::Codex, Backend::Claude);
        route.command = vec![
            "claude".into(),
            "--settings".into(),
            serde_json::json!({
                "permissions": {"defaultMode": "plan"},
                "hooks": {
                    "PreCompact": [{
                        "matcher": "manual",
                        "hooks": [{"type": "command", "command": "echo user-hook"}]
                    }]
                }
            })
            .to_string(),
        ];
        let runtime = ForegroundRuntime::start(&route, Vec::new()).unwrap();
        let adapter = RecordingAdapter::default();
        runtime
            .spawn_with(
                &adapter,
                SpawnMode::Subprocess,
                route.command.clone(),
                route.cwd.clone(),
            )
            .unwrap();

        let calls = adapter.calls.borrow();
        let command = &calls[0].1;
        assert_eq!(
            command
                .iter()
                .filter(|argument| argument.as_str() == "--settings")
                .count(),
            1
        );
        let merged_path = PathBuf::from(&command[2]);
        assert!(merged_path.is_absolute());
        assert!(!command.join(" ").contains("echo user-hook"));
        assert!(!command.join(" ").contains(runtime.base_url().unwrap()));
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(merged_path).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "plan");
        assert_eq!(
            settings["hooks"]["PreCompact"][0]["hooks"][0]["command"],
            "echo user-hook"
        );
        assert_eq!(settings["hooks"]["PreCompact"].as_array().unwrap().len(), 2);
        assert_eq!(settings["hooks"]["SessionStart"][0]["matcher"], "clear");
    }

    #[test]
    fn bridge_merges_file_user_settings_with_context_lifecycle_hooks() {
        let directory = tempfile::tempdir().unwrap();
        let settings_path = directory.path().join("claude-settings.json");
        std::fs::write(
            &settings_path,
            serde_json::json!({
                "env": {"USER_SETTING": "preserved"},
                "hooks": {
                    "SessionStart": [{
                        "matcher": "startup",
                        "hooks": [{"type": "command", "command": "echo startup"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut route = plan(ModelProvider::Codex, Backend::Claude);
        route.cwd = Some(directory.path().to_string_lossy().into_owned());
        route.command = vec!["claude".into(), "--settings=claude-settings.json".into()];
        let runtime = ForegroundRuntime::start(&route, Vec::new()).unwrap();
        let adapter = RecordingAdapter::default();
        runtime
            .spawn_with(
                &adapter,
                SpawnMode::Pty,
                route.command.clone(),
                route.cwd.clone(),
            )
            .unwrap();

        let calls = adapter.calls.borrow();
        let command = &calls[0].1;
        assert_eq!(command[1], "--settings");
        assert_eq!(command.len(), 3);
        let merged_path = PathBuf::from(&command[2]);
        assert!(merged_path.is_absolute());
        assert!(!command.join(" ").contains("preserved"));
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&merged_path).unwrap()).unwrap();
        assert_eq!(settings["env"]["USER_SETTING"], "preserved");
        // The user's hook, the bridge's clear and compact-finished hooks, and
        // clud's session-history hook (#922).
        assert_eq!(
            settings["hooks"]["SessionStart"].as_array().unwrap().len(),
            4
        );
        assert_eq!(settings["hooks"]["PreCompact"][0]["matcher"], "manual|auto");
        drop(calls);
        drop(runtime);
        assert!(!merged_path.exists());
    }

    #[test]
    fn bridge_leaves_settings_shaped_positional_arguments_untouched() {
        let mut route = plan(ModelProvider::Codex, Backend::Claude);
        route.command = vec![
            "claude".into(),
            "--".into(),
            "--settings".into(),
            "literal prompt text".into(),
        ];
        let runtime = ForegroundRuntime::start(&route, Vec::new()).unwrap();
        let adapter = RecordingAdapter::default();
        runtime
            .spawn_with(&adapter, SpawnMode::Subprocess, route.command.clone(), None)
            .unwrap();

        let calls = adapter.calls.borrow();
        let command = &calls[0].1;
        assert_eq!(&command[3..], &route.command[1..]);
        assert_eq!(command[1], "--settings");
        let generated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&command[2]).unwrap()).unwrap();
        assert_eq!(
            generated["hooks"]["PreCompact"][0]["matcher"],
            "manual|auto"
        );
    }

    #[test]
    fn all_scoped_outcomes_drop_the_bridge_and_close_its_port() {
        #[derive(Clone, Copy)]
        enum Outcome {
            Success,
            ChildFailure,
            SpawnFailure,
            Cancelled,
        }

        for outcome in [
            Outcome::Success,
            Outcome::ChildFailure,
            Outcome::SpawnFailure,
            Outcome::Cancelled,
        ] {
            let mut address = None;
            let result = with_foreground_runtime(
                &plan(ModelProvider::Codex, Backend::Claude),
                Vec::new(),
                |runtime| {
                    address = runtime.socket_addr();
                    match outcome {
                        Outcome::Success => 0,
                        Outcome::ChildFailure | Outcome::SpawnFailure => 1,
                        Outcome::Cancelled => 130,
                    }
                },
            )
            .unwrap();
            assert!(matches!(result, 0 | 1 | 130));

            // The property is that the runtime released the bridge port.
            //
            // Probing with `connect` and requiring a refusal is not that
            // property: ephemeral ports are recycled promptly, so a sibling
            // test in the same parallel run can bind this exact address
            // between the drop and the probe, and the connect then succeeds
            // against *its* listener. `BridgeHandle::shutdown` joins the
            // serve thread, so ours is provably gone by the time we get here
            // -- the old assertion was reading someone else's socket and
            // calling it a leak. It failed roughly half of full-suite runs
            // locally while passing in isolation, which is the signature.
            //
            // Binding is the question actually worth asking. If the bind
            // succeeds the port was free, which is the property. If it fails,
            // another listener owns the address now, so nothing about our
            // bridge can be concluded either way -- inconclusive, not a
            // failure.
            let address = address.unwrap();
            if let Ok(reclaimed) = TcpListener::bind(address) {
                drop(reclaimed);
            }
        }
    }

    #[test]
    fn runtime_debug_omits_bridge_url_and_token() {
        let runtime =
            ForegroundRuntime::start(&plan(ModelProvider::Codex, Backend::Claude), Vec::new())
                .unwrap();
        let rendered = format!("{runtime:?}");
        assert!(!rendered.contains(runtime.base_url().unwrap()));
        assert!(!rendered.contains(runtime.bearer_token().unwrap()));
    }

    // -- Kimi (#937 Phase 3, Lane 3B) -----------------------------------
    //
    // `apply_anthropic_compat_overlay` is fully generic (landed in Phase 2),
    // so nothing below requires a production change in this file -- these
    // tests exist to prove the existing generic machinery produces exactly
    // Kimi's official profile (#936) once Lane 3A's descriptor/catalog rows
    // land, and to pin the routing/no-bridge/no-secret-leak guarantees
    // DeepSeek already has.

    fn kimi_descriptor() -> &'static crate::provider_registry::AnthropicCompatProvider {
        crate::provider_registry::descriptor_for(ModelProvider::Kimi)
            .expect("Kimi must have an Anthropic-compat descriptor")
    }

    /// #936's exact documented profile: every default-model slot *and* the
    /// subagent slot pin to `kimi-k3[1m]` (unlike DeepSeek, whose
    /// haiku/subagent slots use a cheaper flash model), compact window
    /// 1048576, effort max, and the route-agnostic `API_TIMEOUT_MS` (#1263).
    #[test]
    fn golden_kimi_overlay_default_selection() {
        let mut env = Vec::new();
        crate::route_plan::render_direct_env(
            &direct_route(kimi_descriptor(), None, &[], &env),
            "kimi-golden-secret",
        )
        .apply(&mut env);
        let mut pairs = env.clone();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (
                    "ANTHROPIC_AUTH_TOKEN".to_string(),
                    "kimi-golden-secret".to_string()
                ),
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "https://api.moonshot.ai/anthropic".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_FABLE_MODEL".to_string(),
                    "kimi-k3[1m]".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                    "kimi-k3[1m]".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                    "kimi-k3[1m]".to_string()
                ),
                (
                    "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
                    "kimi-k3[1m]".to_string()
                ),
                ("ANTHROPIC_MODEL".to_string(), "kimi-k3[1m]".to_string()),
                ("API_TIMEOUT_MS".to_string(), "600000".to_string()),
                (
                    "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
                    "1048576".to_string()
                ),
                (
                    "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
                    "kimi-k3[1m]".to_string()
                ),
            ]
        );
    }

    /// Ambient `ANTHROPIC_API_KEY`, `ANTHROPIC_SMALL_FAST_MODEL`, and the
    /// mixed-case `anthropic_api_key` form are actively scrubbed by the
    /// shared union const. `MOONSHOT_API_KEY` is not an `ANTHROPIC_*`
    /// variable and is not scrubbed (there is no ambient-credential fallback
    /// to scrub away -- #936 says "do not fall back to ... ambient
    /// MOONSHOT_API_KEY", a claim about which value seeds
    /// `ANTHROPIC_AUTH_TOKEN`, not about deleting an unrelated key from the
    /// child env). The real proof is that the auth token equals the
    /// injected vault secret and never the ambient Moonshot value, even
    /// when both are present in the base environment. The parent `base`
    /// vector must stay byte-identical throughout.
    #[test]
    fn kimi_overlay_scrubs_ambient_anthropic_values_and_never_consults_moonshot_key() {
        let base = vec![
            ("ANTHROPIC_API_KEY".to_string(), "ambient-key".to_string()),
            (
                "ANTHROPIC_SMALL_FAST_MODEL".to_string(),
                "ambient-fast".to_string(),
            ),
            (
                "anthropic_api_key".to_string(),
                "ambient-mixed-case-key".to_string(),
            ),
            (
                "MOONSHOT_API_KEY".to_string(),
                "ambient-moonshot-key".to_string(),
            ),
            ("UNCHANGED".to_string(), "yes".to_string()),
        ];
        let mut child = base.clone();
        crate::route_plan::render_direct_env(
            &direct_route(kimi_descriptor(), None, &[], &child),
            "kimi-test-secret",
        )
        .apply(&mut child);

        assert_eq!(lookup(&child, "UNCHANGED"), Some("yes"));
        assert_eq!(lookup(&child, "ANTHROPIC_API_KEY"), None);
        assert_eq!(lookup(&child, "ANTHROPIC_SMALL_FAST_MODEL"), None);
        assert_eq!(lookup(&child, "anthropic_api_key"), None);
        assert_eq!(
            lookup(&child, "ANTHROPIC_AUTH_TOKEN"),
            Some("kimi-test-secret"),
            "the auth token must come from the injected vault secret, never an ambient key"
        );
        assert_ne!(
            lookup(&child, "ANTHROPIC_AUTH_TOKEN"),
            Some("ambient-moonshot-key"),
            "MOONSHOT_API_KEY must never be consulted as a credential source"
        );
        assert_eq!(lookup(&base, "ANTHROPIC_API_KEY"), Some("ambient-key"));
        assert_eq!(
            lookup(&base, "MOONSHOT_API_KEY"),
            Some("ambient-moonshot-key")
        );
    }

    /// Kimi routes directly through the child-overlay path, exactly like
    /// DeepSeek: no `BridgeHandle`, so no loopback listener either.
    #[test]
    fn kimi_direct_route_creates_no_bridge_and_no_listener() {
        let base = vec![("UNRELATED".to_string(), "kept".to_string())];
        let store = FakeSecretStore(Some("kimi-routing-secret".to_string()));
        let runtime = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::Kimi, Backend::Claude),
            base.clone(),
            &store,
        )
        .unwrap();
        assert!(
            !runtime.has_bridge(),
            "Kimi must route directly, never through BridgeHandle"
        );
        assert_eq!(runtime.socket_addr(), None, "no listener without a bridge");
        assert_eq!(runtime.base_url(), None);
        assert_eq!(runtime.bearer_token(), None);
        assert_eq!(
            lookup(runtime.env(), "ANTHROPIC_AUTH_TOKEN"),
            Some("kimi-routing-secret")
        );
        assert_eq!(lookup(runtime.env(), "UNRELATED"), Some("kept"));
    }

    /// #936: the Kimi key reaches the child env and nowhere else a user or log
    /// could see -- not the runtime's `Debug`, the plan's `Debug`, or the
    /// plan's serialized (dry-run / daemon) form.
    #[test]
    fn kimi_runtime_and_plan_debug_omit_the_secret() {
        let secret = "sk-kimi-debug-canary-0123456789abcdef";
        let plan = plan(ModelProvider::Kimi, Backend::Claude);
        let runtime = ForegroundRuntime::start_with_secret_store(
            &plan,
            Vec::new(),
            &FakeSecretStore(Some(secret.to_string())),
        )
        .unwrap();
        assert_eq!(lookup(runtime.env(), "ANTHROPIC_AUTH_TOKEN"), Some(secret));
        assert!(!format!("{runtime:?}").contains(secret));
        assert!(!format!("{runtime:#?}").contains(secret));
        assert!(!format!("{plan:?}").contains(secret));
        assert!(!serde_json::to_string(&plan).unwrap().contains(secret));
    }

    #[test]
    fn kimi_route_without_a_stored_credential_fails_the_launch() {
        let store = FakeSecretStore(None);
        let error = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::Kimi, Backend::Claude),
            Vec::new(),
            &store,
        )
        .unwrap_err();
        // Every descriptor-backed provider shares one secret-free,
        // provider-neutral credential error.
        assert!(matches!(&error, BridgeError::AnthropicCompatCredentials));
    }

    #[test]
    fn kimi_subprocess_and_pty_receive_the_same_secret_child_overlay() {
        let mut env = vec![("ANTHROPIC_API_KEY".to_string(), "ambient-key".to_string())];
        crate::route_plan::render_direct_env(
            &direct_route(kimi_descriptor(), None, &[], &env),
            "kimi-test-secret",
        )
        .apply(&mut env);
        let runtime = ForegroundRuntime {
            env,
            bridge: None,
            claude_settings: None,
            startup_notices: Vec::new(),
        };
        let adapter = RecordingAdapter::default();
        runtime
            .spawn_with(&adapter, SpawnMode::Subprocess, vec!["claude".into()], None)
            .unwrap();
        runtime
            .spawn_with(&adapter, SpawnMode::Pty, vec!["claude".into()], None)
            .unwrap();

        let calls = adapter.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].2, calls[1].2);
        assert_eq!(
            lookup(&calls[0].2, "ANTHROPIC_AUTH_TOKEN"),
            Some("kimi-test-secret")
        );
        assert_eq!(lookup(&calls[0].2, "ANTHROPIC_API_KEY"), None);
        assert!(!runtime.has_bridge());
    }

    fn openrouter_descriptor() -> &'static crate::provider_registry::AnthropicCompatProvider {
        crate::provider_registry::descriptor_for(ModelProvider::OpenRouter)
            .expect("OpenRouter must have an Anthropic-compat descriptor")
    }

    #[test]
    fn golden_openrouter_overlay_uses_documented_claude_gateway_profile() {
        let base = vec![
            (
                "ANTHROPIC_API_KEY".to_string(),
                "ambient-anthropic".to_string(),
            ),
            (
                "OPENROUTER_API_KEY".to_string(),
                "ambient-openrouter".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_FABLE_MODEL".to_string(),
                "ambient-fable".to_string(),
            ),
            (
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(),
                "0".to_string(),
            ),
        ];
        let mut child = base.clone();
        crate::route_plan::render_direct_env(
            &direct_route(openrouter_descriptor(), None, &[], &child),
            "openrouter-vault-secret",
        )
        .apply(&mut child);

        assert_eq!(
            lookup(&child, "ANTHROPIC_BASE_URL"),
            Some("https://openrouter.ai/api")
        );
        assert_eq!(
            lookup(&child, "ANTHROPIC_AUTH_TOKEN"),
            Some("openrouter-vault-secret")
        );
        assert_eq!(lookup(&child, "ANTHROPIC_API_KEY"), Some(""));
        assert_eq!(lookup(&child, "OPENROUTER_API_KEY"), None);
        assert_eq!(
            lookup(&child, "ANTHROPIC_MODEL"),
            Some("~anthropic/claude-sonnet-latest")
        );
        assert_eq!(
            lookup(&child, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some("~anthropic/claude-opus-latest")
        );
        assert_eq!(
            lookup(&child, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some("~anthropic/claude-sonnet-latest")
        );
        assert_eq!(
            lookup(&child, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some("~anthropic/claude-haiku-latest")
        );
        assert_eq!(
            lookup(&child, "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("~anthropic/claude-opus-latest")
        );
        assert_eq!(
            lookup(&child, "ANTHROPIC_DEFAULT_FABLE_MODEL"),
            Some("~anthropic/claude-fable-latest")
        );
        assert_eq!(
            lookup(&child, "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some("1")
        );
        // The default wire ID has a served datasheet row (#1258): OpenRouter
        // advertises a 1M window for it, so the harness must not clamp at its
        // 200k unknown-model default.
        assert_eq!(
            lookup(&child, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some("1000000")
        );
        assert_eq!(
            lookup(&base, "ANTHROPIC_API_KEY"),
            Some("ambient-anthropic")
        );
    }

    fn openrouter_selection(model: &str) -> crate::provider_catalog::ResolvedModelSelection {
        crate::provider_catalog::resolve(Some(ModelProvider::OpenRouter), Some(model), None, None)
            .expect("model must parse")
            .expect("an explicit model always resolves to a selection")
    }

    /// #1258: the reported bug — a live OpenRouter ID in neither catalog
    /// source must still teach the harness its exact window at launch, so
    /// auto-compact stops clamping a 1M model at 200k.
    #[test]
    fn direct_openrouter_launch_emits_the_served_window_for_an_uncataloged_model() {
        let mut plan = plan(ModelProvider::OpenRouter, Backend::Claude);
        plan.model_selection = Some(openrouter_selection("xiaomi/mimo-v2.6-flash"));
        let store = FakeSecretStore(Some("openrouter-routing-secret".to_string()));
        let runtime =
            ForegroundRuntime::start_with_secret_store(&plan, Vec::new(), &store).unwrap();
        // Expected window read from the served seed, which the scheduled
        // refresh rewrites; the invariant is that it is emitted verbatim.
        let served = crate::server_settings::effective_context_window("xiaomi/mimo-v2.6-flash")
            .expect("seed covers the reported model")
            .to_string();
        assert_eq!(
            lookup(runtime.env(), "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some(served.as_str())
        );
        // The compact threshold stays unset on this route: with the real
        // window learned, the harness derives its own threshold (#1258).
        assert_eq!(
            lookup(runtime.env(), "CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            None
        );
    }

    /// #1675: the record built from the real child env equals the dry-run
    /// preview, with and without an ambient override.
    #[test]
    fn launch_context_record_matches_the_dry_run_preview_on_the_direct_route() {
        use crate::launch_context::Source;
        let mut plan = plan(ModelProvider::OpenRouter, Backend::Claude);
        plan.model_selection = Some(openrouter_selection("xiaomi/mimo-v2.6-flash"));
        let store = FakeSecretStore(Some("openrouter-routing-secret".to_string()));
        let facts = launch_context_plan_facts(&plan);
        assert_eq!(facts.route, crate::launch_context::Route::Direct);
        for (ambient, source) in [
            (Vec::new(), Source::Served),
            (
                vec![(
                    "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(),
                    "4242".to_string(),
                )],
                Source::Ambient,
            ),
        ] {
            let runtime =
                ForegroundRuntime::start_with_secret_store(&plan, ambient.clone(), &store).unwrap();
            let launched = facts.at_launch(&ambient, runtime.env(), 0);
            assert_eq!(launched, facts.preview(&ambient));
            assert_eq!(launched.max_context_tokens.source, source);
            let text = serde_json::to_string(&launched).unwrap();
            assert!(!text.contains("openrouter-routing-secret"));
        }
    }

    #[test]
    fn anthropic_compat_overlay_preserves_an_ambient_max_context_window() {
        let mut env = vec![(
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(),
            "4242".to_string(),
        )];
        crate::route_plan::render_direct_env(
            &direct_route(
                openrouter_descriptor(),
                Some(&openrouter_selection("xiaomi/mimo-v2.6-flash")),
                &[],
                &env,
            ),
            "openrouter-vault-secret",
        )
        .apply(&mut env);
        // An explicit user-set window is never clobbered (DD-059's spirit).
        assert_eq!(lookup(&env, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"), Some("4242"));
    }

    /// A synthetic one-row catalog for the #1528 injection tests, so the
    /// decision is driven by an artifact shape rather than the real 458-row
    /// embedded copy or a network read.
    fn per_turn_effort_catalog(wire_model: &str, supports_reasoning_effort: bool) -> String {
        serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-09-27T00:00:00Z",
            "source": "https://openrouter.ai/api/v1/models",
            "models": [{
                "id": wire_model,
                "name": "Fixture Row",
                "provider": wire_model.split('/').next().unwrap(),
                "context_length": 131_072,
                "input_price_per_token": 1e-7,
                "output_price_per_token": 1e-6,
                "cached_input_price_per_token": 1e-7,
                "supports_tools": true,
                "supports_text_input": true,
                "supports_text_output": true,
                "supports_reasoning": true,
                "supports_reasoning_effort": supports_reasoning_effort,
                "reasoning": {
                    "supported_efforts": ["max", "high", "low"],
                    "default_effort": "high",
                    "default_enabled": true,
                    "mandatory": false
                },
                "supports_vision": false,
                "eligible_for_coding": true,
                "ineligibility_reasons": []
            }]
        })
        .to_string()
    }

    fn catalog_from(json: &str) -> crate::openrouter_catalog::Catalog {
        crate::openrouter_catalog::Catalog::parse(json.as_bytes()).expect("fixture catalog parses")
    }

    #[test]
    fn per_turn_effort_capability_maps_the_openrouter_gate_to_the_harness_name() {
        let wire = "deepseek/deepseek-v4.1-flash";
        let gated = catalog_from(&per_turn_effort_catalog(wire, true));
        assert_eq!(
            per_turn_effort_capability(&gated, wire, true),
            Some(format!("{wire}=per_turn_effort"))
        );
        // The flip is off: the ship-dark state injects nothing even for a row
        // OpenRouter admits.
        assert_eq!(per_turn_effort_capability(&gated, wire, false), None);
        // A row without the `reasoning_effort` token is never advertised.
        let ungated = catalog_from(&per_turn_effort_catalog(wire, false));
        assert_eq!(per_turn_effort_capability(&ungated, wire, true), None);
        // Exact wire-ID match: no same-family fallback, no `[1m]` stripping.
        assert_eq!(
            per_turn_effort_capability(&gated, &format!("{wire}[1m]"), true),
            None
        );
        assert_eq!(
            per_turn_effort_capability(&gated, "other/model", true),
            None
        );
    }

    /// The overlay itself is dark today (the built-in flip is `false`), and it
    /// must not have started emitting the key on any route.
    #[test]
    fn anthropic_compat_overlay_emits_no_capabilities_while_the_flip_ships_dark() {
        for descriptor in [
            openrouter_descriptor(),
            deepseek_descriptor(),
            kimi_descriptor(),
        ] {
            let mut env = Vec::new();
            crate::route_plan::render_direct_env(
                &direct_route(descriptor, None, &[], &env),
                "secret",
            )
            .apply(&mut env);
            assert_eq!(
                lookup(&env, "CLAUDE_CODE_MODEL_CAPABILITIES"),
                None,
                "{}",
                descriptor.display_name
            );
        }
    }

    #[test]
    fn anthropic_compat_overlay_emits_no_window_for_an_id_in_neither_source() {
        let mut env = Vec::new();
        crate::route_plan::render_direct_env(
            &direct_route(
                openrouter_descriptor(),
                Some(&openrouter_selection("nonexistent/model-v9")),
                &[],
                &env,
            ),
            "openrouter-vault-secret",
        )
        .apply(&mut env);
        assert_eq!(lookup(&env, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"), None);
        assert_eq!(lookup(&env, "CLAUDE_CODE_AUTO_COMPACT_WINDOW"), None);
    }

    #[test]
    fn openrouter_direct_route_creates_no_bridge_and_requires_its_vault_secret() {
        let store = FakeSecretStore(Some("openrouter-routing-secret".to_string()));
        let runtime = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::OpenRouter, Backend::Claude),
            Vec::new(),
            &store,
        )
        .unwrap();
        assert!(!runtime.has_bridge());
        assert_eq!(runtime.socket_addr(), None);
        assert_eq!(
            lookup(runtime.env(), "ANTHROPIC_AUTH_TOKEN"),
            Some("openrouter-routing-secret")
        );
    }

    #[test]
    fn openrouter_route_without_a_stored_credential_reports_a_provider_neutral_error() {
        let error = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::OpenRouter, Backend::Claude),
            Vec::new(),
            &FakeSecretStore(None),
        )
        .unwrap_err();
        assert!(matches!(&error, BridgeError::AnthropicCompatCredentials));
        assert!(!error.to_string().contains("DeepSeek"));
    }

    /// `launch_preflight_target` (provider_auth.rs) and `PreflightError`
    /// are already provider-neutral: `launch_preflight_target` is a plain
    /// registry lookup, and `PreflightError::describe` takes the descriptor
    /// as a parameter rather than hardcoding a provider. Both are exercised
    /// against DeepSeek in `provider_auth.rs`'s own test module (which this
    /// lane does not own), so this only pins that the same generic surface
    /// resolves correctly for Kimi's descriptor once it lands: the
    /// dry-run-is-vault-free guarantee, and that the actionable message
    /// names `clud auth login kimi`. The interactive
    /// prompt/store/continue, empty/cancel, and non-interactive
    /// no-stdin-read mechanics themselves live in `preflight_with`, a
    /// private fn in `provider_auth.rs` that takes no `ModelProvider`
    /// parameter at all (only an injected `SecretStore` + closure) -- so
    /// DeepSeek's existing coverage of that fn already proves Kimi's
    /// identical behavior; there is no Kimi-specific mechanism to
    /// duplicate here.
    #[test]
    fn kimi_preflight_target_resolves_to_kimis_descriptor_and_is_vault_free_on_dry_run() {
        assert_eq!(
            crate::provider_auth::launch_preflight_target(ModelProvider::Kimi, false),
            Some(kimi_descriptor())
        );
        assert_eq!(
            crate::provider_auth::launch_preflight_target(ModelProvider::Kimi, true),
            None,
            "dry run must never resolve a descriptor to preflight against, for any provider"
        );
    }

    #[test]
    fn kimi_preflight_error_names_kimi_and_its_login_command() {
        let descriptor = kimi_descriptor();
        assert_eq!(
            crate::provider_auth::PreflightError::Missing.describe(descriptor),
            "Kimi credentials are not configured; run `clud auth login kimi`"
        );
        assert_eq!(
            crate::provider_auth::PreflightError::Cancelled.describe(descriptor),
            "Kimi credential entry was cancelled"
        );
    }

    // -----------------------------------------------------------------
    // #1257: a launch-time pin -- typed or inherited -- constrains every
    // slot (haiku, subagent, fable) and the rows discovery may advertise,
    // not just the main conversation model.
    // -----------------------------------------------------------------

    #[test]
    fn provider_only_hands_openrouter_routing_to_the_harness() {
        let only = vec!["parasail/fp8".to_string()];
        let mut env = vec![(
            "CLAUDE_CODE_EXTRA_BODY".to_string(),
            r#"{"top_k":5}"#.to_string(),
        )];
        let mut plan = plan(ModelProvider::OpenRouter, Backend::Claude);
        plan.provider_only = only.clone();
        let routing = crate::route_plan::provider_routing(&plan);
        apply_upstream_routing(&mut env, openrouter_descriptor(), routing.as_ref()).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(lookup(&env, "CLAUDE_CODE_EXTRA_BODY").unwrap()).unwrap();
        assert_eq!(body["top_k"], 5);
        assert_eq!(
            body["provider"],
            serde_json::json!({"only": ["parasail/fp8"], "allow_fallbacks": false})
        );
        // No flag, or another provider: the environment is untouched.
        let mut untouched = Vec::new();
        apply_upstream_routing(&mut untouched, openrouter_descriptor(), None).unwrap();
        apply_upstream_routing(&mut untouched, kimi_descriptor(), routing.as_ref()).unwrap();
        assert!(untouched.is_empty());
    }

    #[test]
    fn a_pinned_openrouter_overlay_pins_every_slot_and_turns_discovery_off() {
        let pin = "~anthropic/claude-sonnet-latest";
        let base = vec![
            (
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(),
                "1".to_string(),
            ),
            (
                "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
                "ambient-not-in-list".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                "ambient-opus".to_string(),
            ),
        ];
        let mut child = base.clone();
        crate::route_plan::render_direct_env(
            &direct_route(openrouter_descriptor(), None, &[pin.to_string()], &child),
            "openrouter-vault-secret",
        )
        .apply(&mut child);
        for key in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ] {
            assert_eq!(lookup(&child, key), Some(pin), "{key} must be the pin");
        }
        // The ambient subagent is outside the allowlist, so the pin wins.
        // Discovery never comes back: the scrub removed it and a constrained
        // launch does not re-request it (DD-054 -- discovery only adds rows,
        // and this launch asked for no rows).
        assert_eq!(
            lookup(&child, "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            None
        );
        // The parent environment is never mutated.
        assert_eq!(
            lookup(&base, "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some("1")
        );
        assert_eq!(
            lookup(&base, "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("ambient-not-in-list")
        );
    }

    #[test]
    fn an_ambient_subagent_wins_the_slot_only_when_the_allowlist_admits_it() {
        let allowed = vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()];
        // Admitted: the user's own subagent rides inside the boundary, while
        // the main slots stay on the launch selection.
        let mut admitted = vec![(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "deepseek-v4-pro".to_string(),
        )];
        crate::route_plan::render_direct_env(
            &direct_route(deepseek_descriptor(), None, &allowed, &admitted),
            "ds-test-secret",
        )
        .apply(&mut admitted);
        assert_eq!(
            lookup(&admitted, "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("deepseek-v4-pro")
        );
        assert_eq!(lookup(&admitted, "ANTHROPIC_MODEL"), Some("deepseek-flash"));
        // Rejected: the pin covers the subagent slot too.
        let mut rejected = vec![(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "claude-opus-5".to_string(),
        )];
        crate::route_plan::render_direct_env(
            &direct_route(deepseek_descriptor(), None, &allowed, &rejected),
            "ds-test-secret",
        )
        .apply(&mut rejected);
        assert_eq!(
            lookup(&rejected, "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("deepseek-flash"),
            "an ambient subagent outside the allowlist must not reach the child"
        );
    }

    #[test]
    fn an_inherited_pin_is_announced_green_on_a_tty_and_plain_otherwise() {
        let mut route = plan(ModelProvider::OpenRouter, Backend::Claude);
        // Nothing pinned: nothing announced.
        assert!(model_pin_notices(&route, true).is_empty());
        // A boundary the user typed: no announcement, they just typed it.
        route.allowed_models = vec!["xiaomi/mimo-v2.6-flash".to_string()];
        assert!(model_pin_notices(&route, false).is_empty());
        // Inherited from the previous selection: announce exactly this case.
        route.pinned_from_previous_selection = true;
        let green = model_pin_notices(&route, true);
        assert_eq!(green.len(), 1);
        assert!(
            green[0].starts_with("\x1b[32m") && green[0].ends_with("\x1b[0m"),
            "{}",
            green[0]
        );
        assert!(
            green[0].contains(
                "[clud] info: no --model given; pinned to previous model selection: \
                 xiaomi/mimo-v2.6-flash"
            ),
            "{}",
            green[0]
        );
        // Plain text when stderr is not a terminal, so logs stay parseable.
        assert_eq!(
            model_pin_notices(&route, false),
            vec![
                "[clud] info: no --model given; pinned to previous model selection: \
                 xiaomi/mimo-v2.6-flash"
                    .to_string()
            ]
        );
        // An inherited pin with an empty allowlist pins nothing: say nothing.
        route.allowed_models.clear();
        assert!(model_pin_notices(&route, true).is_empty());
    }

    #[test]
    fn an_inherited_pin_leads_the_startup_notices() {
        let mut route = plan(ModelProvider::Claude, Backend::Claude);
        route.allowed_models = vec!["claude-opus".to_string()];
        route.pinned_from_previous_selection = true;
        let runtime = ForegroundRuntime::start(&route, Vec::new()).unwrap();
        let first = runtime
            .startup_notices
            .first()
            .expect("an inherited pin must be announced first");
        assert!(
            first.contains("no --model given; pinned to previous model selection: claude-opus"),
            "{first}"
        );
    }

    #[test]
    fn a_pinned_gateway_launch_announces_that_discovery_is_off() {
        let mut route = plan(ModelProvider::OpenRouter, Backend::Claude);
        route.allowed_models = vec!["~anthropic/claude-sonnet-latest".to_string()];
        let runtime = ForegroundRuntime::start_with_secret_store(
            &route,
            Vec::new(),
            &FakeSecretStore(Some("openrouter-vault-secret".to_string())),
        )
        .unwrap();
        assert!(runtime.startup_notices.iter().any(|notice| notice.contains(
            "gateway model discovery is off for this launch; models are pinned to: \
             ~anthropic/claude-sonnet-latest"
        )));
        // The child env agrees: a constrained direct launch never asks for
        // discovery, so the picker only shows in-boundary built-in rows.
        assert_eq!(
            lookup(runtime.env(), "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            None
        );
        // An unconstrained launch keeps discovery and says nothing about it.
        let plain = ForegroundRuntime::start_with_secret_store(
            &plan(ModelProvider::OpenRouter, Backend::Claude),
            Vec::new(),
            &FakeSecretStore(Some("openrouter-vault-secret".to_string())),
        )
        .unwrap();
        assert!(plain
            .startup_notices
            .iter()
            .all(|notice| !notice.contains("discovery is off")));
        assert_eq!(
            lookup(plain.env(), "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some("1")
        );
    }

    #[test]
    fn unified_overlay_pins_every_slot_to_the_pinned_discovery_id() {
        let mut route = plan(ModelProvider::Claude, Backend::Claude);
        route.routing_mode = RoutingMode::Unified;
        route.allowed_models = vec!["deepseek-flash".to_string()];
        // The ambient subagent is outside the boundary: the pin wins.
        let base = vec![(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "claude-haiku-4-5-20251001".to_string(),
        )];
        let runtime = ForegroundRuntime::start_with_secret_store(
            &route,
            base.clone(),
            &FakeSecretStore(Some("deepseek-test-secret".to_string())),
        )
        .unwrap();
        let env = runtime.env();
        for key in [
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ] {
            assert_eq!(
                lookup(env, key),
                Some("clud-claude-deepseek-flash"),
                "{key}"
            );
        }
        // Discovery stays ON here: clud proxies and filters the catalog, so
        // every row the harness merges is already inside the boundary.
        assert_eq!(
            lookup(env, "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some("1")
        );
        assert_eq!(
            lookup(&base, "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("claude-haiku-4-5-20251001"),
            "the parent environment is never mutated"
        );
    }

    #[test]
    fn unified_overlay_lets_an_admitted_ambient_subagent_win() {
        let mut route = plan(ModelProvider::Claude, Backend::Claude);
        route.routing_mode = RoutingMode::Unified;
        route.allowed_models = vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()];
        let base = vec![(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "deepseek-v4-pro".to_string(),
        )];
        let runtime = ForegroundRuntime::start_with_secret_store(
            &route,
            base,
            &FakeSecretStore(Some("deepseek-test-secret".to_string())),
        )
        .unwrap();
        assert_eq!(
            lookup(runtime.env(), "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("deepseek-v4-pro")
        );
        assert_eq!(
            lookup(runtime.env(), "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some("clud-claude-deepseek-flash"),
            "the main slots stay on the launch selection (first allowed entry)"
        );
    }

    #[test]
    fn codex_via_claude_folds_its_role_rows_into_the_boundary_without_dupes() {
        // Unconstrained stays unconstrained: no boundary, no injected rows.
        let mut route = plan(ModelProvider::Codex, Backend::Claude);
        assert!(crate::route_plan::codex_bridge_allowlist(&route).is_empty());
        // A pin gains the route's own injected role rows (DD-038), or the
        // bridge would refuse clud's own configuration.
        route.allowed_models = vec!["gpt-5.6-luna".to_string()];
        assert_eq!(
            crate::route_plan::codex_bridge_allowlist(&route),
            vec![
                "gpt-5.6-luna".to_string(),
                CODEX_VIA_CLAUDE_OPUS_MODEL.to_string(),
                CODEX_VIA_CLAUDE_SONNET_MODEL.to_string(),
            ]
        );
        // Already present (in any case): not duplicated.
        route.allowed_models = vec![
            CODEX_VIA_CLAUDE_SONNET_MODEL.to_string(),
            "gpt-5.6-luna".to_string(),
        ];
        assert_eq!(
            crate::route_plan::codex_bridge_allowlist(&route),
            vec![
                CODEX_VIA_CLAUDE_SONNET_MODEL.to_string(),
                "gpt-5.6-luna".to_string(),
                CODEX_VIA_CLAUDE_OPUS_MODEL.to_string(),
            ]
        );
    }

    // -----------------------------------------------------------------
    // #967 Phase 2b: declared hooks compiled into `--settings`.
    // -----------------------------------------------------------------

    fn repo_declaring(hooks: &str) -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".clud")).unwrap();
        std::fs::write(tmp.path().join(".clud").join("hooks.json"), hooks).unwrap();
        tmp
    }

    fn plan_in(provider: ModelProvider, harness: Backend, cwd: &std::path::Path) -> LaunchPlan {
        let mut plan = plan(provider, harness);
        plan.cwd = Some(cwd.to_string_lossy().into_owned());
        plan
    }

    /// Every file under `root`, with its bytes — for proving a launch wrote
    /// nothing (DD-049).
    fn snapshot(root: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.insert(path, bytes);
                }
            }
        }
        out
    }

    /// The argv clud would actually spawn, via the recording adapter.
    fn composed_command(runtime: &ForegroundRuntime, command: Vec<String>) -> Vec<String> {
        let adapter = RecordingAdapter::default();
        runtime
            .spawn_with(&adapter, SpawnMode::Subprocess, command, None)
            .unwrap();
        let calls = adapter.calls.borrow();
        calls[0].1.clone()
    }

    fn settings_argument(runtime: &ForegroundRuntime) -> Option<String> {
        let command = composed_command(runtime, vec!["claude".to_string()]);
        let index = command
            .iter()
            .position(|argument| argument == "--settings")?;
        command.get(index + 1).cloned()
    }

    #[test]
    fn older_claude_receives_effort_as_settings_for_every_spawn_mode() {
        let mut launch = plan(ModelProvider::Claude, Backend::Claude);
        launch.command = vec!["claude".into(), "--effort".into(), "low".into()];
        let mut runtime = ForegroundRuntime::start(&launch, Vec::new()).unwrap();
        runtime.apply_initial_effort(&launch).unwrap();
        let path = settings_argument(&runtime).unwrap();
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(settings["effortLevel"], "low");
        for mode in [SpawnMode::Subprocess, SpawnMode::Pty] {
            let adapter = RecordingAdapter::default();
            runtime
                .spawn_with(&adapter, mode, launch.command.clone(), None)
                .unwrap();
            let calls = adapter.calls.borrow();
            assert!(!calls[0].1.iter().any(|arg| arg == "--effort"));
            assert!(calls[0].1.iter().any(|arg| arg == "--settings"));
        }
        let dry = dry_run_claude_command(&launch).unwrap();
        assert!(!dry.iter().any(|arg| arg == "--effort"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&dry[2]).unwrap()["effortLevel"],
            "low"
        );
    }

    #[test]
    fn a_plain_claude_launch_carries_declared_hooks_as_settings() {
        // The ungating: before Phase 2b only bridge routes composed settings,
        // so a plain launch registered nothing.
        let repo = repo_declaring(r#"{"hooks":{"Stop":[{"command":"guard"}]}}"#);
        let runtime = ForegroundRuntime::start(
            &plan_in(ModelProvider::Claude, Backend::Claude, repo.path()),
            Vec::new(),
        )
        .unwrap();

        let path = settings_argument(&runtime).expect("--settings injected");
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["hooks"]["Stop"][0]["hooks"][0]["command"],
            "clud-cmd-scan --event Stop"
        );
        assert!(lookup(runtime.env(), crate::clud_hooks_compile::DISPATCH_ENV).is_some());
    }

    /// #1962/#1971: a client that knows `WorktreeCreate` gets clud's line, so
    /// its worktrees land under `~/.clud/worktrees`; one that does not gets no
    /// line at all, since an unknown event could void the whole document.
    #[test]
    fn worktree_hooks_are_registered_only_when_the_client_takes_them() {
        let launch = plan(ModelProvider::Claude, Backend::Claude);
        let (with, _) =
            compose_launch_settings_document_with(&launch, serde_json::json!({}), true).unwrap();
        assert_eq!(
            with["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
            "clud-cmd-scan --event WorktreeCreate"
        );
        assert_eq!(
            with["hooks"]["WorktreeRemove"][0]["hooks"][0]["command"],
            "clud-cmd-scan --event WorktreeRemove"
        );
        assert!(with["hooks"]["PreToolUse"].is_array(), "the guards stay");
        let (without, _) =
            compose_launch_settings_document_with(&launch, serde_json::json!({}), false).unwrap();
        assert!(without["hooks"].get("WorktreeCreate").is_none());
    }

    #[test]
    fn unsafe_claude_settings_keep_user_hooks_and_settings_authority() {
        let mut launch = plan(ModelProvider::Claude, Backend::Claude);
        launch.unsafe_mode = true;
        let user_hook = serde_json::json!({
            "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "user-check"}]}]}
        });
        let (settings, _) = compose_launch_settings_document(&launch, user_hook).unwrap();
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "user-check"
        );
        assert_eq!(
            settings["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
            crate::clud_hooks_compile::dispatcher_command(crate::clud_hooks_compile::PRE_TOOL_USE)
        );
        assert_eq!(
            settings["permissions"]["deny"],
            serde_json::json!(crate::clud_hooks_compile::SETTINGS_FILE_DENIES)
        );
    }

    #[test]
    fn plan_mode_opt_out_is_launch_scoped_even_with_an_inherited_override() {
        for allowed in [false, true] {
            let mut launch = plan(ModelProvider::Claude, Backend::Claude);
            launch.allow_plan_mode = allowed;
            let inherited = vec![(
                crate::loop_plan_mode::ALLOW_ENV.to_string(),
                if allowed { "0" } else { "1" }.to_string(),
            )];
            let runtime = ForegroundRuntime::start(&launch, inherited).unwrap();
            assert_eq!(
                lookup(runtime.env(), crate::loop_plan_mode::ALLOW_ENV),
                Some(if allowed { "1" } else { "0" })
            );
        }
    }

    #[test]
    fn a_repo_that_declares_nothing_gets_deletion_guard_and_history_hooks() {
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();

        // `--coauthor` opts back in to Claude Code's own attribution. What is
        // left in `--settings` is clud's own session-history hooks (#922):
        // no repo-declared hook, and no dispatcher env.
        let mut plan = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        plan.coauthor = crate::attribution::Coauthor::Harness;
        let runtime = ForegroundRuntime::start(&plan, Vec::new()).unwrap();

        let path = settings_argument(&runtime).expect("session-history hooks");
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let hooks = document["hooks"].as_object().unwrap();
        let mut expected: std::collections::BTreeSet<String> = crate::session_history::hook::EVENTS
            .iter()
            .map(|e| e.to_string())
            .collect();
        expected.insert("PreToolUse".to_string());
        // #1984: the user's prompt may allow worktrees for the session.
        expected.insert(crate::worktree_consent::USER_PROMPT_SUBMIT_EVENT.to_string());
        expected.extend([
            "PostToolUse".to_string(),
            "PostToolUseFailure".to_string(),
            "Stop".to_string(),
            "SessionEnd".to_string(),
        ]);
        assert_eq!(
            hooks
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        for (event, tool) in [
            ("PreToolUse", "StructuredOutput"),
            ("PostToolUse", "Bash"),
            ("PostToolUseFailure", "Bash"),
        ] {
            assert!(hooks[event].as_array().unwrap().iter().any(|entry| {
                entry["matcher"]
                    .as_str()
                    .is_some_and(|matcher| matcher.split('|').any(|candidate| candidate == tool))
            }));
        }
        for entries in hooks.values() {
            let command = entries[0]["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.contains(" session-hook ") || command.contains("clud-cmd-scan"),
                "{command}"
            );
        }
        assert!(document.get("attribution").is_none());
        assert!(lookup(runtime.env(), crate::clud_hooks_compile::DISPATCH_ENV).is_none());
    }

    /// #1317: by default a Claude launch hides the co-author trailer and the
    /// PR line through its launch `--settings`.
    #[test]
    fn a_default_claude_launch_hides_attribution() {
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();

        let runtime = ForegroundRuntime::start(
            &plan_in(ModelProvider::Claude, Backend::Claude, repo.path()),
            Vec::new(),
        )
        .unwrap();

        let path = settings_argument(&runtime).expect("--settings injected");
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["attribution"],
            serde_json::json!({"commit": "", "pr": ""})
        );
    }

    #[test]
    fn a_coauthor_tag_replaces_both_attribution_strings() {
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let mut plan = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        plan.coauthor = crate::attribution::Coauthor::Tag("Made with clud".to_string());

        let runtime = ForegroundRuntime::start(&plan, Vec::new()).unwrap();

        let path = settings_argument(&runtime).expect("--settings injected");
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["attribution"],
            serde_json::json!({"commit": "Made with clud", "pr": "Made with clud"})
        );
    }

    #[test]
    fn a_launch_modifies_no_file_in_the_repo() {
        // DD-049: settings reach the harness as arguments, never as writes.
        let repo = repo_declaring(r#"{"hooks":{"Stop":[{"command":"guard"}]}}"#);
        let before = snapshot(repo.path());

        let runtime = ForegroundRuntime::start(
            &plan_in(ModelProvider::Claude, Backend::Claude, repo.path()),
            Vec::new(),
        )
        .unwrap();
        let _ = settings_argument(&runtime);

        assert_eq!(
            snapshot(repo.path()),
            before,
            "the launch wrote to the repo"
        );
    }

    #[test]
    fn a_user_supplied_settings_argument_is_merged_not_shadowed() {
        // Claude accepts one `--settings`; a second would shadow the first.
        let repo = repo_declaring(r#"{"hooks":{"Stop":[{"command":"guard"}]}}"#);
        let user = repo.path().join("mine.json");
        std::fs::write(
            &user,
            r#"{"model":"theirs","hooks":{"Stop":[{"hooks":[{"command":"user-hook"}]}]}}"#,
        )
        .unwrap();

        let mut launch_plan = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        launch_plan.command = vec![
            "claude".to_string(),
            "--settings".to_string(),
            user.to_string_lossy().into_owned(),
        ];
        let runtime = ForegroundRuntime::start(&launch_plan, Vec::new()).unwrap();

        let composed = composed_command(&runtime, launch_plan.command.clone());
        let occurrences = composed
            .iter()
            .filter(|argument| *argument == "--settings")
            .count();
        assert_eq!(occurrences, 1, "exactly one source survives: {composed:?}");

        let index = composed
            .iter()
            .position(|argument| argument == "--settings")
            .unwrap();
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&composed[index + 1]).unwrap()).unwrap();
        assert_eq!(document["model"], "theirs", "user keys survive");
        let commands: Vec<&str> = document["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|group| group["hooks"][0]["command"].as_str())
            .collect();
        assert!(commands.contains(&"user-hook"), "{commands:?}");
        assert!(
            commands.contains(&"clud-cmd-scan --event Stop"),
            "{commands:?}"
        );
    }

    // -----------------------------------------------------------------
    // #967 Phase 3b: roots harvested at launch.
    // -----------------------------------------------------------------

    fn hook_roots_env(runtime: &ForegroundRuntime) -> Option<String> {
        lookup(runtime.env(), crate::clud_hook_roots::HOOK_ROOTS_ENV).map(ToOwned::to_owned)
    }

    fn roots_in(encoded: &str) -> Vec<String> {
        serde_json::from_str::<serde_json::Value>(encoded)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn add_dir_targets_are_harvested_into_the_registry() {
        // `--add-dir` reaches the harness as passthrough argv and appears in
        // no hook payload, so the launch is the only place that can see it.
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let extra = tempfile::TempDir::new().unwrap();

        let mut launch = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        launch.command = vec![
            "claude".to_string(),
            "--add-dir".to_string(),
            extra.path().to_string_lossy().into_owned(),
        ];
        let runtime = ForegroundRuntime::start(&launch, Vec::new()).unwrap();

        let encoded = hook_roots_env(&runtime).expect("roots carried to the hook");
        assert!(
            roots_in(&encoded)
                .iter()
                .any(|path| path.contains(extra.path().file_name().unwrap().to_str().unwrap())),
            "{encoded}"
        );
    }

    #[test]
    fn add_dir_accepts_both_spellings_and_several_directories() {
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();

        let mut launch = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        launch.command = vec![
            "claude".to_string(),
            "--add-dir".to_string(),
            first.path().to_string_lossy().into_owned(),
            second.path().to_string_lossy().into_owned(),
            "--verbose".to_string(),
        ];
        let runtime = ForegroundRuntime::start(&launch, Vec::new()).unwrap();
        assert_eq!(
            roots_in(&hook_roots_env(&runtime).expect("roots")).len(),
            2,
            "the flag takes directories until the next flag"
        );

        launch.command = vec![
            "claude".to_string(),
            format!("--add-dir={}", first.path().to_string_lossy()),
        ];
        let joined = ForegroundRuntime::start(&launch, Vec::new()).unwrap();
        assert_eq!(roots_in(&hook_roots_env(&joined).expect("roots")).len(), 1);
    }

    #[test]
    fn tokens_after_a_bare_separator_are_not_harness_flags() {
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let extra = tempfile::TempDir::new().unwrap();

        let mut launch = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        launch.command = vec![
            "claude".to_string(),
            "--".to_string(),
            "--add-dir".to_string(),
            extra.path().to_string_lossy().into_owned(),
        ];
        let runtime = ForegroundRuntime::start(&launch, Vec::new()).unwrap();

        assert_eq!(hook_roots_env(&runtime), None);
    }

    #[test]
    fn additional_directories_from_claude_settings_are_harvested_too() {
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        std::fs::create_dir_all(repo.path().join(".claude")).unwrap();
        std::fs::write(
            repo.path().join(".claude").join("settings.json"),
            r#"{"permissions":{"additionalDirectories":["../sibling"]}}"#,
        )
        .unwrap();

        let runtime = ForegroundRuntime::start(
            &plan_in(ModelProvider::Claude, Backend::Claude, repo.path()),
            Vec::new(),
        )
        .unwrap();

        let encoded = hook_roots_env(&runtime).expect("roots");
        assert!(
            roots_in(&encoded)
                .iter()
                .any(|path| path.contains("sibling")),
            "{encoded}"
        );
    }

    #[test]
    fn a_launch_that_grants_nothing_leaves_the_env_alone() {
        // The hook can work the rest out for itself; an empty registry would
        // be noise the child has to parse on every tool call.
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();

        let runtime = ForegroundRuntime::start(
            &plan_in(ModelProvider::Claude, Backend::Claude, repo.path()),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(hook_roots_env(&runtime), None);
    }

    #[test]
    fn harvested_directories_are_registered_as_extern() {
        // A granted sibling is no more the parent's business than a checkout
        // clud cloned: its project guards would misfire there (#841). The two
        // differ in trust, not in firing, which Phase 4 has to separate.
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let extra = tempfile::TempDir::new().unwrap();

        let mut launch = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        launch.command = vec![
            "claude".to_string(),
            "--add-dir".to_string(),
            extra.path().to_string_lossy().into_owned(),
        ];
        let runtime = ForegroundRuntime::start(&launch, Vec::new()).unwrap();

        let encoded = hook_roots_env(&runtime).expect("roots");
        let document: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(document[0]["kind"], "extern");

        let roots = crate::clud_hook_roots::HookRoots::resolve(repo.path(), &[], Some(&encoded));
        assert!(
            !roots.parent_hooks_apply_to(&extra.path().join("file.rs")),
            "the parent's guards must not follow the agent into a granted directory"
        );
    }

    #[test]
    fn a_codex_harness_launch_gets_no_settings_because_codex_cannot_take_them() {
        // `-c` overrides config.toml; codex hooks live in a separate
        // hooks.json with no flag pointing at an alternate one.
        let repo = repo_declaring(r#"{"hooks":{"Stop":[{"command":"guard"}]}}"#);
        let runtime = ForegroundRuntime::start(
            &plan_in(ModelProvider::Codex, Backend::Codex, repo.path()),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(settings_argument(&runtime), None);
        assert!(lookup(runtime.env(), crate::clud_hooks_compile::DISPATCH_ENV).is_none());
    }

    #[test]
    fn only_a_launch_that_registers_the_prompt_hook_gates_worktrees() {
        // #1984: consent can only be recorded where clud's UserPromptSubmit
        // line is registered; everywhere else the gate must stay off, and an
        // inherited marker must not leak into a launch that has no prompt hook.
        let gate = crate::worktree_consent::GATE_ENV;
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let inherited = vec![(gate.to_string(), "1".to_string())];

        let claude = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        let runtime = ForegroundRuntime::start(&claude, Vec::new()).unwrap();
        assert_eq!(lookup(runtime.env(), gate), Some("1"));

        let mut unsafe_launch = claude.clone();
        unsafe_launch.unsafe_mode = true;
        let runtime = ForegroundRuntime::start(&unsafe_launch, inherited.clone()).unwrap();
        assert_eq!(lookup(runtime.env(), gate), None);

        let codex = plan_in(ModelProvider::Codex, Backend::Codex, repo.path());
        let runtime = ForegroundRuntime::start(&codex, inherited).unwrap();
        assert_eq!(lookup(runtime.env(), gate), None);
    }

    // ── #1189: Claude status-line injection ────────────────────────────────

    fn statusline_injection(dir: &Path) -> crate::toast::launch::StatuslineInjection {
        crate::toast::launch::StatuslineInjection {
            exe: PathBuf::from("/opt/clud/bin/clud"),
            session_pid: 4242,
            state_dir: dir.join("state"),
        }
    }

    fn settings_document(runtime: &ForegroundRuntime) -> Option<serde_json::Value> {
        runtime.claude_settings.as_ref().map(|settings| {
            serde_json::from_str(&std::fs::read_to_string(&settings.value).unwrap()).unwrap()
        })
    }

    fn chain_of(document: &serde_json::Value) -> Option<String> {
        let command = document["statusLine"]["command"].as_str()?;
        let encoded = command.split("--chain-b64 ").nth(1)?;
        crate::toast::statusline::decode_chain(encoded)
    }

    fn claude_plan_in(dir: &Path) -> LaunchPlan {
        let mut plan = plan(ModelProvider::Claude, Backend::Claude);
        plan.cwd = Some(dir.to_string_lossy().into_owned());
        plan
    }

    #[test]
    fn statusline_injection_gives_a_plain_claude_launch_a_settings_source() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let plan = claude_plan_in(dir.path());
        let mut runtime =
            ForegroundRuntime::start_with_secret_store(&plan, Vec::new(), &FakeSecretStore(None))
                .unwrap();
        assert!(
            runtime.claude_settings.is_some(),
            "deletion guard supplies settings"
        );

        runtime
            .inject_statusline(&plan, &statusline_injection(dir.path()), Some(home.path()))
            .unwrap();

        let document = settings_document(&runtime).expect("settings source created");
        let command = document["statusLine"]["command"].as_str().unwrap();
        assert!(
            command.contains(" statusline --session-pid 4242 "),
            "{command}"
        );
        assert_eq!(
            document["statusLine"]["refreshInterval"],
            crate::toast::statusline::REFRESH_INTERVAL_SECS
        );
        assert_eq!(chain_of(&document), None, "no user status line to chain");
        assert!(
            !runtime
                .claude_settings
                .as_ref()
                .unwrap()
                .replaces_user_argument
        );
    }

    #[test]
    fn statusline_injection_chains_the_status_line_from_the_users_own_settings_argument() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut plan = claude_plan_in(dir.path());
        plan.command.push("--settings".to_string());
        plan.command.push(
            r#"{"model":"opus","statusLine":{"type":"command","command":"~/bin/mine.sh","padding":1}}"#
                .to_string(),
        );
        let mut runtime =
            ForegroundRuntime::start_with_secret_store(&plan, Vec::new(), &FakeSecretStore(None))
                .unwrap();
        runtime
            .inject_statusline(&plan, &statusline_injection(dir.path()), Some(home.path()))
            .unwrap();

        let settings = runtime.claude_settings.as_ref().unwrap();
        assert!(
            settings.replaces_user_argument,
            "Claude accepts one --settings source"
        );
        let document = settings_document(&runtime).unwrap();
        assert_eq!(
            document["model"], "opus",
            "the user's other settings survive"
        );
        assert_eq!(document["statusLine"]["padding"], 1);
        assert_eq!(chain_of(&document).as_deref(), Some("~/bin/mine.sh"));
    }

    #[test]
    fn statusline_injection_chains_a_project_status_line() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude/settings.json"),
            r#"{"statusLine":{"type":"command","command":"npx ccstatusline"}}"#,
        )
        .unwrap();
        let plan = claude_plan_in(dir.path());
        let mut runtime =
            ForegroundRuntime::start_with_secret_store(&plan, Vec::new(), &FakeSecretStore(None))
                .unwrap();
        runtime
            .inject_statusline(&plan, &statusline_injection(dir.path()), Some(home.path()))
            .unwrap();
        let document = settings_document(&runtime).unwrap();
        assert_eq!(chain_of(&document).as_deref(), Some("npx ccstatusline"));
    }

    #[test]
    fn statusline_injection_merges_into_the_hooks_settings_file() {
        let repo = repo_declaring(r#"{"hooks":{"Stop":[{"command":"guard"}]}}"#);
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(ModelProvider::Claude, Backend::Claude, repo.path());
        let mut runtime =
            ForegroundRuntime::start_with_secret_store(&plan, Vec::new(), &FakeSecretStore(None))
                .unwrap();
        let before = settings_document(&runtime).expect("hooks produce a settings source");
        assert!(before.get("hooks").is_some());
        let path_before = runtime.claude_settings.as_ref().unwrap().value.clone();

        runtime
            .inject_statusline(&plan, &statusline_injection(repo.path()), Some(home.path()))
            .unwrap();

        assert_eq!(
            runtime.claude_settings.as_ref().unwrap().value,
            path_before,
            "one settings source, updated in place"
        );
        let after = settings_document(&runtime).unwrap();
        assert_eq!(after["hooks"], before["hooks"], "hooks survive the merge");
        assert!(after["statusLine"]["command"].is_string());
    }

    #[test]
    fn statusline_injection_reports_whether_the_status_line_was_composed() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let plan = claude_plan_in(dir.path());
        let mut runtime =
            ForegroundRuntime::start_with_secret_store(&plan, Vec::new(), &FakeSecretStore(None))
                .unwrap();
        let composed = runtime
            .inject_statusline(&plan, &statusline_injection(dir.path()), Some(home.path()))
            .unwrap();
        assert!(composed);

        // An executable path that cannot be quoted safely yields no command.
        let mut unquotable = statusline_injection(dir.path());
        unquotable.exe = PathBuf::from("/opt/$clud/bin/clud");
        let plan = claude_plan_in(dir.path());
        let mut runtime =
            ForegroundRuntime::start_with_secret_store(&plan, Vec::new(), &FakeSecretStore(None))
                .unwrap();
        let composed = runtime
            .inject_statusline(&plan, &unquotable, Some(home.path()))
            .unwrap();
        assert!(!composed, "no status line was injected");
    }

    #[test]
    fn statusline_injection_never_touches_a_codex_launch() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(ModelProvider::Codex, Backend::Codex);
        let mut runtime =
            ForegroundRuntime::start_with_secret_store(&plan, Vec::new(), &FakeSecretStore(None))
                .unwrap();
        runtime
            .inject_statusline(&plan, &statusline_injection(dir.path()), Some(dir.path()))
            .unwrap();
        assert!(runtime.claude_settings.is_none());
    }

    fn cli_subscription_record() -> crate::codex_auth::SubscriptionCredentials {
        crate::codex_upstream::CodexCliCredentials::from_auth_json(
            br#"{"tokens":{"access_token":"e30.eyJlbWFpbCI6InBlcnNvbkBleGFtcGxlLnRlc3QiLCJleHAiOjQxMDI0NDQ4MDB9.sig","refresh_token":"cli-refresh","account_id":"acct-cli"}}"#,
        )
        .unwrap()
        .subscription_record()
    }

    #[test]
    fn cli_login_import_acceptance_copies_only_into_clud_store() {
        let home = tempfile::tempdir().unwrap();
        let original = br#"{"tokens":{"access_token":"opaque","refresh_token":"cli-refresh"}}"#;
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(home.path().join(".codex/auth.json"), original).unwrap();

        let record = cli_subscription_record();
        let selector = CodexCliImportSelector::new(record.email.clone());
        assert!(selector
            .view(std::time::Duration::ZERO)
            .title
            .contains("person@example.test"));
        apply_codex_cli_import_choice_at(
            home.path(),
            None,
            &record,
            CodexCliImportChoice::Import,
            crate::codex_upstream::CodexBridgeCredentialError::Missing,
        )
        .unwrap();

        assert_eq!(
            crate::codex_auth::load_at(home.path()).unwrap(),
            Some(record)
        );
        assert_eq!(
            std::fs::read(home.path().join(".codex/auth.json")).unwrap(),
            original
        );
    }

    #[test]
    fn only_interactive_missing_or_rejected_credentials_offer_cli_import() {
        use crate::codex_upstream::CodexBridgeCredentialError as Error;
        assert!(!should_offer_codex_cli_login_import(Error::Missing, false));
        assert!(!should_offer_codex_cli_login_import(
            Error::RefreshRejected,
            false
        ));
        assert!(should_offer_codex_cli_login_import(
            Error::RefreshRejected,
            true
        ));
        for error in [
            Error::ExpiredOrRevoked,
            Error::RefreshTemporary,
            Error::RefreshMalformed,
            Error::Corrupt,
            Error::Unreadable,
        ] {
            assert!(!should_offer_codex_cli_login_import(error, true));
        }
    }

    #[test]
    fn native_login_repair_and_account_switch_have_distinct_prompts() {
        let native = cli_subscription_record();
        let same = native.clone();
        assert_eq!(
            codex_cli_import_mode(Some(&same), &native),
            CodexCliImportMode::RepairSameAccount
        );
        let repair =
            CodexCliImportSelector::for_replacement(&native, CodexCliImportMode::RepairSameAccount);
        assert!(repair
            .view(std::time::Duration::ZERO)
            .title
            .contains("Repair"));

        let mut different = native.clone();
        different.account_id = Some("another-account".to_string());
        assert_eq!(
            codex_cli_import_mode(Some(&different), &native),
            CodexCliImportMode::SwitchAccount
        );
        let switch =
            CodexCliImportSelector::for_replacement(&native, CodexCliImportMode::SwitchAccount);
        assert!(switch
            .view(std::time::Duration::ZERO)
            .title
            .contains("Switch"));

        different.account_id = None;
        assert_eq!(
            codex_cli_import_mode(Some(&different), &native),
            CodexCliImportMode::AmbiguousAccount
        );
    }

    #[test]
    fn never_choice_persists_and_not_now_leaves_no_clud_credentials() {
        let home = tempfile::tempdir().unwrap();
        let record = cli_subscription_record();
        assert!(apply_codex_cli_import_choice_at(
            home.path(),
            None,
            &record,
            CodexCliImportChoice::NotNow,
            crate::codex_upstream::CodexBridgeCredentialError::Missing,
        )
        .is_err());
        assert_eq!(crate::codex_auth::load_at(home.path()).unwrap(), None);

        assert!(apply_codex_cli_import_choice_at(
            home.path(),
            None,
            &record,
            CodexCliImportChoice::Never,
            crate::codex_upstream::CodexBridgeCredentialError::Missing,
        )
        .is_err());
        assert_eq!(
            crate::clud_settings::load_codex_cli_login_import_at(home.path()).unwrap(),
            Some(CodexCliLoginImport::Never)
        );
    }
}

#[cfg(test)]
#[path = "route_snapshots_tests.rs"]
mod route_snapshots_tests;

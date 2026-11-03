//! Step 0 of #1852 (issue #1854): characterization snapshots of what each
//! backend produces today, captured *before* any backend is moved onto the
//! central `ResolvedRoute` (#1855, and the renderers in #1856-#1859).
//!
//! These snapshots are the safety net for the migration. Every later PR must
//! keep them byte-identical except for the divergence lines it is explicitly
//! fixing; each of today's known divergences is marked where it shows up as
//! `// divergence #N, fixed in #<child>`.
//!
//! The scenario matrix is #1852's: backend x provider x model source x pin,
//! plus `--provider-only` and `:free`. Everything runs in-process -- no
//! network, no native vault (a fake store stands in), no child process. The
//! only per-run values are the loopback bridge's random port and bearer
//! token, and both are normalized to stable placeholders before comparison,
//! as is the sentinel that stands in for the vault secret, so a golden can
//! never become a credential.

use super::*;

/// Stands in for a vault-held secret. A rendered snapshot must never contain
/// it: `normalize` replaces it with a placeholder.
const SENTINEL: &str = "clud-1852-sentinel-secret";

/// Injectable fake so routing tests never touch the host's real native
/// credential vault.
struct FakeSecretStore(Option<String>);

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

/// A launch plan shaped like the ones the routing tests in the parent module
/// build, kept local so this file can move to `route_plan`'s render tests in
/// step 2 (#1856) without dragging the parent's fixtures along.
fn plan(provider: ModelProvider, harness: Backend) -> LaunchPlan {
    use crate::backend::{HarnessSelection, PreferenceSource};
    LaunchPlan {
        unsafe_mode: false,
        allow_plan_mode: false,
        command: vec![harness.executable_name().to_string()],
        iterations: 1,
        backend: harness,
        routing_mode: RoutingMode::Direct,
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
        launch_mode: crate::backend::LaunchMode::Subprocess,
        // A scratch cwd: the launch harvests `permissions.additionalDirectories`
        // from the nearest repo root, so a real cwd would fold this checkout's
        // own `.claude/settings.json` into the snapshot.
        cwd: Some(
            std::env::temp_dir()
                .join("clud-1852-snapshot-scratch")
                .to_string_lossy()
                .into_owned(),
        ),
        graphics: crate::graphics::GraphicsConfig::default(),
        repeat_schedule: None,
        task_summary: None,
        loop_markers: None,
        stream_json_progress: false,
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

/// Which of #1852's four backends produced a snapshot. Step 1 adds the
/// production `RouteBackend` (#1855); step 0 keeps its own copy so this PR
/// adds no production surface.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend4 {
    Direct,
    Unified,
    CodexBridge,
    DeepSeekNative,
}

impl Backend4 {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Unified => "unified",
            Self::CodexBridge => "codex-bridge",
            Self::DeepSeekNative => "deepseek-native",
        }
    }
}

/// One row of the matrix.
struct Scenario {
    name: &'static str,
    backend: Backend4,
    plan: LaunchPlan,
    /// Ambient child env the launch inherits.
    ambient: Vec<(String, String)>,
}

fn scenario(
    name: &'static str,
    backend: Backend4,
    plan: LaunchPlan,
    ambient: Vec<(String, String)>,
) -> Scenario {
    Scenario {
        name,
        backend,
        plan,
        ambient,
    }
}

fn selection(
    provider: ModelProvider,
    model: &str,
    effort: Option<&str>,
) -> crate::provider_catalog::ResolvedModelSelection {
    crate::provider_catalog::resolve(Some(provider), Some(model), effort, None)
        .expect("fixture model resolves")
        .expect("a named model yields a selection")
}

fn with_selection(
    mut plan: LaunchPlan,
    selection: crate::provider_catalog::ResolvedModelSelection,
) -> LaunchPlan {
    plan.model_selection = Some(selection);
    plan
}

fn claude_plan(provider: ModelProvider) -> LaunchPlan {
    plan(provider, Backend::Claude)
}

/// Direct routes: the descriptor-driven overlays of `--openrouter`,
/// `--deepseek` and `--kimi`.
fn direct_scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![
        scenario(
            "direct/deepseek/default",
            Backend4::Direct,
            claude_plan(ModelProvider::DeepSeek),
            vec![],
        ),
        scenario(
            "direct/deepseek/--model",
            Backend4::Direct,
            with_selection(
                claude_plan(ModelProvider::DeepSeek),
                selection(ModelProvider::DeepSeek, "deepseek-v4-pro", Some("high")),
            ),
            vec![],
        ),
    ];
    let allow_only = |allowed: &[&str]| {
        let mut plan = claude_plan(ModelProvider::DeepSeek);
        plan.allowed_models = allowed.iter().map(|id| id.to_string()).collect();
        plan
    };
    scenarios.push(scenario(
        "direct/deepseek/--allow-model",
        Backend4::Direct,
        allow_only(&["deepseek-v4-pro"]),
        vec![],
    ));
    // #1257's one precedence rule: the allowlist is the boundary and the user
    // chooses inside it. An ambient `CLAUDE_CODE_SUBAGENT_MODEL` wins only on
    // a constrained launch and only when the allowlist admits it, so this
    // scenario pins every slot to `deepseek-v4-pro` but leaves the subagent on
    // the ambient `deepseek-flash`.
    scenarios.push(scenario(
        "direct/deepseek/--allow-model+ambient-subagent",
        Backend4::Direct,
        allow_only(&["deepseek-v4-pro", "deepseek-flash"]),
        vec![(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "deepseek-flash".to_string(),
        )],
    ));
    // The same precedence, one step out of bounds: the ambient subagent names
    // a model the allowlist does not admit, so the pin wins it back.
    scenarios.push(scenario(
        "direct/deepseek/--allow-model+disallowed-ambient-subagent",
        Backend4::Direct,
        allow_only(&["deepseek-v4-pro"]),
        vec![(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "deepseek-flash".to_string(),
        )],
    ));
    scenarios.push(scenario(
        "direct/kimi/default",
        Backend4::Direct,
        claude_plan(ModelProvider::Kimi),
        vec![],
    ));
    scenarios.push(scenario(
        "direct/openrouter/default",
        Backend4::Direct,
        claude_plan(ModelProvider::OpenRouter),
        vec![],
    ));
    let mut provider_only = claude_plan(ModelProvider::OpenRouter);
    provider_only.provider_only = vec!["deepinfra".to_string()];
    scenarios.push(scenario(
        "direct/openrouter/--provider-only",
        Backend4::Direct,
        provider_only,
        vec![],
    ));
    let mut inherited = with_selection(
        claude_plan(ModelProvider::DeepSeek),
        selection(ModelProvider::DeepSeek, "deepseek-v4-pro", None),
    );
    inherited.pinned_from_previous_selection = true;
    scenarios.push(scenario(
        "direct/deepseek/inherited-previous-selection",
        Backend4::Direct,
        inherited,
        vec![],
    ));
    scenarios
}

/// The unified gateway: one local gateway in front of the harness, holding a
/// route per credential the launch has.
fn unified_scenarios() -> Vec<Scenario> {
    let unified = |provider: ModelProvider| {
        let mut plan = claude_plan(provider);
        plan.routing_mode = RoutingMode::Unified;
        plan
    };
    let mut pinned = unified(ModelProvider::OpenRouter);
    pinned.allowed_models = vec!["openrouter-claude-sonnet".to_string()];
    pinned.model_selection = Some(selection(
        ModelProvider::OpenRouter,
        "openrouter-claude-sonnet",
        None,
    ));
    // Divergence #1b, fixed in #1861: an unpinned gateway launch whose
    // provider has a descriptor now carries that descriptor's role mappings.
    // Before the fix this scenario wrote no slot at all.
    let mut unpinned = unified(ModelProvider::DeepSeek);
    unpinned.model_selection = Some(selection(ModelProvider::DeepSeek, "deepseek-v4-pro", None));
    vec![
        scenario(
            "unified/claude/default",
            Backend4::Unified,
            unified(ModelProvider::Claude),
            vec![],
        ),
        scenario(
            "unified/openrouter/--model-pin",
            Backend4::Unified,
            pinned,
            vec![],
        ),
        scenario(
            "unified/deepseek/unpinned",
            Backend4::Unified,
            unpinned,
            vec![],
        ),
    ]
}

/// The Codex route through the Claude harness: clud's translation bridge.
fn codex_bridge_scenarios() -> Vec<Scenario> {
    let mut model = claude_plan(ModelProvider::Codex);
    model.codex_model = Some("codex-terra".to_string());
    let mut allow = claude_plan(ModelProvider::Codex);
    allow.allowed_models = vec!["codex-terra".to_string()];
    vec![
        scenario(
            "codex-bridge/codex/default",
            Backend4::CodexBridge,
            claude_plan(ModelProvider::Codex),
            vec![],
        ),
        scenario(
            "codex-bridge/codex/--model",
            Backend4::CodexBridge,
            model,
            vec![],
        ),
        scenario(
            "codex-bridge/codex/--allow-model",
            Backend4::CodexBridge,
            allow,
            vec![],
        ),
    ]
}

/// The DeepSeek-native harness: `dsh` owns the request path, and clud hands
/// it a key and (for OpenRouter) a `--patch` overlay.
fn deepseek_native_scenarios() -> Vec<Scenario> {
    vec![
        scenario(
            "deepseek-native/deepseek/default",
            Backend4::DeepSeekNative,
            with_selection(
                plan(ModelProvider::DeepSeek, Backend::DeepSeek),
                selection(ModelProvider::DeepSeek, "deepseek-v4-pro", None),
            ),
            vec![],
        ),
        scenario(
            "deepseek-native/openrouter/--model",
            Backend4::DeepSeekNative,
            with_selection(
                plan(ModelProvider::OpenRouter, Backend::DeepSeek),
                selection(ModelProvider::OpenRouter, "openrouter-claude-sonnet", None),
            ),
            vec![],
        ),
    ]
}

fn matrix() -> Vec<Scenario> {
    let mut scenarios = direct_scenarios();
    scenarios.extend(unified_scenarios());
    scenarios.extend(codex_bridge_scenarios());
    scenarios.extend(deepseek_native_scenarios());
    scenarios
}

/// Render one backend's scenarios, in matrix order.
fn render_backend(scenarios: &[Scenario]) -> String {
    let mut rendered = String::new();
    for scenario in scenarios {
        rendered.push_str(&match scenario.backend {
            Backend4::DeepSeekNative => render_dsh(scenario),
            _ => render_claude_harness(scenario),
        });
    }
    rendered
}

/// Replace every per-run value with a stable placeholder. The bridge listens
/// on a random loopback port with a random bearer token, and the vault secret
/// varies by launch; none of the three is part of the decision under test.
fn normalize(lines: &mut [String], runtime: Option<&ForegroundRuntime>) {
    let sockets = runtime
        .and_then(|runtime| {
            Some((
                runtime.base_url()?.to_string(),
                runtime.bearer_token()?.to_string(),
            ))
        })
        .into_iter()
        .collect::<Vec<_>>();
    for line in lines.iter_mut() {
        for (base_url, token) in &sockets {
            *line = line.replace(base_url.as_str(), "http://gateway.invalid");
            *line = line.replace(token, "<gateway-token>");
        }
        *line = line.replace(SENTINEL, "<vault-secret>");
    }
}

fn render_lines(header: &str, mut lines: Vec<String>) -> String {
    lines.sort();
    let mut rendered = format!("--- {header}\n");
    for line in lines {
        rendered.push_str("  ");
        rendered.push_str(&line);
        rendered.push('\n');
    }
    rendered
}

/// Render one Claude-harness scenario: the child env plus the routing
/// configuration the launch hands its bridge, if any.
fn render_claude_harness(scenario: &Scenario) -> String {
    let runtime = ForegroundRuntime::start_with_secret_store(
        &scenario.plan,
        scenario.ambient.clone(),
        &FakeSecretStore(Some(SENTINEL.to_string())),
    )
    .unwrap_or_else(|error| panic!("{}: launch failed: {error}", scenario.name));

    let mut lines = runtime
        .env()
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    normalize(&mut lines, Some(&runtime));
    let header = format!("{} [{}]", scenario.name, scenario.backend.label());
    let mut rendered = render_lines(&header, lines);
    rendered.push_str(&render_config(scenario));
    rendered
}

/// The routing configuration this backend builds alongside its env. Unified
/// renders the gateway's structural Debug view (route list, Codex
/// availability, failover rung count) built from fixture credentials, so it
/// stays deterministic and secret-free; the Codex bridge renders the config
/// fields its branch derives from the plan.
fn render_config(scenario: &Scenario) -> String {
    match scenario.backend {
        Backend4::Unified => {
            let config = crate::codex_bridge::UnifiedGatewayConfig::new(
                Some("fixture-deepseek".to_string()),
                true,
            )
            .with_openrouter(Some("fixture-openrouter".to_string()))
            .with_route(ModelProvider::Kimi, Some("fixture-kimi".to_string()));
            format!("  gateway={config:?}\n")
        }
        Backend4::CodexBridge => {
            let selection = codex_selection_from_plan(&scenario.plan).expect("codex selection");
            let allowed = crate::route_plan::codex_bridge_allowlist(&scenario.plan);
            format!("  bridge.default_model={selection:?}\n  bridge.allowed_models={allowed:?}\n")
        }
        Backend4::Direct | Backend4::DeepSeekNative => String::new(),
    }
}

/// Render one DeepSeek-native scenario: the dsh child env and the overlay
/// YAML. The patch is written under a scratch home so the real one is never
/// touched, and the scratch directory removes itself.
fn render_dsh(scenario: &Scenario) -> String {
    let route = crate::route_plan::resolve(
        &scenario.plan,
        &crate::route_plan::Ambient::from_child_env(&scenario.ambient),
    );
    let render = crate::route_plan::render_dsh(&route);
    let facts = crate::dsh_harness::ChildFacts {
        executable: "dsh",
        provider: render.provider,
        wire_model: render.wire_model.as_deref(),
        base_url: render.base_url.as_deref(),
    };
    let home = tempfile::tempdir().expect("scratch home");
    let mut env = scenario.ambient.clone();
    crate::dsh_harness::prepare_child(
        &facts,
        Some(home.path()),
        &mut env,
        // A deterministic ambient: the dsh setup may consult the process
        // environment, which would make the snapshot host-dependent.
        &|_| None,
        &|provider| crate::dsh_harness::key_env_for(provider).map(|_| SENTINEL.to_string()),
    )
    .unwrap_or_else(|error| panic!("{}: dsh setup failed: {error}", scenario.name));

    let lines = env
        .iter()
        .map(|(key, value)| format!("{key}={}", value.replace(SENTINEL, "<vault-secret>")))
        .collect::<Vec<_>>();
    let header = format!("{} [{}]", scenario.name, scenario.backend.label());
    let mut rendered = render_lines(&header, lines);
    let provider = scenario.plan.model_provider();
    let key_env = crate::dsh_harness::key_env_for(match provider {
        ModelProvider::OpenRouter => ModelProvider::OpenRouter,
        _ => ModelProvider::DeepSeek,
    })
    .unwrap_or("<none>");
    rendered.push_str(&format!("  dsh.key_env={key_env}\n"));
    // Only an OpenRouter launch gets the `--patch` overlay: dsh resolves its
    // own DeepSeek route from the key alone.
    if provider == ModelProvider::OpenRouter {
        let wire = facts.wire_model.unwrap_or("<none>");
        rendered.push_str(&format!("  dsh.wire_model={wire}\n"));
        let patch = crate::dsh_harness::openrouter_patch_path(home.path(), wire);
        let relative = patch
            .strip_prefix(home.path())
            .expect("patch lives under the scratch home");
        // The golden is OS-neutral: Windows joins components with `\`.
        let relative = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        rendered.push_str(&format!("  dsh.patch_path={relative}\n"));
        for line in
            crate::dsh_harness::openrouter_patch_yaml(wire, "https://openrouter.ai/api").lines()
        {
            rendered.push_str("  dsh.yaml|");
            rendered.push_str(line);
            rendered.push('\n');
        }
    }
    rendered
}

/// A fixture OpenRouter catalog: `openrouter_free` classifies against
/// pricing, and the real embedded catalog is not this test's subject.
fn free_catalog() -> crate::openrouter_catalog::Catalog {
    let row = |id: &str, input: f64, output: f64| {
        serde_json::json!({
            "id": id,
            "name": id,
            "provider": "nvidia",
            "context_length": 262_144,
            "input_price_per_token": input,
            "output_price_per_token": output,
            "cached_input_price_per_token": null,
            "supports_tools": true,
            "supports_text_input": true,
            "supports_text_output": true,
            "supports_reasoning": true,
            "supports_vision": false,
            "eligible_for_coding": true,
            "ineligibility_reasons": [],
        })
    };
    let json = serde_json::json!({
        "schema_version": 1,
        "generated_at": "2026-10-05T00:00:00Z",
        "source": "https://openrouter.ai/api/v1/models",
        "models": [
            row("nvidia/nemotron-3-ultra-550b-a55b:free", 0.0, 0.0),
            row("nvidia/nemotron-3-ultra-550b-a55b", 5e-7, 2.2e-6),
            row("acme/priced:free", 1e-6, 2e-6),
        ],
    });
    crate::openrouter_catalog::Catalog::parse(json.to_string().as_bytes())
        .expect("fixture catalog parses")
}

/// `:free` handling. `openrouter_free::check`/`refusal` are the pure decision
/// and the pure message; the probe that follows a `Free` verdict is the only
/// part of #1833 that needs the network, and today it runs on the direct
/// route alone -- `// divergence #2, fixed in #1862`.
fn render_free_checks() -> String {
    let catalog = free_catalog();
    let mut lines = Vec::new();
    for id in [
        "nvidia/nemotron-3-ultra-550b-a55b:free",
        "acme/priced:free",
        "acme/missing:free",
        "nvidia/nemotron-3-ultra-550b-a55b",
    ] {
        let verdict = crate::openrouter_free::check(id, &catalog);
        let refusal = crate::openrouter_free::refusal(id, &verdict, &catalog);
        lines.push(format!("{id}={verdict:?}"));
        lines.push(format!(
            "{id}.refusal={}",
            refusal.unwrap_or_else(|| "<none>".to_string())
        ));
    }
    render_lines("free/id-checks [direct]", lines)
}

/// Render every scenario's snapshot, in matrix order.
fn render_all() -> String {
    let mut rendered = render_backend(&matrix());
    rendered.push_str(&render_free_checks());
    rendered
}

fn assert_golden(backend: &str, rendered: &str, golden: &str) {
    if rendered != golden {
        panic!("{backend} snapshot mismatch.\n--- rendered ---\n{rendered}--- end ---");
    }
}

/// GOLDEN (#1854): the direct route's launch output today, captured before the
/// migration -- the byte-for-byte baseline step 2 (#1856) must reproduce.
///
/// `// divergence #2, fixed in #1862`: the `:free` launch check and the
/// `--provider-only` validation run here and nowhere else. See
/// `render_free_checks` for the check itself.
const DIRECT_GOLDEN: &str = r#"--- direct/deepseek/default [direct]
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic
  ANTHROPIC_DEFAULT_FABLE_MODEL=deepseek-flash[1m]
  ANTHROPIC_DEFAULT_HAIKU_MODEL=deepseek-flash[1m]
  ANTHROPIC_DEFAULT_OPUS_MODEL=deepseek-flash[1m]
  ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-flash[1m]
  ANTHROPIC_MODEL=deepseek-flash[1m]
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_AUTO_COMPACT_WINDOW=786432
  CLAUDE_CODE_SUBAGENT_MODEL=deepseek-flash[1m]
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"deepseek","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
--- direct/deepseek/--model [direct]
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic
  ANTHROPIC_DEFAULT_FABLE_MODEL=deepseek-v4-pro[1m]
  ANTHROPIC_DEFAULT_HAIKU_MODEL=deepseek-flash[1m]
  ANTHROPIC_DEFAULT_OPUS_MODEL=deepseek-v4-pro[1m]
  ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-v4-pro[1m]
  ANTHROPIC_MODEL=deepseek-v4-pro[1m]
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_AUTO_COMPACT_WINDOW=786432
  CLAUDE_CODE_SUBAGENT_MODEL=deepseek-flash[1m]
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"deepseek","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
--- direct/deepseek/--allow-model [direct]
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic
  ANTHROPIC_DEFAULT_FABLE_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_HAIKU_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_OPUS_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-v4-pro
  ANTHROPIC_MODEL=deepseek-v4-pro
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_SUBAGENT_MODEL=deepseek-v4-pro
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"deepseek","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
--- direct/deepseek/--allow-model+ambient-subagent [direct]
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic
  ANTHROPIC_DEFAULT_FABLE_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_HAIKU_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_OPUS_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-v4-pro
  ANTHROPIC_MODEL=deepseek-v4-pro
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_SUBAGENT_MODEL=deepseek-flash
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"deepseek","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
--- direct/deepseek/--allow-model+disallowed-ambient-subagent [direct]
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic
  ANTHROPIC_DEFAULT_FABLE_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_HAIKU_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_OPUS_MODEL=deepseek-v4-pro
  ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-v4-pro
  ANTHROPIC_MODEL=deepseek-v4-pro
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_SUBAGENT_MODEL=deepseek-v4-pro
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"deepseek","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
--- direct/kimi/default [direct]
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://api.moonshot.ai/anthropic
  ANTHROPIC_DEFAULT_FABLE_MODEL=kimi-k3[1m]
  ANTHROPIC_DEFAULT_HAIKU_MODEL=kimi-k3[1m]
  ANTHROPIC_DEFAULT_OPUS_MODEL=kimi-k3[1m]
  ANTHROPIC_DEFAULT_SONNET_MODEL=kimi-k3[1m]
  ANTHROPIC_MODEL=kimi-k3[1m]
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_AUTO_COMPACT_WINDOW=1048576
  CLAUDE_CODE_SUBAGENT_MODEL=kimi-k3[1m]
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"kimi","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
--- direct/openrouter/default [direct]
  ANTHROPIC_API_KEY=
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://openrouter.ai/api
  ANTHROPIC_DEFAULT_FABLE_MODEL=~anthropic/claude-fable-latest
  ANTHROPIC_DEFAULT_HAIKU_MODEL=~anthropic/claude-haiku-latest
  ANTHROPIC_DEFAULT_OPUS_MODEL=~anthropic/claude-opus-latest
  ANTHROPIC_DEFAULT_SONNET_MODEL=~anthropic/claude-sonnet-latest
  ANTHROPIC_MODEL=~anthropic/claude-sonnet-latest
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000
  CLAUDE_CODE_SUBAGENT_MODEL=~anthropic/claude-opus-latest
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"openrouter","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
--- direct/openrouter/--provider-only [direct]
  ANTHROPIC_API_KEY=
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://openrouter.ai/api
  ANTHROPIC_DEFAULT_FABLE_MODEL=~anthropic/claude-fable-latest
  ANTHROPIC_DEFAULT_HAIKU_MODEL=~anthropic/claude-haiku-latest
  ANTHROPIC_DEFAULT_OPUS_MODEL=~anthropic/claude-opus-latest
  ANTHROPIC_DEFAULT_SONNET_MODEL=~anthropic/claude-sonnet-latest
  ANTHROPIC_MODEL=~anthropic/claude-sonnet-latest
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  CLAUDE_CODE_EXTRA_BODY={"provider":{"only":["deepinfra"],"allow_fallbacks":false}}
  CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000
  CLAUDE_CODE_SUBAGENT_MODEL=~anthropic/claude-opus-latest
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"openrouter","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
--- direct/deepseek/inherited-previous-selection [direct]
  ANTHROPIC_AUTH_TOKEN=<vault-secret>
  ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic
  ANTHROPIC_DEFAULT_FABLE_MODEL=deepseek-v4-pro[1m]
  ANTHROPIC_DEFAULT_HAIKU_MODEL=deepseek-flash[1m]
  ANTHROPIC_DEFAULT_OPUS_MODEL=deepseek-v4-pro[1m]
  ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-v4-pro[1m]
  ANTHROPIC_MODEL=deepseek-v4-pro[1m]
  API_TIMEOUT_MS=600000
  CLAUDE_CODE_AUTO_COMPACT_WINDOW=786432
  CLAUDE_CODE_SUBAGENT_MODEL=deepseek-flash[1m]
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"deepseek","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
"#;

/// GOLDEN (#1854): the unified gateway's launch output today, the baseline
/// step 4 (#1858) must reproduce except for the divergences below.
///
/// `// divergence #1, fixed in #1861`: unpinned, no slot is set at all, so
/// `role_models` and the served subagent model never reach this route.
/// `// divergence #1, fixed in #1861`: pinned, the slots take the row's
/// *discovery* id, where the direct route writes the raw wire id.
/// Divergence #3 (fixed in #1860): the gateway route now writes
/// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` from the same resolver value the direct
/// route uses, so a gateway model is no longer clamped at the harness's 200k
/// default. Divergence #4 (fixed in #1860): `CLAUDE_CODE_MODEL_CAPABILITIES`
/// is written the same way; it is absent here only because the per-turn-effort
/// setting ships dark.
/// `unified/claude/default` still writes no slot: a native Claude launch has
/// no descriptor and no pin, so there is nothing to bind, and no wire id to
/// key a context window on. `unified/deepseek/unpinned` is the #1861 fix -- it
/// carries the descriptor's role mappings. Divergence #1 is fixed in #1861:
/// the *slot decision* is now one function, and only the id namespace each
/// renderer asks for differs.
const UNIFIED_GOLDEN: &str = r#"--- unified/claude/default [unified]
  ANTHROPIC_BASE_URL=http://gateway.invalid
  ANTHROPIC_CUSTOM_HEADERS=X-Clud-Gateway-Token: <gateway-token>
  API_TIMEOUT_MS=3000000
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_GATEWAY_TOKEN=<gateway-token>
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"claude","harness":"claude","routing_mode":"unified","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
  gateway=UnifiedGatewayConfig { configured_routes: ["deepseek", "openrouter", "kimi"], codex_available: true, failover_rungs: 0 }
--- unified/openrouter/--model-pin [unified]
  ANTHROPIC_BASE_URL=http://gateway.invalid
  ANTHROPIC_CUSTOM_HEADERS=X-Clud-Gateway-Token: <gateway-token>
  ANTHROPIC_DEFAULT_FABLE_MODEL=clud-claude-openrouter-sonnet
  ANTHROPIC_DEFAULT_HAIKU_MODEL=clud-claude-openrouter-sonnet
  ANTHROPIC_DEFAULT_OPUS_MODEL=clud-claude-openrouter-sonnet
  ANTHROPIC_DEFAULT_SONNET_MODEL=clud-claude-openrouter-sonnet
  API_TIMEOUT_MS=3000000
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000
  CLAUDE_CODE_SUBAGENT_MODEL=clud-claude-openrouter-sonnet
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_GATEWAY_TOKEN=<gateway-token>
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"openrouter","harness":"claude","routing_mode":"unified","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
  gateway=UnifiedGatewayConfig { configured_routes: ["deepseek", "openrouter", "kimi"], codex_available: true, failover_rungs: 0 }
--- unified/deepseek/unpinned [unified]
  ANTHROPIC_BASE_URL=http://gateway.invalid
  ANTHROPIC_CUSTOM_HEADERS=X-Clud-Gateway-Token: <gateway-token>
  ANTHROPIC_DEFAULT_FABLE_MODEL=clud-claude-deepseek-v4-pro-0813
  ANTHROPIC_DEFAULT_HAIKU_MODEL=clud-claude-deepseek-flash
  ANTHROPIC_DEFAULT_OPUS_MODEL=clud-claude-deepseek-v4-pro-0813
  ANTHROPIC_DEFAULT_SONNET_MODEL=clud-claude-deepseek-v4-pro-0813
  API_TIMEOUT_MS=3000000
  CLAUDE_CODE_AUTO_COMPACT_WINDOW=786432
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  CLAUDE_CODE_SUBAGENT_MODEL=clud-claude-deepseek-flash
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_GATEWAY_TOKEN=<gateway-token>
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"deepseek","harness":"claude","routing_mode":"unified","delegation":{"cost_policy":"prefer_the_cheapest_harness_supported_worker; escalate_only_when_needed","roles":"use_harness_native_model_selection"}}
  CLUD_WORKTREE_GATE=1
  gateway=UnifiedGatewayConfig { configured_routes: ["deepseek", "openrouter", "kimi"], codex_available: true, failover_rungs: 0 }
"#;

/// GOLDEN (#1854): the Codex-via-Claude bridge's launch output today, the
/// baseline step 3 (#1857) must reproduce.
///
/// `// divergence #2, fixed in #1862`: `--provider-only` and the `:free`
/// cost check do not apply here; the bridge's own allowlist widening is the
/// only boundary this route builds.
const CODEX_BRIDGE_GOLDEN: &str = r#"--- codex-bridge/codex/default [codex-bridge]
  ANTHROPIC_AUTH_TOKEN=<gateway-token>
  ANTHROPIC_BASE_URL=http://gateway.invalid
  ANTHROPIC_DEFAULT_OPUS_MODEL=clud-claude-codex-sol
  ANTHROPIC_DEFAULT_OPUS_MODEL_NAME=Codex Sol (OpenAI)
  ANTHROPIC_DEFAULT_SONNET_MODEL=clud-claude-codex-terra
  ANTHROPIC_DEFAULT_SONNET_MODEL_NAME=Codex Terra (OpenAI)
  API_TIMEOUT_MS=3000000
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  CLAUDE_CODE_MAX_CONTEXT_TOKENS=1050000
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"codex","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"workers_use_sonnet; reserve_opus_for_planning_review_and_integration","roles":{"planner":"opus","reviewer":"opus","integrator":"opus","worker":"sonnet"},"resolved_models":{"opus":"clud-claude-codex-sol","sonnet":"clud-claude-codex-terra"}}}
  CLUD_WORKTREE_GATE=1
  bridge.default_model=None
  bridge.allowed_models=[]
--- codex-bridge/codex/--model [codex-bridge]
  ANTHROPIC_AUTH_TOKEN=<gateway-token>
  ANTHROPIC_BASE_URL=http://gateway.invalid
  ANTHROPIC_DEFAULT_OPUS_MODEL=clud-claude-codex-sol
  ANTHROPIC_DEFAULT_OPUS_MODEL_NAME=Codex Sol (OpenAI)
  ANTHROPIC_DEFAULT_SONNET_MODEL=clud-claude-codex-terra
  ANTHROPIC_DEFAULT_SONNET_MODEL_NAME=Codex Terra (OpenAI)
  API_TIMEOUT_MS=3000000
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  CLAUDE_CODE_MAX_CONTEXT_TOKENS=1050000
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"codex","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"workers_use_sonnet; reserve_opus_for_planning_review_and_integration","roles":{"planner":"opus","reviewer":"opus","integrator":"opus","worker":"sonnet"},"resolved_models":{"opus":"clud-claude-codex-sol","sonnet":"clud-claude-codex-terra"}}}
  CLUD_WORKTREE_GATE=1
  bridge.default_model=Some(ModelSpec { model: "gpt-5.6-terra", effort: None })
  bridge.allowed_models=[]
--- codex-bridge/codex/--allow-model [codex-bridge]
  ANTHROPIC_AUTH_TOKEN=<gateway-token>
  ANTHROPIC_BASE_URL=http://gateway.invalid
  ANTHROPIC_DEFAULT_OPUS_MODEL=clud-claude-codex-sol
  ANTHROPIC_DEFAULT_OPUS_MODEL_NAME=Codex Sol (OpenAI)
  ANTHROPIC_DEFAULT_SONNET_MODEL=clud-claude-codex-terra
  ANTHROPIC_DEFAULT_SONNET_MODEL_NAME=Codex Terra (OpenAI)
  API_TIMEOUT_MS=3000000
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  CLAUDE_CODE_MAX_CONTEXT_TOKENS=1050000
  CLUD_ALLOW_PLAN_MODE=0
  CLUD_ROUTE_CONTEXT={"version":1,"model_provider":"codex","harness":"claude","routing_mode":"direct","delegation":{"cost_policy":"workers_use_sonnet; reserve_opus_for_planning_review_and_integration","roles":{"planner":"opus","reviewer":"opus","integrator":"opus","worker":"sonnet"},"resolved_models":{"opus":"clud-claude-codex-sol","sonnet":"clud-claude-codex-terra"}}}
  CLUD_WORKTREE_GATE=1
  bridge.default_model=None
  bridge.allowed_models=["codex-terra", "clud-claude-codex-sol", "clud-claude-codex-terra"]
"#;

/// GOLDEN (#1854): the DeepSeek-native harness's launch output today, the
/// baseline step 4b (#1859) must reproduce. dsh reads its key straight from
/// the process environment, which is the `CredentialSource::Env` the resolver
/// has to make explicit.
const DEEPSEEK_NATIVE_GOLDEN: &str = r#"--- deepseek-native/deepseek/default [deepseek-native]
  DEEPSEEK_API_KEY=<vault-secret>
  dsh.key_env=DEEPSEEK_API_KEY
--- deepseek-native/openrouter/--model [deepseek-native]
  OPENROUTER_API_KEY=<vault-secret>
  dsh.key_env=OPENROUTER_API_KEY
  dsh.wire_model=~anthropic/claude-sonnet-latest
  dsh.patch_path=.clud/harnesses/dsh/patches/openrouter-_anthropic_claude-sonnet-latest.yml
  dsh.yaml|# managed-by: clud. Regenerated on every OpenRouter dsh launch (#1829).
  dsh.yaml|- id: llm-pi-ai
  dsh.yaml|  config:
  dsh.yaml|    providers:
  dsh.yaml|      openrouter:
  dsh.yaml|        apiKeyEnv: OPENROUTER_API_KEY
  dsh.yaml|        api: anthropic-messages
  dsh.yaml|        baseURL: 'https://openrouter.ai/api'
  dsh.yaml|        models:
  dsh.yaml|          - id: '~anthropic/claude-sonnet-latest'
  dsh.yaml|- id: agent-default-model
  dsh.yaml|  config:
  dsh.yaml|    provider: openrouter
  dsh.yaml|    model: '~anthropic/claude-sonnet-latest'
"#;

/// GOLDEN (#1854): the `:free` decision and message, which are the same on
/// every backend because they are pure functions of the catalog.
///
/// `// divergence #2, fixed in #1862`: applying them is what runs on the
/// direct route alone today.
const FREE_GOLDEN: &str = r#"--- free/id-checks [direct]
  acme/missing:free.refusal=`acme/missing:free` is not in OpenRouter's model catalog, so clud cannot confirm it is free; refusing rather than risk billing. Without `:free`, `acme/missing` is a paid model whose price clud does not know.
  acme/missing:free=Unknown
  acme/priced:free.refusal=`acme/priced:free` is priced at $1.00/$2.00 per million input/output tokens on OpenRouter, but `:free` asks for a free model; refusing.
  acme/priced:free=NotFree { input: 1.0, output: 2.0 }
  nvidia/nemotron-3-ultra-550b-a55b.refusal=<none>
  nvidia/nemotron-3-ultra-550b-a55b:free.refusal=<none>
  nvidia/nemotron-3-ultra-550b-a55b:free=Free
  nvidia/nemotron-3-ultra-550b-a55b=NotRequested
"#;

#[test]
fn direct_snapshots_are_the_committed_golden() {
    assert_golden(
        "direct",
        &render_backend(&direct_scenarios()),
        DIRECT_GOLDEN,
    );
}

#[test]
fn unified_snapshots_are_the_committed_golden() {
    assert_golden(
        "unified",
        &render_backend(&unified_scenarios()),
        UNIFIED_GOLDEN,
    );
}

#[test]
fn codex_bridge_snapshots_are_the_committed_golden() {
    assert_golden(
        "codex-bridge",
        &render_backend(&codex_bridge_scenarios()),
        CODEX_BRIDGE_GOLDEN,
    );
}

#[test]
fn deepseek_native_snapshots_are_the_committed_golden() {
    assert_golden(
        "deepseek-native",
        &render_backend(&deepseek_native_scenarios()),
        DEEPSEEK_NATIVE_GOLDEN,
    );
}

#[test]
fn free_check_snapshots_are_the_committed_golden() {
    assert_golden("free", &render_free_checks(), FREE_GOLDEN);
}

#[test]
fn route_snapshots_are_deterministic() {
    assert_eq!(render_all(), render_all());
}

#[test]
fn route_snapshots_never_carry_the_vault_secret() {
    assert!(!render_all().contains(SENTINEL));
}

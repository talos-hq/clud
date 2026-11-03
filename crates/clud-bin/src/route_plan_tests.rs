//! `route_plan` tests: #1852's A1-A8, plus the secret-free guarantee (C12).
//!
//! Every test here is pure: no network, no native vault, no child process. The
//! catalog and the per-turn-effort gate are passed in, and the one piece of
//! process state the precedence rule reads arrives as an explicit [`Ambient`].

use super::*;
use crate::backend::{Backend, HarnessSelection, ModelProvider, PreferenceSource, RoutingMode};
use crate::command::LaunchPlan;

fn plan(provider: ModelProvider, harness: Backend) -> LaunchPlan {
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
        cwd: None,
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

fn unified(provider: ModelProvider) -> LaunchPlan {
    let mut plan = plan(provider, Backend::Claude);
    plan.routing_mode = RoutingMode::Unified;
    plan
}

fn with_selection(mut plan: LaunchPlan, model: &str, provider: ModelProvider) -> LaunchPlan {
    plan.model_selection =
        crate::provider_catalog::resolve(Some(provider), Some(model), None, None)
            .expect("fixture model resolves");
    plan
}

/// The offline catalog fixture: `openrouter_free` and the per-turn capability
/// both read pricing/capability from here, so neither test touches the daemon's
/// cache or the network.
fn catalog() -> crate::openrouter_catalog::Catalog {
    let row = |id: &str, effort: bool| {
        serde_json::json!({
            "id": id,
            "name": id,
            "provider": "nvidia",
            "context_length": 262_144,
            "input_price_per_token": 1e-6,
            "output_price_per_token": 2e-6,
            "cached_input_price_per_token": null,
            "supports_tools": true,
            "supports_text_input": true,
            "supports_text_output": true,
            "supports_reasoning": true,
            "supports_reasoning_effort": effort,
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
            row("~anthropic/claude-sonnet-latest", true),
            row("nvidia/nemotron-3-ultra-550b-a55b:free", false),
        ],
    });
    crate::openrouter_catalog::Catalog::parse(json.to_string().as_bytes())
        .expect("fixture catalog parses")
}

fn resolve(plan: &LaunchPlan, ambient: &Ambient) -> ResolvedRoute {
    resolve_with_catalog(plan, ambient, &catalog(), true)
}

fn slot_wire(slot: &Option<SlotModel>) -> Option<&str> {
    slot.as_ref().map(|slot| slot.wire_id.as_str())
}

/// A1: the backend classification covers the whole matrix, and the runtime's
/// own branch order is what decides it.
#[test]
fn every_plan_shape_resolves_to_the_backend_that_will_run_it() {
    let cases = [
        (
            plan(ModelProvider::Claude, Backend::Claude),
            RouteBackend::Native,
        ),
        (
            plan(ModelProvider::DeepSeek, Backend::Claude),
            RouteBackend::Direct,
        ),
        (
            plan(ModelProvider::Kimi, Backend::Claude),
            RouteBackend::Direct,
        ),
        (
            plan(ModelProvider::OpenRouter, Backend::Claude),
            RouteBackend::Direct,
        ),
        (
            plan(ModelProvider::Codex, Backend::Claude),
            RouteBackend::CodexBridge,
        ),
        (
            plan(ModelProvider::DeepSeek, Backend::DeepSeek),
            RouteBackend::DeepSeekNative,
        ),
        (unified(ModelProvider::Claude), RouteBackend::Unified),
        (unified(ModelProvider::OpenRouter), RouteBackend::Unified),
    ];
    for (plan, expected) in cases {
        let route = resolve(&plan, &Ambient::default());
        assert_eq!(route.backend, expected, "{}", route.backend.as_str());
    }
}

/// A2: slot pinning is decided once. A pin covers every slot and fable too,
/// and an ambient subagent wins only when the allowlist admits it -- the
/// single precedence rule #1257 established, here and nowhere else.
#[test]
fn a_pin_covers_every_slot_and_the_ambient_subagent_wins_only_inside_it() {
    let mut pinned = plan(ModelProvider::DeepSeek, Backend::Claude);
    pinned.allowed_models = vec!["deepseek-v4-pro".to_string()];

    let plain = resolve(&pinned, &Ambient::default());
    for slot in [
        &plain.slots.main,
        &plain.slots.opus,
        &plain.slots.sonnet,
        &plain.slots.haiku,
        &plain.slots.fable,
        &plain.slots.subagent,
    ] {
        assert_eq!(slot_wire(slot), Some("deepseek-v4-pro"));
    }

    let admitted = resolve(
        &pinned,
        &Ambient {
            subagent_model: Some("deepseek-v4-pro".to_string()),
            ..Ambient::default()
        },
    );
    assert_eq!(slot_wire(&admitted.slots.subagent), Some("deepseek-v4-pro"));
    assert_eq!(slot_wire(&admitted.slots.opus), Some("deepseek-v4-pro"));

    // The same rule, outside the boundary: the pin wins the slot back.
    let refused = resolve(
        &pinned,
        &Ambient {
            subagent_model: Some("deepseek-flash".to_string()),
            ..Ambient::default()
        },
    );
    assert_eq!(slot_wire(&refused.slots.subagent), Some("deepseek-v4-pro"));

    // And on an unconstrained launch there is nothing for the ambient value to
    // win: the descriptor's own role mapping stays authoritative.
    let free = plan(ModelProvider::DeepSeek, Backend::Claude);
    let unconstrained = resolve(
        &free,
        &Ambient {
            subagent_model: Some("some/ambient-model".to_string()),
            ..Ambient::default()
        },
    );
    assert_eq!(unconstrained.allowlist, Vec::<String>::new());
    assert_eq!(
        slot_wire(&unconstrained.slots.subagent),
        Some("deepseek-flash[1m]")
    );
}

/// A2 also covers the direct-vs-unified equality divergence #1 is about: the
/// *decision* is one, and only the id namespace each renderer asks for differs.
#[test]
fn direct_and_unified_choose_the_same_rows_for_the_same_pin() {
    let mut direct = plan(ModelProvider::OpenRouter, Backend::Claude);
    direct.allowed_models = vec!["openrouter-claude-sonnet".to_string()];
    let mut unified = direct.clone();
    unified.routing_mode = RoutingMode::Unified;

    let direct = resolve(&direct, &Ambient::default());
    let unified = resolve(&unified, &Ambient::default());
    assert_eq!(direct.slots, unified.slots);
    assert_eq!(
        direct
            .slots
            .opus
            .as_ref()
            .and_then(|slot| slot.discovery_id.as_deref()),
        Some("clud-claude-openrouter-sonnet")
    );
}

/// A3: unpinned, every descriptor's role table reaches the slots, and the
/// served subagent model overrides the compiled-in one.
#[test]
fn unpinned_slots_follow_each_descriptor_role_table() {
    for descriptor in crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS {
        let mut plan = plan(descriptor.provider, Backend::Claude);
        plan.model_selection =
            crate::provider_catalog::resolve(Some(descriptor.provider), None, None, None)
                .expect("provider default resolves");
        let route = resolve(&plan, &Ambient::default());
        let main = slot_wire(&route.slots.main).expect("a descriptor has a reviewed default");
        match descriptor.role_models {
            Some(roles) => {
                assert_eq!(slot_wire(&route.slots.opus), Some(roles.opus));
                assert_eq!(slot_wire(&route.slots.sonnet), Some(roles.sonnet));
                assert_eq!(slot_wire(&route.slots.haiku), Some(roles.haiku));
                assert_eq!(slot_wire(&route.slots.fable), roles.fable);
            }
            None => {
                assert_eq!(slot_wire(&route.slots.opus), Some(main));
                assert_eq!(slot_wire(&route.slots.sonnet), Some(main));
                assert_eq!(
                    slot_wire(&route.slots.haiku),
                    Some(descriptor.subagent_wire_id)
                );
                assert_eq!(slot_wire(&route.slots.fable), Some(main));
            }
        }
    }
}

/// A3/A2 at the render site, the regression #1861 fixes: an *unpinned*
/// gateway launch now carries the descriptor's role mappings and the served
/// subagent model. Before the fix the gateway wrote no slot at all here.
#[test]
fn an_unpinned_gateway_launch_carries_the_descriptor_role_mappings() {
    let mut plan = unified(ModelProvider::DeepSeek);
    plan.model_selection = Some(selection_for(
        ModelProvider::DeepSeek,
        "deepseek-v4-pro[1m]",
    ));
    let route = resolve(&plan, &Ambient::default());
    assert!(route.allowlist.is_empty(), "this launch is unpinned");
    assert_eq!(
        route.slots.main.as_ref().map(|slot| slot.wire_id.as_str()),
        Some("deepseek-v4-pro[1m]")
    );

    let mut child = Vec::new();
    render_unified_env(&route, "http://gateway.invalid", "token", None).apply(&mut child);
    let value = |key: &str| {
        child
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    };
    // Single-profile provider: every role is the main row except haiku and
    // subagent, which take the served subagent model.
    assert_eq!(
        value("ANTHROPIC_DEFAULT_OPUS_MODEL"),
        Some("clud-claude-deepseek-v4-pro-0813")
    );
    assert_eq!(
        value("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
        Some("clud-claude-deepseek-flash")
    );
    assert_eq!(
        value("CLAUDE_CODE_SUBAGENT_MODEL"),
        Some("clud-claude-deepseek-flash")
    );
    assert_eq!(
        value("ANTHROPIC_DEFAULT_FABLE_MODEL"),
        Some("clud-claude-deepseek-v4-pro-0813")
    );
}

/// C14: cross-renderer slot parity. The direct route writes `wire_id` and the
/// gateway writes `discovery_id`; both must name the same catalog row, so a
/// gateway launch cannot serve a different model than the direct one it
/// mirrors.
#[test]
fn direct_and_gateway_slots_name_the_same_catalog_rows() {
    let mut plan = plan(ModelProvider::DeepSeek, Backend::Claude);
    plan.model_selection = Some(selection_for(
        ModelProvider::DeepSeek,
        "deepseek-v4-pro[1m]",
    ));
    let mut unified_plan = plan.clone();
    unified_plan.routing_mode = RoutingMode::Unified;

    let direct = resolve(&plan, &Ambient::default());
    let gateway = resolve(&unified_plan, &Ambient::default());
    assert_eq!(gateway.slots, direct.slots);

    for slot in [
        &gateway.slots.opus,
        &gateway.slots.sonnet,
        &gateway.slots.haiku,
        &gateway.slots.fable,
        &gateway.slots.subagent,
    ] {
        let slot = slot.as_ref().expect("the gateway carries every slot");
        let written = slot.discovery_id.as_deref().unwrap_or(&slot.wire_id);
        let via_wire = crate::provider_catalog::model_by_any_id(&slot.wire_id);
        let via_discovery = crate::provider_catalog::model_by_any_id(written);
        assert_eq!(
            via_wire.map(|row| row.cli_id),
            via_discovery.map(|row| row.cli_id),
            "{} and {} must name one row",
            slot.wire_id,
            written
        );
    }
}

/// A4: the credential is descriptor-derived. No test can make the resolver
/// invent a vault identifier, because it never carries one.
#[test]
fn every_descriptor_provider_resolves_to_its_own_vault_identifiers() {
    for descriptor in crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS {
        let route = resolve(
            &plan(descriptor.provider, Backend::Claude),
            &Ambient::default(),
        );
        let upstream = route
            .upstreams
            .first()
            .expect("a direct route has one upstream");
        assert_eq!(
            upstream.credential,
            CredentialSource::Vault(descriptor.provider)
        );
        assert_eq!(
            upstream.credential.vault_identifiers(),
            Some((descriptor.vault_service, descriptor.vault_account))
        );
    }
}

/// A5, the guard half: the copies step 6 deleted cannot come back. Each one
/// was a second place the same decision was made, and neither copy was visible
/// from the other -- which is how the two pin implementations drifted apart in
/// the first place (#1852).
#[test]
fn the_deleted_duplicates_cannot_return() {
    let sources = [
        (
            "failover.rs",
            include_str!("failover.rs"),
            "fn route_for(provider: ModelProvider)",
        ),
        (
            "codex_bridge.rs",
            include_str!("codex_bridge.rs"),
            "fn conversation_route_for(",
        ),
        (
            "foreground_runtime.rs",
            include_str!("foreground_runtime.rs"),
            "OPENROUTER_VAULT_SERVICE",
        ),
        (
            "foreground_runtime.rs",
            include_str!("foreground_runtime.rs"),
            "KIMI_VAULT_ACCOUNT",
        ),
        (
            "foreground_runtime.rs",
            include_str!("foreground_runtime.rs"),
            "const ANTHROPIC_COMPAT_CONFLICTING",
        ),
        (
            "foreground_runtime.rs",
            include_str!("foreground_runtime.rs"),
            "const CODEX_VIA_CLAUDE_CONFLICTING",
        ),
        (
            "foreground_runtime.rs",
            include_str!("foreground_runtime.rs"),
            "fn apply_anthropic_compat_overlay(",
        ),
        (
            "foreground_runtime.rs",
            include_str!("foreground_runtime.rs"),
            "fn apply_unified_overlay(",
        ),
        (
            "foreground_runtime.rs",
            include_str!("foreground_runtime.rs"),
            "fn apply_cross_route_overlay(",
        ),
        (
            "foreground_runtime.rs",
            include_str!("foreground_runtime.rs"),
            "fn codex_via_claude_bridge_allowlist(",
        ),
    ];
    for (file, source, needle) in sources {
        assert!(
            !source.contains(needle),
            "{file} carries {needle:?} again; that decision belongs to `route_plan`"
        );
    }
}

/// A5: one route mapping, total over providers.
#[test]
fn every_provider_maps_to_exactly_one_conversation_route() {
    let mut seen = Vec::new();
    for provider in ModelProvider::ALL {
        let route = conversation_route(*provider);
        assert!(
            !seen.contains(&route),
            "{} and another provider share {:?}",
            provider.as_str(),
            route
        );
        seen.push(route);
    }
    assert_eq!(seen.len(), ModelProvider::ALL.len());
}

/// A6: the context window is keyed on the wire id, so the same id yields the
/// same value on every backend (divergence #3), and an unknown id yields none.
#[test]
fn the_context_window_is_keyed_on_the_wire_id_on_every_backend() {
    let direct = resolve(
        &plan(ModelProvider::DeepSeek, Backend::Claude),
        &Ambient::default(),
    );
    assert_eq!(direct.context.auto_compact_window, Some(786_432));
    assert_eq!(direct.context.max_tokens, None);

    let unified = resolve(&unified(ModelProvider::DeepSeek), &Ambient::default());
    assert_eq!(unified.context, direct.context);

    let unknown = context_policy(Some("vendor/not-in-the-catalog"));
    assert_eq!(unknown, ContextPolicy::default());
}

/// A7: the per-turn capability is a function of the catalog row, so it is the
/// same on the direct and the unified route for the same wire id.
#[test]
fn the_per_turn_capability_is_identical_on_direct_and_unified() {
    let mut direct = with_selection(
        plan(ModelProvider::OpenRouter, Backend::Claude),
        "openrouter-claude-sonnet",
        ModelProvider::OpenRouter,
    );
    direct.allowed_models = vec!["openrouter-claude-sonnet".to_string()];
    let mut unified = direct.clone();
    unified.routing_mode = RoutingMode::Unified;

    let direct = resolve(&direct, &Ambient::default());
    let unified = resolve(&unified, &Ambient::default());
    assert_eq!(
        direct.slots.main.as_ref().unwrap().wire_id,
        "~anthropic/claude-sonnet-latest"
    );
    assert_eq!(direct.effort, unified.effort);
    assert_eq!(direct.slots, unified.slots);

    // The gate is off by default upstream; with it off nothing is advertised.
    let gated_off = resolve_with_catalog(
        &plan(ModelProvider::OpenRouter, Backend::Claude),
        &Ambient::default(),
        &catalog(),
        false,
    );
    assert_eq!(gated_off.effort.per_turn_capability, None);
}

/// The per-turn capability reaches OpenRouter's route only: DeepSeek and Kimi
/// direct routes hit their own vendor endpoints, so a claim about OpenRouter's
/// Messages API must never reach them (#1528).
#[test]
fn only_openrouter_routes_advertise_the_per_turn_capability() {
    fn gated_catalog(wire: &str, effort: bool) -> crate::openrouter_catalog::Catalog {
        let json = serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-10-05T00:00:00Z",
            "source": "https://openrouter.ai/api/v1/models",
            "models": [{
                "id": wire,
                "name": wire,
                "provider": "nvidia",
                "context_length": 262_144,
                "input_price_per_token": 1e-6,
                "output_price_per_token": 2e-6,
                "cached_input_price_per_token": null,
                "supports_tools": true,
                "supports_text_input": true,
                "supports_text_output": true,
                "supports_reasoning": true,
                "supports_reasoning_effort": effort,
                "supports_vision": false,
                "eligible_for_coding": true,
                "ineligibility_reasons": [],
            }],
        });
        crate::openrouter_catalog::Catalog::parse(json.to_string().as_bytes())
            .expect("fixture catalog parses")
    }

    let wire = "deepseek/deepseek-v4.1-flash";
    let catalog = gated_catalog(wire, true);
    let mut openrouter = with_selection(
        plan(ModelProvider::OpenRouter, Backend::Claude),
        "openrouter-claude-sonnet",
        ModelProvider::OpenRouter,
    );
    openrouter.model_selection.as_mut().unwrap().wire_model = Some(wire.to_string());
    let route = resolve_with_catalog(&openrouter, &Ambient::default(), &catalog, true);
    assert_eq!(
        route.effort.per_turn_capability.as_deref(),
        Some(format!("{wire}=per_turn_effort").as_str())
    );

    // The flip is off: nothing is advertised even for an admitting row.
    let dark = resolve_with_catalog(&openrouter, &Ambient::default(), &catalog, false);
    assert_eq!(dark.effort.per_turn_capability, None);

    // A row that does not admit the gate advertises nothing either.
    let ungated = resolve_with_catalog(
        &openrouter,
        &Ambient::default(),
        &gated_catalog(wire, false),
        true,
    );
    assert_eq!(ungated.effort.per_turn_capability, None);

    for descriptor in crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS {
        if descriptor.provider == ModelProvider::OpenRouter {
            continue;
        }
        let mut plan = plan(descriptor.provider, Backend::Claude);
        plan.model_selection = Some(selection_for(descriptor.provider, wire));
        let route = resolve_with_catalog(&plan, &Ambient::default(), &catalog, true);
        assert_eq!(
            route.effort.per_turn_capability, None,
            "{} must never carry an OpenRouter capability",
            descriptor.display_name
        );
    }
}

/// A selection whose wire spelling is `wire`, bypassing catalog validation:
/// these tests are about what a route does with an id, not about what the CLI
/// would accept.
fn selection_for(
    provider: ModelProvider,
    wire: &str,
) -> crate::provider_catalog::ResolvedModelSelection {
    crate::provider_catalog::ResolvedModelSelection {
        provider,
        model: None,
        wire_model: Some(wire.to_string()),
        effort: None,
        context_window: None,
        model_source: None,
        effort_source: None,
        context_window_source: None,
    }
}

/// A6/A7 at the render site, the regression #1860 fixes: a gateway route now
/// writes the same context window and per-turn capability the direct route
/// writes for the same wire id. Before the fix the unified overlay wrote
/// neither, so the harness clamped every gateway model at its own default.
#[test]
fn the_gateway_renders_the_same_context_and_effort_as_the_direct_route() {
    let mut direct = with_selection(
        plan(ModelProvider::OpenRouter, Backend::Claude),
        "openrouter-claude-sonnet",
        ModelProvider::OpenRouter,
    );
    direct.allowed_models = vec!["openrouter-claude-sonnet".to_string()];
    let mut unified = direct.clone();
    unified.routing_mode = RoutingMode::Unified;

    let direct = resolve(&direct, &Ambient::default());
    let unified = resolve(&unified, &Ambient::default());

    let value = |route: &ResolvedRoute, key: &str| -> Option<String> {
        let mut env = Vec::new();
        match route.backend {
            RouteBackend::Unified => {
                render_unified_env(route, "http://gateway.invalid", "token", None)
            }
            _ => render_direct_env(route, "token"),
        }
        .apply(&mut env);
        env.iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.clone())
    };

    assert_eq!(direct.context.max_tokens, Some(1_000_000));
    assert_eq!(unified.context, direct.context);
    assert_eq!(
        value(&unified, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
        value(&direct, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
        "the gateway must not clamp where the direct route does not"
    );
    assert_eq!(
        value(&unified, "CLAUDE_CODE_MODEL_CAPABILITIES"),
        value(&direct, "CLAUDE_CODE_MODEL_CAPABILITIES")
    );
    // The fixture catalog admits the gate and this test resolves with it on,
    // so both routes advertise the same token -- and with the gate off (the
    // shipped default) neither does.
    assert_eq!(
        value(&unified, "CLAUDE_CODE_MODEL_CAPABILITIES").as_deref(),
        Some("~anthropic/claude-sonnet-latest=per_turn_effort")
    );
    let mut dark = direct.clone();
    dark.effort.per_turn_capability = None;
    let mut env = Vec::new();
    render_direct_env(&dark, "token").apply(&mut env);
    assert!(env
        .iter()
        .all(|(key, _)| key != "CLAUDE_CODE_MODEL_CAPABILITIES"));
}

/// A8: resolving twice from the same inputs gives equal values, and the route
/// survives the JSON round trip a daemon worker performs.
#[test]
fn resolving_is_deterministic_and_survives_the_plan_round_trip() {
    let plan = with_selection(
        unified(ModelProvider::OpenRouter),
        "openrouter-claude-sonnet",
        ModelProvider::OpenRouter,
    );
    let first = resolve(&plan, &Ambient::default());
    let second = resolve(&plan, &Ambient::default());
    assert_eq!(first, second);

    let mut carried = plan.clone();
    carried.route = Some(first.clone());
    let json = serde_json::to_string(&carried).expect("a plan serializes");
    let restored: LaunchPlan = serde_json::from_str(&json).expect("a plan round trips");
    assert_eq!(restored.route, Some(first));
}

/// A pre-#1855 payload has no `route` key at all; it must still parse.
#[test]
fn a_plan_without_a_route_still_parses() {
    let mut plan = plan(ModelProvider::Claude, Backend::Claude);
    plan.route = None;
    let json = serde_json::to_string(&plan).expect("a plan serializes");
    assert!(!json.contains("\"route\""), "{json}");
    let restored: LaunchPlan = serde_json::from_str(&json).expect("a pre-#1855 plan parses");
    assert_eq!(restored.route, None);
}

/// C12: a route names where a credential comes from and never what it is. The
/// serialized credential is a provider name, so no field of a route can carry
/// a key even by accident.
#[test]
fn every_route_serializes_its_credentials_as_names_never_secrets() {
    let route = resolve(
        &plan(ModelProvider::OpenRouter, Backend::Claude),
        &Ambient::default(),
    );
    assert_eq!(
        serde_json::to_string(&route.upstreams[0].credential).expect("a credential serializes"),
        r#"{"source":"vault","value":"openrouter"}"#
    );
    let unified = resolve(&unified(ModelProvider::Claude), &Ambient::default());
    let rendered = serde_json::to_string(&unified).expect("a route serializes");
    for needle in [
        "api-key-v1",
        "clud.openrouter",
        "clud.deepseek",
        "clud.kimi",
    ] {
        assert!(!rendered.contains(needle), "{needle} reached {rendered}");
    }
}

/// The applicability table, asserted exhaustively: a new backend that forgets
/// to classify a check fails here.
#[test]
fn every_check_is_classified_for_every_backend() {
    let plans = [
        (
            plan(ModelProvider::DeepSeek, Backend::Claude),
            RouteBackend::Direct,
        ),
        (
            plan(ModelProvider::OpenRouter, Backend::Claude),
            RouteBackend::Direct,
        ),
        (unified(ModelProvider::OpenRouter), RouteBackend::Unified),
        (
            plan(ModelProvider::Codex, Backend::Claude),
            RouteBackend::CodexBridge,
        ),
        (
            plan(ModelProvider::DeepSeek, Backend::DeepSeek),
            RouteBackend::DeepSeekNative,
        ),
        (
            plan(ModelProvider::Claude, Backend::Claude),
            RouteBackend::Native,
        ),
    ];
    for (plan, expected) in plans {
        let route = resolve(&plan, &Ambient::default());
        assert_eq!(route.backend, expected);
        let names: Vec<&str> = route
            .checks
            .iter()
            .map(|check| check.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "request_path_features",
                "openrouter_free_cost",
                "provider_only",
                "secret_redaction"
            ]
        );
        for check in &route.checks {
            assert!(!check.verdict.is_empty(), "{} has no verdict", check.name);
        }
    }

    // `--provider-only` is where it is meaningful and refused where it is not.
    let mut refused = plan(ModelProvider::Codex, Backend::Claude);
    refused.provider_only = vec!["deepinfra".to_string()];
    let route = resolve(&refused, &Ambient::default());
    let check = route
        .checks
        .iter()
        .find(|check| check.name == "provider_only")
        .expect("the check is classified");
    assert!(matches!(check.applicability, Applicability::Unsupported(_)));
    assert!(check.verdict.contains("refused"), "{}", check.verdict);
}

/// The `:free` verdict is catalog data, and the route says where it ran.
#[test]
fn a_free_wire_id_is_classified_from_the_offline_catalog() {
    let mut plan = with_selection(
        plan(ModelProvider::OpenRouter, Backend::Claude),
        "acme/missing:free",
        ModelProvider::OpenRouter,
    );
    plan.model_selection.as_mut().unwrap().wire_model = Some("acme/missing:free".to_string());
    let route = resolve(&plan, &Ambient::default());
    let check = route
        .checks
        .iter()
        .find(|check| check.name == "openrouter_free_cost")
        .expect("the check is classified");
    assert_eq!(check.applicability, Applicability::Applies);
    assert!(check.verdict.starts_with("refused:"), "{}", check.verdict);
    // The refusal is what the launch fails with, and it names the fix.
    let verdict = evaluate(&route, &catalog())
        .into_iter()
        .find(|verdict| verdict.check.name == "openrouter_free_cost")
        .expect("the check is evaluated");
    let refusal = verdict.refusal.expect("an uncatalogued `:free` id refuses");
    assert!(refusal.contains("cannot confirm it is free"), "{refusal}");
}

/// B11/C17: `--provider-only` produces one `ProviderRouting` value, valid on
/// both routes that can carry it and refused -- naming the backend -- where
/// they cannot.
#[test]
fn provider_only_is_one_value_on_direct_and_unified() {
    let mut direct = plan(ModelProvider::OpenRouter, Backend::Claude);
    direct.provider_only = vec!["parasail/fp8".to_string()];
    let mut unified = direct.clone();
    unified.routing_mode = RoutingMode::Unified;

    let direct = resolve(&direct, &Ambient::default());
    let unified = resolve(&unified, &Ambient::default());
    assert_eq!(direct.upstream_routing, unified.upstream_routing);

    let catalog = catalog();
    for route in [&direct, &unified] {
        let check = evaluate(route, &catalog)
            .into_iter()
            .find(|verdict| verdict.check.name == "provider_only")
            .expect("classified");
        assert_eq!(check.check.applicability, Applicability::Applies);
        assert_eq!(check.refusal, None);
    }

    // A backend that cannot carry the object refuses it, and the refusal names
    // the check.
    let mut bridge = plan(ModelProvider::Codex, Backend::Claude);
    bridge.provider_only = vec!["parasail/fp8".to_string()];
    let bridge = resolve(&bridge, &Ambient::default());
    let refusal = first_refusal(&bridge, &catalog).expect("the bridge refuses the pin");
    assert!(refusal.starts_with("provider_only:"), "{refusal}");
    assert!(
        refusal.contains("only to an OpenRouter launch clud routes"),
        "{refusal}"
    );
}

/// The scrub list each backend owns, including the prefix rule.
#[test]
fn each_backend_scrubs_the_keys_it_owns() {
    let direct = resolve(
        &plan(ModelProvider::OpenRouter, Backend::Claude),
        &Ambient::default(),
    );
    assert!(direct
        .scrub
        .iter()
        .any(|key| key.key == "OPENROUTER_API_KEY"));
    assert!(direct
        .scrub
        .iter()
        .any(|key| key.key == "ANTHROPIC_API_KEY"));
    assert!(!direct
        .scrub
        .iter()
        .any(|key| key.key == "CLAUDE_CODE_EFFORT_LEVEL"));
    // The direct overlay's rule is the security guarantee, not OS semantics.
    assert!(direct
        .scrub
        .iter()
        .all(|key| key.mode == ScrubMode::AnyCase || key.mode == ScrubMode::Prefix));

    let unified = resolve(&unified(ModelProvider::Claude), &Ambient::default());
    assert!(unified
        .scrub
        .iter()
        .any(|key| key.key == "ANTHROPIC_BASE_URL"));
    assert!(
        !unified
            .scrub
            .iter()
            .any(|key| key.key == "ANTHROPIC_AUTH_TOKEN"),
        "the gateway keeps the Claude credential so native auth reaches the upstream"
    );

    let native = resolve(
        &plan(ModelProvider::Claude, Backend::Claude),
        &Ambient::default(),
    );
    assert_eq!(native.scrub, Vec::<ScrubKey>::new());
}

/// The prefix rule: `ANTHROPIC_CUSTOM_MODEL_OPTION` has no fixed suffix, so a
/// rewrite that only matched exact keys would leave the user's own options
/// behind.
#[test]
fn the_custom_model_option_prefix_matches_every_suffix() {
    let prefix = ScrubKey {
        key: CUSTOM_MODEL_OPTION_PREFIX.to_string(),
        mode: ScrubMode::Prefix,
    };
    assert!(prefix.matches("ANTHROPIC_CUSTOM_MODEL_OPTION"));
    assert!(prefix.matches("anthropic_custom_model_option_sonnet"));
    assert!(prefix.matches("ANTHROPIC_CUSTOM_MODEL_OPTION_HAIKU_NAME"));
    assert!(!prefix.matches("ANTHROPIC_DEFAULT_HAIKU_MODEL"));
}

/// C15: a renderer removes every key the route owns, writes its entries, and
/// never touches the environment it was handed a reference to.
#[test]
fn the_overlay_applies_the_scrub_and_never_mutates_its_parent() {
    let route = resolve(
        &plan(ModelProvider::OpenRouter, Backend::Claude),
        &Ambient::default(),
    );
    let parent = vec![
        (
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "ambient-subagent".to_string(),
        ),
        ("ANTHROPIC_API_KEY".to_string(), "sk-ambient".to_string()),
        ("UNCHANGED".to_string(), "yes".to_string()),
    ];
    let untouched = parent.clone();
    let mut child = parent.clone();
    render_direct_env(&route, "fixture-secret").apply(&mut child);

    assert_eq!(parent, untouched, "the parent environment was mutated");
    assert_eq!(
        child
            .iter()
            .find(|(key, _)| key == "UNCHANGED")
            .map(|(_, value)| value.as_str()),
        Some("yes")
    );
    // OpenRouter needs the key present but empty: merely removing an inherited
    // one makes Claude Code fall back to Anthropic.
    assert_eq!(
        child
            .iter()
            .find(|(key, _)| key == "ANTHROPIC_API_KEY")
            .map(|(_, value)| value.as_str()),
        Some("")
    );
    assert!(!child.iter().any(|(_, value)| value == "sk-ambient"));
    assert_eq!(
        child
            .iter()
            .filter(|(key, _)| key == "ANTHROPIC_AUTH_TOKEN")
            .count(),
        1
    );
    // The route owns the subagent slot, so the ambient spelling is replaced by
    // the resolved one rather than surviving.
    assert_eq!(
        child
            .iter()
            .find(|(key, _)| key == "CLAUDE_CODE_SUBAGENT_MODEL")
            .map(|(_, value)| value.as_str()),
        Some("~anthropic/claude-opus-latest")
    );
}

/// C18: the timeout is the route's, and an ambient value still wins.
#[test]
fn the_direct_timeout_comes_from_the_route() {
    let route = resolve(
        &plan(ModelProvider::DeepSeek, Backend::Claude),
        &Ambient::default(),
    );
    assert_eq!(route.timeout_ms, Some(600_000));
    let mut child = Vec::new();
    render_direct_env(&route, "fixture-secret").apply(&mut child);
    assert_eq!(
        child
            .iter()
            .find(|(key, _)| key == "API_TIMEOUT_MS")
            .map(|(_, value)| value.as_str()),
        Some("600000")
    );

    let mut ambient = vec![("API_TIMEOUT_MS".to_string(), "120000".to_string())];
    render_direct_env(&route, "fixture-secret").apply(&mut ambient);
    assert_eq!(
        ambient
            .iter()
            .find(|(key, _)| key == "API_TIMEOUT_MS")
            .map(|(_, value)| value.as_str()),
        Some("120000")
    );
}

/// C17: `--provider-only` renders into `CLAUDE_CODE_EXTRA_BODY`, merging the
/// keys already there.
#[test]
fn a_provider_only_pin_is_the_routes_upstream_routing() {
    let mut plan = plan(ModelProvider::OpenRouter, Backend::Claude);
    plan.provider_only = vec!["deepinfra".to_string()];
    let route = resolve(&plan, &Ambient::default());
    assert_eq!(
        route.upstream_routing,
        Some(ProviderRouting {
            only: vec!["deepinfra".to_string()],
            allow_fallbacks: false,
            zdr: false,
            data_collection: None,
        })
    );
}

#[test]
fn openrouter_privacy_policy_survives_serialization() {
    let policy = serde_json::json!({
        "only": [], "allow_fallbacks": true, "zdr": true, "data_collection": "deny"
    });
    let routing: ProviderRouting = serde_json::from_value(policy.clone()).unwrap();
    assert_eq!(serde_json::to_value(routing).unwrap(), policy);
}

#[test]
fn openrouter_privacy_flags_keep_fallbacks_until_a_provider_is_pinned() {
    let mut plan = plan(ModelProvider::OpenRouter, Backend::Claude);
    plan.zdr = true;
    plan.no_data_collection = true;
    let route = resolve(&plan, &Ambient::default());
    let routing = route.upstream_routing.unwrap();
    assert!(routing.zdr);
    assert_eq!(routing.data_collection.as_deref(), Some("deny"));
    assert!(routing.allow_fallbacks);
    plan.provider_only.push("parasail/fp8".to_string());
    let pinned = resolve(&plan, &Ambient::default())
        .upstream_routing
        .unwrap();
    assert!(!pinned.allow_fallbacks);
    assert!(pinned.zdr);
}

/// C14/D20: the bridge renders its slots and boundary from the route, and the
/// config it hands `BridgeConfig` is exactly that boundary.
#[test]
fn the_codex_bridge_renders_its_slots_and_boundary_from_the_route() {
    let mut plan = plan(ModelProvider::Codex, Backend::Claude);
    plan.codex_model = Some("codex-terra".to_string());
    plan.allowed_models = vec!["codex-terra".to_string()];
    let route = resolve(&plan, &Ambient::default());

    let render = render_codex_bridge(
        &route,
        CodexBridgeRequest {
            default_model: crate::codex_model::ModelSpec::parse("codex-terra").ok(),
            selected_model_cli_id: Some("codex-terra"),
        },
    );
    assert_eq!(render.allowed_models, route.allowlist);
    assert_eq!(render.selected_model_cli_id.as_deref(), Some("codex-terra"));

    let overlay = render_codex_bridge_env(&route, "http://gateway.invalid", "fixture-token");
    let mut child = Vec::new();
    overlay.apply(&mut child);
    let value = |key: &str| {
        child
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    };
    let opus = route.slots.opus.as_ref().expect("the bridge pins opus");
    assert_eq!(
        value("ANTHROPIC_DEFAULT_OPUS_MODEL"),
        Some(opus.wire_id.as_str())
    );
    assert_eq!(
        value("ANTHROPIC_DEFAULT_OPUS_MODEL_NAME"),
        opus.display_name.as_deref()
    );
    let sonnet = route.slots.sonnet.as_ref().expect("the bridge pins sonnet");
    assert_eq!(
        value("ANTHROPIC_DEFAULT_SONNET_MODEL"),
        Some(sonnet.wire_id.as_str())
    );
    // The process-wide ceiling the catalog proves across every Codex row.
    assert_eq!(value("CLAUDE_CODE_MAX_CONTEXT_TOKENS"), Some("1050000"));
    assert_eq!(
        value("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
        Some("1")
    );
    assert_eq!(value("API_TIMEOUT_MS"), Some("3000000"));
    // Haiku is deliberately left alone: the bridge has no Codex tier for it.
    assert_eq!(value("ANTHROPIC_DEFAULT_HAIKU_MODEL"), None);
    assert_eq!(value("ANTHROPIC_AUTH_TOKEN"), Some("fixture-token"));
}

/// The unified route lists every upstream it may serve, and the direct route
/// exactly one. Which of them can answer right now is a launch-time credential
/// probe, deliberately not a resolution input.
#[test]
fn the_unified_route_lists_a_credential_source_per_upstream() {
    let route = resolve(&unified(ModelProvider::Claude), &Ambient::default());
    let providers: Vec<ModelProvider> = route
        .upstreams
        .iter()
        .map(|upstream| upstream.provider)
        .collect();
    assert_eq!(
        providers,
        vec![
            ModelProvider::Claude,
            ModelProvider::DeepSeek,
            ModelProvider::Kimi,
            ModelProvider::OpenRouter,
            ModelProvider::Codex,
        ]
    );
    assert_eq!(route.upstreams[0].credential, CredentialSource::Harness);
    assert_eq!(route.upstreams[4].credential, CredentialSource::CodexAuth);
    for (index, upstream) in route.upstreams.iter().enumerate() {
        assert_eq!(
            upstream.conversation_route,
            conversation_route(upstream.provider),
            "upstream {index} disagrees with the one route mapping"
        );
    }
}

/// The DeepSeek-native harness keeps its key in the environment, and the route
/// says so explicitly instead of hiding it behind a vault id.
#[test]
fn the_deepseek_native_route_names_its_environment_credential() {
    let route = resolve(
        &plan(ModelProvider::DeepSeek, Backend::DeepSeek),
        &Ambient::default(),
    );
    assert_eq!(route.credential_env(), Some("DEEPSEEK_API_KEY"));

    let openrouter = resolve(
        &plan(ModelProvider::OpenRouter, Backend::DeepSeek),
        &Ambient::default(),
    );
    assert_eq!(openrouter.credential_env(), Some("OPENROUTER_API_KEY"));

    // The render says the same thing, and never a vault id: this is the
    // assertion that fails if dsh ever silently switches to the vault path.
    let render = render_dsh(&openrouter);
    assert_eq!(render.key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    assert_eq!(
        render.base_url.as_deref(),
        Some("https://openrouter.ai/api")
    );
    assert_eq!(
        render.wire_model.as_deref(),
        Some("~anthropic/claude-sonnet-latest")
    );
    assert!(render
        .key_env
        .as_deref()
        .is_some_and(|var| var.ends_with("_API_KEY")));
    assert_eq!(
        openrouter.upstreams[0].credential.vault_identifiers(),
        None,
        "dsh's credential is the environment, not clud's vault"
    );
}

#[test]
fn the_direct_test_upstream_needs_the_integration_gate() {
    // #1984: an end-to-end test points `clud --openrouter` at a scripted
    // local model; nothing else may move a direct route's upstream.
    let env = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect::<Vec<_>>()
    };
    let url = "http://127.0.0.1:9/api";
    let gated = Ambient::from_child_env(&env(&[
        ("CLUD_INTEGRATION_TESTS", "1"),
        (TEST_DIRECT_UPSTREAM_ENV, url),
    ]));
    assert_eq!(gated.direct_upstream_url.as_deref(), Some(url));
    let ungated = Ambient::from_child_env(&env(&[(TEST_DIRECT_UPSTREAM_ENV, url)]));
    assert_eq!(ungated.direct_upstream_url, None);
}

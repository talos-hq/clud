use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::backend::{
    Backend, HarnessSelection, LaunchMode, ModelProvider, PreferenceSource, RoutingMode,
};
use crate::graphics::GraphicsConfig;
use crate::provider_catalog::ResolvedModelSelection;

/// A single non-interactive provider turn owned by the daemon session API.
/// This narrow typed input deliberately excludes raw argv and environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessTurnRequest {
    pub message: String,
    pub cwd: PathBuf,
    pub session: HeadlessSession,
}

/// Whether a headless turn creates or resumes a provider conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessSession {
    /// Claude needs a caller-assigned UUID; Codex creates its own thread.
    Initial { claude_session_id: Option<String> },
    /// An ID captured from an earlier provider event.
    Resume { provider_session_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub command: Vec<String>,
    /// Explicit per-launch opt-out of clud's agent safety policy.
    #[serde(default)]
    pub unsafe_mode: bool,
    /// Explicit opt-out of the runtime loop-scoped plan-mode guard.
    #[serde(default)]
    pub allow_plan_mode: bool,
    pub iterations: u32,
    pub backend: Backend,
    #[serde(default)]
    pub routing_mode: RoutingMode,
    /// Additive provider/harness metadata. `None` means a pre-#625 payload;
    /// consumers must fall back to the legacy `backend` field.
    #[serde(default)]
    pub model_provider: Option<ModelProvider>,
    #[serde(default)]
    pub requested_harness: Option<HarnessSelection>,
    #[serde(default)]
    pub effective_harness: Option<Backend>,
    #[serde(default)]
    pub provider_source: Option<PreferenceSource>,
    #[serde(default)]
    pub harness_source: Option<PreferenceSource>,
    pub launch_mode: LaunchMode,
    pub cwd: Option<String>,
    #[serde(default)]
    pub graphics: GraphicsConfig,
    #[serde(default)]
    pub repeat_schedule: Option<RepeatSchedule>,
    #[serde(default)]
    pub task_summary: Option<String>,
    /// When set, the outer loop should poll for DONE/BLOCKED marker files
    /// after each iteration and terminate accordingly.
    #[serde(default)]
    pub loop_markers: Option<LoopMarkers>,
    /// When set, claude is being invoked with `--output-format stream-json
    /// --verbose` and the subprocess runner should route its captured stdout
    /// through `stream_json::render_line` so the user sees live progress.
    #[serde(default)]
    pub stream_json_progress: bool,
    /// Foreground print turns use compact classified activity and full final stdout.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prompt_summary: bool,
    /// Legacy Codex model+effort compatibility selection, canonicalized as
    /// `gpt-5.6-terra@high`. New plans use `model_selection` and emit a
    /// harness-facing discovery ID; this field remains for old daemon payloads
    /// and continued sessions that still carry the compound spelling.
    #[serde(default)]
    pub codex_model: Option<String>,
    /// Additive normalized selection. Older daemon payloads retain `codex_model`.
    #[serde(default)]
    pub model_selection: Option<ResolvedModelSelection>,
    /// Ordered fallback routes for a unified launch, exactly as written on the
    /// command line. Resolved when the gateway is constructed so an unroutable
    /// rung fails the launch rather than a turn (#968).
    #[serde(default)]
    pub failover: Option<String>,
    /// Consent to descend onto a rung billed per token.
    #[serde(default)]
    pub failover_allow_metered: bool,
    /// Every model this launch is allowed to reach (#1257): the haiku and
    /// subagent slots, the rows gateway discovery may advertise, and what a
    /// bridge will serve. Empty means unconstrained. A serde default keeps
    /// old worker payloads readable.
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// OpenRouter upstream providers every request must use, fallbacks off
    /// (`--provider-only`); empty leaves OpenRouter's routing alone.
    #[serde(default)]
    pub provider_only: Vec<String>,
    /// Require OpenRouter zero-data-retention endpoints.
    #[serde(default)]
    pub zdr: bool,
    /// Exclude OpenRouter endpoints that collect request data.
    #[serde(default)]
    pub no_data_collection: bool,
    /// Explicit reversible-redaction override; None uses route policy.
    #[serde(default)]
    pub redact_secrets: Option<bool>,
    /// True when `allowed_models` came from the *previous* model selection
    /// (no `--model` / `--allow-model` on the command line), not from an
    /// explicit pin (#1257). The runtime announces exactly that case as a
    /// green startup line: the boundary exists, the user did not type it.
    #[serde(default)]
    pub pinned_from_previous_selection: bool,
    /// Claude commit/PR attribution for this launch (#1317). Hidden unless
    /// `--coauthor[=TAG]` or `CLUD_COAUTHOR` opts in; the serde default keeps
    /// old worker payloads on the same default.
    #[serde(default)]
    pub coauthor: crate::attribution::Coauthor,
    /// Every routing decision this launch made, resolved once (#1855). A
    /// serde default keeps pre-#1855 daemon payloads readable; a worker that
    /// receives one uses it rather than re-deriving a second copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<crate::route_plan::ResolvedRoute>,
}

impl LaunchPlan {
    pub fn model_provider(&self) -> ModelProvider {
        self.model_provider
            .unwrap_or_else(|| self.backend.as_model_provider())
    }

    pub fn effective_harness(&self) -> Backend {
        self.effective_harness.unwrap_or(self.backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_with_metadata(
        provider: ModelProvider,
        requested: HarnessSelection,
        effective: Backend,
        provider_source: PreferenceSource,
        harness_source: PreferenceSource,
    ) -> LaunchPlan {
        LaunchPlan {
            unsafe_mode: false,
            allow_plan_mode: false,
            command: vec![effective.executable_name().to_string()],
            iterations: 1,
            backend: effective,
            routing_mode: RoutingMode::Direct,
            model_provider: Some(provider),
            requested_harness: Some(requested),
            effective_harness: Some(effective),
            provider_source: Some(provider_source),
            harness_source: Some(harness_source),
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
    fn launch_plan_metadata_round_trips_for_default_cli_and_saved_routes() {
        let cases = [
            (
                ModelProvider::Claude,
                HarnessSelection::Default,
                Backend::Claude,
                PreferenceSource::BuiltInDefault,
                PreferenceSource::BuiltInDefault,
            ),
            (
                ModelProvider::Codex,
                HarnessSelection::Claude,
                Backend::Claude,
                PreferenceSource::Cli,
                PreferenceSource::Cli,
            ),
            (
                ModelProvider::Codex,
                HarnessSelection::Claude,
                Backend::Claude,
                PreferenceSource::GlobalSetting,
                PreferenceSource::GlobalSetting,
            ),
            (
                ModelProvider::DeepSeek,
                HarnessSelection::Default,
                Backend::Claude,
                PreferenceSource::Cli,
                PreferenceSource::BuiltInDefault,
            ),
        ];

        for (provider, requested, effective, provider_source, harness_source) in cases {
            let plan = plan_with_metadata(
                provider,
                requested,
                effective,
                provider_source,
                harness_source,
            );
            let json = serde_json::to_value(&plan).unwrap();
            assert_eq!(json["model_provider"], provider.as_str());
            assert_eq!(json["requested_harness"], requested.as_str());
            assert_eq!(
                json["effective_harness"],
                serde_json::to_value(effective).unwrap()
            );
            assert_eq!(json["provider_source"], provider_source.as_str());
            assert_eq!(json["harness_source"], harness_source.as_str());

            let decoded: LaunchPlan = serde_json::from_value(json).unwrap();
            assert_eq!(decoded.model_provider, Some(provider));
            assert_eq!(decoded.requested_harness, Some(requested));
            assert_eq!(decoded.effective_harness, Some(effective));
            assert_eq!(decoded.provider_source, Some(provider_source));
            assert_eq!(decoded.harness_source, Some(harness_source));
        }
    }

    #[test]
    fn old_launch_plan_payload_defaults_new_metadata_to_legacy_backend() {
        let plan: LaunchPlan = serde_json::from_str(
            r#"{
                "command":["codex"],
                "iterations":1,
                "backend":"Codex",
                "launch_mode":"subprocess",
                "cwd":null,
                "stream_json_progress":false
            }"#,
        )
        .unwrap();
        assert_eq!(plan.model_provider, None);
        assert_eq!(plan.requested_harness, None);
        assert_eq!(plan.effective_harness, None);
        assert_eq!(plan.provider_source, None);
        assert_eq!(plan.harness_source, None);
        assert_eq!(plan.model_provider(), ModelProvider::Codex);
        assert_eq!(plan.effective_harness(), Backend::Codex);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopMarkers {
    pub done_path: String,
    pub blocked_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepeatSchedule {
    pub interval_secs: u64,
}

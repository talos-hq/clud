//! Gemini model-discovery surface: `GET /v1beta/models` and
//! `GET /v1beta/models/<model>`.
//!
//! CLIProxyAPI opens both doors beside the Gemini action route
//! (`internal/api/server_routes.go:134` `v1beta.GET("/models", ...)` and `:137`
//! `v1beta.GET("/models/*action", ...)`), answered by
//! `sdk/api/handlers/gemini/gemini_handlers.go: GeminiModels` (`:…`) and
//! `GeminiGetHandler`. A Gemini SDK client lists models before it calls one, so
//! without these two routes a Gemini-shaped client cannot discover a single
//! model even though the `generateContent` door answers.
//!
//! Deliberate differences from upstream, all documented rather than silent:
//!
//! * **A listed name is a servable name.** Upstream rewrites a row's `name` to
//!   `models/<id>` for discovery but leaves the registry id in place when it
//!   generates; clud advertises the catalog's discovery id behind the `models/`
//!   prefix and the action route strips that prefix back off, so what discovery
//!   hands a client is exactly what generation accepts.
//! * **`streamGenerateContent` is advertised**, because this endpoint serves it
//!   (`POST /v1beta/models/<model>:streamGenerateContent`). Upstream's default
//!   method list is `["generateContent"]`.
//! * **`description` carries the display name** instead of upstream's echo of
//!   the raw id. `inputTokenLimit` is the catalog's reviewed context ceiling
//!   (`claude_max_context_tokens`) and is omitted when the row has none, rather
//!   than invented; `outputTokenLimit` is omitted for the same reason.
//! * **No pagination.** `pageSize`/`pageToken` are accepted and ignored: the
//!   catalog is a compile-time list and upstream does not paginate either.
//! * **The listing is the route's advertised set**, not every catalog row — the
//!   same provider-availability and #1257 allowlist filters `GET /v1/models`
//!   applies, so a row cannot be discoverable here and unservable there.

use serde_json::{json, Value};

use crate::provider_catalog::CatalogModel;

/// The list door: `GET /v1beta/models`.
pub const LIST_PATH: &str = "/v1beta/models";

/// Every action route lives under this prefix, including both GET doors.
pub const MODEL_ROUTE_PREFIX: &str = "/v1beta/models/";

/// Generation methods [`row_json`] advertises for every row.
pub const SERVED_METHODS: [&str; 2] = ["generateContent", "streamGenerateContent"];

/// What a `GET` request under the models prefix is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelQuery<'a> {
    /// `GET /v1beta/models` -- the whole advertised set.
    List,
    /// `GET /v1beta/models/<name>` -- one row, spelled as the client sent it
    /// (with or without the leading `models/`).
    Detail(&'a str),
    /// A GET under the prefix with no model name at all.
    MissingName,
}

/// Classify a `GET` path. `None` means the path is not on this surface at all.
pub fn parse_model_query(path: &str) -> Option<ModelQuery<'_>> {
    if path == LIST_PATH {
        return Some(ModelQuery::List);
    }
    let rest = path.strip_prefix(MODEL_ROUTE_PREFIX)?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return Some(ModelQuery::MissingName);
    }
    Some(ModelQuery::Detail(rest))
}

/// The Gemini name for a catalog row: `models/<discovery id>`.
///
/// `None` for a row that has no discovery id — such a row is never advertised,
/// so it has no name a client could ask for.
pub fn model_name(entry: &CatalogModel) -> Option<String> {
    entry.discovery_id.map(|id| format!("models/{id}"))
}

/// The advertised row a client asked for.
///
/// Gemini clients echo the name they read from the listing (`models/<id>`) and
/// older ones send the bare id, so both spellings have to resolve. Matching is
/// exact after the prefix: a partial or differently-cased name is not a row.
pub fn find<'a>(rows: &[&'a CatalogModel], requested: &str) -> Option<&'a CatalogModel> {
    let wanted = requested.strip_prefix("models/").unwrap_or(requested);
    rows.iter()
        .copied()
        .find(|entry| entry.discovery_id.is_some_and(|id| id == wanted))
}

/// One model row in Gemini's `Model` shape, or `None` for a nameless row.
pub fn row_json(entry: &CatalogModel) -> Option<Value> {
    let name = model_name(entry)?;
    let mut row = json!({
        "name": name,
        "displayName": entry.display_name,
        "description": entry.display_name,
        "supportedGenerationMethods": SERVED_METHODS,
    });
    if let Some(tokens) = entry.claude_max_context_tokens {
        row["inputTokenLimit"] = json!(tokens);
    }
    Some(row)
}

/// `{"models": [...]}` for the list door.
pub fn list_body(rows: &[&CatalogModel]) -> Vec<u8> {
    let models = rows
        .iter()
        .filter_map(|entry| row_json(entry))
        .collect::<Vec<_>>();
    serde_json::to_vec(&json!({ "models": models })).unwrap_or_default()
}

/// A single model object for the detail door, or `None` for a nameless row.
pub fn detail_body(entry: &CatalogModel) -> Option<Vec<u8>> {
    let row = row_json(entry)?;
    serde_json::to_vec(&row).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_catalog;

    fn row(id: &str) -> CatalogModel {
        provider_catalog::model_by_discovery_id(id).expect("catalog row")
    }

    #[test]
    fn list_path_is_classified_as_the_list_door() {
        assert_eq!(parse_model_query("/v1beta/models"), Some(ModelQuery::List));
    }

    #[test]
    fn a_model_path_is_classified_as_a_detail_lookup() {
        assert_eq!(
            parse_model_query("/v1beta/models/models/clud-claude-codex-sol"),
            Some(ModelQuery::Detail("models/clud-claude-codex-sol"))
        );
        assert_eq!(
            parse_model_query("/v1beta/models/clud-claude-codex-sol"),
            Some(ModelQuery::Detail("clud-claude-codex-sol"))
        );
    }

    #[test]
    fn a_trailing_slash_still_names_its_model() {
        assert_eq!(
            parse_model_query("/v1beta/models/clud-claude-codex-sol/"),
            Some(ModelQuery::Detail("clud-claude-codex-sol"))
        );
    }

    #[test]
    fn an_empty_name_under_the_prefix_is_not_a_model() {
        assert_eq!(
            parse_model_query("/v1beta/models/"),
            Some(ModelQuery::MissingName)
        );
    }

    #[test]
    fn a_path_outside_the_models_prefix_is_not_this_surface() {
        assert_eq!(parse_model_query("/v1/models"), None);
        assert_eq!(parse_model_query("/v1beta/other"), None);
        // The action route is a POST on the same prefix: a GET must not be
        // mistaken for a model name.
        assert_eq!(
            parse_model_query("/v1beta/models/claude-test:generateContent"),
            Some(ModelQuery::Detail("claude-test:generateContent"))
        );
    }

    #[test]
    fn every_advertised_row_has_a_gemini_name() {
        let entry = row("clud-claude-codex-sol");
        assert_eq!(
            model_name(&entry).as_deref(),
            Some("models/clud-claude-codex-sol")
        );
    }

    #[test]
    fn a_listed_row_carries_the_fields_a_gemini_client_reads() {
        let entry = row("clud-claude-codex-sol");
        let value = row_json(&entry).expect("row");
        assert_eq!(value["name"], "models/clud-claude-codex-sol");
        assert_eq!(value["displayName"], entry.display_name);
        assert_eq!(value["supportedGenerationMethods"][0], "generateContent");
        assert_eq!(
            value["supportedGenerationMethods"][1],
            "streamGenerateContent"
        );
        assert_eq!(
            value["inputTokenLimit"],
            json!(entry.claude_max_context_tokens.expect("context ceiling"))
        );
    }

    /// The name a listing hands out is the name a detail lookup resolves.
    /// (The action route's half of the same contract is pinned in
    /// `codex_bridge::generate_content_route_accepts_the_name_discovery_advertises`.)
    #[test]
    fn the_listed_name_resolves_back_to_its_row() {
        let entry = row("clud-claude-codex-sol");
        let name = model_name(&entry).expect("name");
        let resolved = find(&[&entry], &name).expect("row resolves");
        assert_eq!(resolved.discovery_id, entry.discovery_id);
    }

    #[test]
    fn a_detail_lookup_accepts_both_spellings() {
        let entry = row("clud-claude-kimi-k3");
        let rows = [&entry];
        assert!(find(&rows, "models/clud-claude-kimi-k3").is_some());
        assert!(find(&rows, "clud-claude-kimi-k3").is_some());
        assert!(find(&rows, "models/clud-claude-nope").is_none());
        assert!(find(&rows, "clud-claude-kimi").is_none());
    }

    #[test]
    fn a_nameless_row_is_never_advertised() {
        let native = provider_catalog::MODELS
            .iter()
            .find(|entry| entry.discovery_id.is_none())
            .expect("a native Claude row");
        assert!(row_json(native).is_none());
        assert!(detail_body(native).is_none());
    }

    #[test]
    fn the_list_body_wraps_rows_in_models_and_keeps_their_order() {
        let first = row("clud-claude-codex-sol");
        let second = row("clud-claude-kimi-k3");
        let body: Value = serde_json::from_slice(&list_body(&[&first, &second])).expect("json");
        let models = body["models"].as_array().expect("models array");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["name"], "models/clud-claude-codex-sol");
        assert_eq!(models[1]["name"], "models/clud-claude-kimi-k3");
    }

    #[test]
    fn an_empty_set_lists_no_models_instead_of_failing() {
        let body: Value = serde_json::from_slice(&list_body(&[])).expect("json");
        assert_eq!(body["models"].as_array().map(Vec::len), Some(0));
    }
}

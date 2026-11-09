//! `:free` OpenRouter model ids (#1833).
//!
//! OpenRouter publishes each free variant as its own catalog row
//! (`vendor/model:free`, price 0/0). Two things go wrong with them today:
//!
//! - `:free` reads as a cost promise, but nothing checks it: a stale or
//!   mistyped id could bill. [`check`] looks the id up in the offline catalog
//!   ([`crate::openrouter_catalog::catalog_cached_or_embedded`], no egress on
//!   the launch path) and the launch refuses anything it cannot prove free.
//! - Free endpoints are served by providers that may train on prompts. A
//!   workspace guardrail that blocks that leaves the id with zero routable
//!   endpoints, and Claude Code shows a bare `400` on the first turn. A
//!   request cannot opt back in (account privacy settings are the ceiling),
//!   so [`probe`] asks once at launch and turns that refusal into an
//!   actionable error before the session starts.

use std::io::Read;
use std::time::Duration;

use crate::openrouter_catalog::Catalog;

/// The suffix OpenRouter uses for a model's free variant.
pub const FREE_SUFFIX: &str = ":free";

/// Where the user changes the guardrail that blocks free endpoints.
pub const GUARDRAILS_URL: &str = "https://openrouter.ai/workspaces/default/guardrails";

const PROBE_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What the offline catalog says about a wire id.
#[derive(Debug, Clone, PartialEq)]
pub enum FreeCheck {
    /// The id does not ask for a free variant; nothing is checked.
    NotRequested,
    /// A catalog row with zero input and output price.
    Free,
    /// A catalog row that is priced (USD per million tokens).
    NotFree { input: f64, output: f64 },
    /// No catalog row matches the id exactly.
    Unknown,
}

impl FreeCheck {
    /// The `free_check` value `--dry-run` reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::Free => "free",
            Self::NotFree { .. } => "not_free",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify `wire_id` against `catalog`. Exact match only, like
/// [`Catalog::model_by_id`]: a same-family paid row never vouches for a free id.
pub fn check(wire_id: &str, catalog: &Catalog) -> FreeCheck {
    if !wire_id.ends_with(FREE_SUFFIX) {
        return FreeCheck::NotRequested;
    }
    let Some(model) = catalog.model_by_id(wire_id) else {
        return FreeCheck::Unknown;
    };
    let input = model.input_price_per_token.unwrap_or(0.0);
    let output = model.output_price_per_token.unwrap_or(0.0);
    if input == 0.0 && output == 0.0 {
        FreeCheck::Free
    } else {
        FreeCheck::NotFree {
            input: input * 1_000_000.0,
            output: output * 1_000_000.0,
        }
    }
}

/// The launch refusal for a `:free` id the catalog cannot prove free, or
/// `None` when the launch may proceed. A stale catalog only ever refuses.
pub fn refusal(wire_id: &str, check: &FreeCheck, catalog: &Catalog) -> Option<String> {
    match check {
        FreeCheck::NotRequested | FreeCheck::Free => None,
        FreeCheck::Unknown => Some(format!(
            "`{wire_id}` is not in OpenRouter's model catalog, so clud cannot confirm it is \
             free; refusing rather than risk billing. Without `{FREE_SUFFIX}`, {}.",
            paid_alternative(wire_id, catalog)
        )),
        FreeCheck::NotFree { input, output } => Some(format!(
            "`{wire_id}` is priced at ${input:.2}/${output:.2} per million input/output tokens \
             on OpenRouter, but `{FREE_SUFFIX}` asks for a free model; refusing."
        )),
    }
}

/// What dropping `:free` would cost, so no message nudges a user onto a paid
/// model without naming its price (#1833: that nudge is how money was spent).
pub fn paid_alternative(wire_id: &str, catalog: &Catalog) -> String {
    let paid = wire_id.trim_end_matches(FREE_SUFFIX);
    match catalog.model_by_id(paid) {
        Some(model) => format!(
            "the paid `{paid}` bills ${:.2}/${:.2} per million input/output tokens",
            model.input_price_per_token.unwrap_or(0.0) * 1_000_000.0,
            model.output_price_per_token.unwrap_or(0.0) * 1_000_000.0
        ),
        None => format!("`{paid}` is a paid model whose price clud does not know"),
    }
}

/// The one launch line naming what an OpenRouter launch will bill, from the
/// offline catalog; `source` is the selection's `model_source`. `None` when
/// the catalog has no row (an arbitrary pass-through id).
pub fn price_notice(wire_id: &str, catalog: &Catalog, source: &str) -> Option<String> {
    let model = catalog.model_by_id(wire_id)?;
    let input = model.input_price_per_token.unwrap_or(0.0) * 1_000_000.0;
    let output = model.output_price_per_token.unwrap_or(0.0) * 1_000_000.0;
    let price = if input == 0.0 && output == 0.0 {
        "free".to_string()
    } else {
        format!("${input:.2}/${output:.2} per million input/output tokens")
    };
    let origin = match source {
        "cli" => "named on the command line".to_string(),
        "provider_setting" => "saved default; change it with --model or clud setup".to_string(),
        other => other.replace('_', " "),
    };
    Some(format!(
        "[clud] OpenRouter model {wire_id}: {price} ({origin})"
    ))
}

/// The launch line warning that a `:free` id is rate-limited mid-session.
///
/// OpenRouter caps free ids per minute and per day. When a cap or an upstream
/// provider error lands after a long session has started, the stream ends
/// after one event and the non-streaming retry gets HTTP 200 with a JSON body
/// that is not an Anthropic Message, which Claude Code shows as `API returned
/// an empty or malformed response (HTTP 200)` (#1845). clud is not in the
/// request path on the direct route, so it can only say so before the session
/// starts. `None` for any id that is not `:free`.
pub fn free_limits_notice(wire_id: &str) -> Option<String> {
    let paid = wire_id.strip_suffix(FREE_SUFFIX)?;
    Some(format!(
        "[clud] note: {wire_id} is rate-limited by OpenRouter (per-minute and per-day caps). \
         A cap or upstream error mid-session surfaces in Claude Code as \
         \"API returned an empty or malformed response (HTTP 200)\"; \
         use the paid `{paid}` for long or /loop sessions."
    ))
}

/// How a launch probe of a free id ended.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeVerdict {
    /// The id answered, or the probe could not tell (network, rate limit,
    /// server error): the launch proceeds and the harness reports any error.
    Proceed,
    /// OpenRouter refused every endpoint for the id; carries the message.
    Blocked(String),
}

/// One reason OpenRouter gave for excluding endpoints, as it sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct ExclusionReason {
    /// Machine reason, e.g. `free-model-training-violation-by-guardrail`.
    pub reason: String,
    /// Where OpenRouter says the setting lives.
    pub configure_url: Option<String>,
}

/// An endpoint-exclusion refusal, parsed from either OpenRouter error shape:
/// chat completions (`{"error":{"message",…,"metadata"}}`) or the Anthropic
/// `/messages` surface Claude Code uses (`{"type":"error","error":{…}}`).
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    /// OpenRouter's own message, verbatim.
    pub message: String,
    /// `metadata.ineligibility_reasons`, empty when OpenRouter sent none.
    pub reasons: Vec<ExclusionReason>,
    /// True for "no endpoint serves this id" rather than a policy filter.
    pub no_endpoint: bool,
}

/// The account's prepaid credit, from `GET /api/v1/credits`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreditBalance {
    pub total_credits: f64,
    pub total_usage: f64,
}

impl CreditBalance {
    pub fn remaining(&self) -> f64 {
        self.total_credits - self.total_usage
    }
}

/// Parse a probe's error reply into a [`Refusal`] when it is an
/// endpoint-exclusion refusal. A free model's 429, a 5xx, or any other error
/// says nothing about whether the id can be served, so it yields `None` and
/// the launch proceeds (#1833).
pub fn classify(status: u16, body: &str) -> Option<Refusal> {
    let lower = body.to_ascii_lowercase();
    let policy = lower.contains("guardrail") || lower.contains("data policy");
    let no_endpoint = status == 404 && lower.contains("no endpoints");
    if !policy && !no_endpoint {
        return None;
    }
    let error = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("error").cloned());
    let message = error
        .as_ref()
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(body)
        .trim()
        .to_string();
    let reasons = error
        .as_ref()
        .and_then(|error| error.pointer("/metadata/ineligibility_reasons"))
        .and_then(serde_json::Value::as_array)
        .map(|reasons| {
            reasons
                .iter()
                .filter_map(|entry| {
                    Some(ExclusionReason {
                        reason: entry.get("reason")?.as_str()?.to_string(),
                        configure_url: entry
                            .get("configure_url")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Refusal {
        message,
        reasons,
        no_endpoint: no_endpoint && !policy,
    })
}

/// Parse `GET /api/v1/credits`.
pub fn parse_credits(body: &str) -> Option<CreditBalance> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let data = value.get("data")?;
    Some(CreditBalance {
        total_credits: data.get("total_credits")?.as_f64()?,
        total_usage: data.get("total_usage")?.as_f64()?,
    })
}

/// Up to `limit` other `:free` catalog ids from a different vendor than
/// `wire_id`, offline. A guardrail that blocks free endpoints which train on
/// prompts leaves free models from providers that do not train routable
/// (observed in #1838: nvidia/poolside/liquid blocked, cohere and apodex
/// served), and the catalog cannot say which providers train, so these are
/// suggestions, not promises.
pub fn free_alternatives(wire_id: &str, catalog: &Catalog, limit: usize) -> Vec<String> {
    let vendor = wire_id.split('/').next().unwrap_or_default();
    catalog
        .models()
        .iter()
        .filter(|model| model.id.ends_with(FREE_SUFFIX))
        .filter(|model| model.id.split('/').next() != Some(vendor))
        .filter(|model| check(&model.id, catalog) == FreeCheck::Free)
        .map(|model| model.id.clone())
        .take(limit)
        .collect()
}

/// What dropping `:free` would take: its price, or, when the account has no
/// credit left, that it cannot run at all. A key's spending limit is a cap,
/// not money, so only the account balance is consulted (#1838).
fn paid_hint(wire_id: &str, catalog: &Catalog, credit: Option<CreditBalance>) -> String {
    let paid = wire_id.trim_end_matches(FREE_SUFFIX);
    match credit {
        Some(credit) if credit.remaining() <= 0.0 => format!(
            "the paid `{paid}` needs OpenRouter credit, and the account balance is \
             ${:.2} (${:.2} bought, ${:.2} used; a key's spending limit is not credit). \
             Add credit at {CREDITS_URL}",
            credit.remaining(),
            credit.total_credits,
            credit.total_usage
        ),
        _ => paid_alternative(wire_id, catalog),
    }
}

/// Where the user adds OpenRouter credit.
pub const CREDITS_URL: &str = "https://openrouter.ai/settings/credits";

/// The launch error for a refused free id. OpenRouter's own reason and
/// settings link are relayed verbatim rather than restated, because a
/// guardrail's data-policy flags do not appear in its list view: the list can
/// say "No policies" while a flag blocks free endpoints that train (#1838).
pub fn explain(
    wire_id: &str,
    refusal: &Refusal,
    catalog: &Catalog,
    credit: Option<CreditBalance>,
) -> String {
    let mut lines = Vec::new();
    if refusal.no_endpoint {
        lines.push(format!(
            "OpenRouter has no endpoint serving `{wire_id}` right now."
        ));
    } else {
        lines.push(format!(
            "OpenRouter refused every endpoint for `{wire_id}`: {}",
            refusal.message
        ));
        for reason in &refusal.reasons {
            match &reason.configure_url {
                Some(url) => {
                    lines.push(format!("  reason: {} (configure at {url})", reason.reason))
                }
                None => lines.push(format!("  reason: {}", reason.reason)),
            }
        }
        if refusal.reasons.is_empty() {
            lines.push(format!("  check your guardrails at {GUARDRAILS_URL}"));
        }
        if refusal
            .reasons
            .iter()
            .any(|reason| reason.reason.contains("guardrail"))
        {
            lines.push(
                "Open the guardrail's edit view (not the list) and enable free endpoints that \
                 train on request data, and that publish prompts if shown, then save. The list \
                 can say \"No policies\" while one of these flags blocks the model, and the \
                 account Privacy page cannot override a stricter guardrail."
                    .to_string(),
            );
        }
    }
    let alternatives = free_alternatives(wire_id, catalog, 4);
    if !alternatives.is_empty() {
        lines.push(format!(
            "Free models from providers that do not train on prompts may still route: {}.",
            alternatives.join(", ")
        ));
    }
    lines.push(format!(
        "Without `{FREE_SUFFIX}`, {}.",
        paid_hint(wire_id, catalog, credit)
    ));
    lines.join("\n")
}

/// Send one `max_tokens: 1` request for `wire_id` with the vault `key`.
/// Runs only for ids [`check`] proved free, so it never bills. Only when the
/// id is refused does it make one more free call, to `/credits`, so the
/// explanation never suggests a paid model the account cannot pay for.
pub fn probe(wire_id: &str, key: &str, catalog: &Catalog) -> ProbeVerdict {
    let agent = ureq::AgentBuilder::new()
        .timeout(PROBE_TIMEOUT)
        .redirects(0)
        .build();
    let body = serde_json::json!({
        "model": wire_id,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let refused = match agent
        .post(PROBE_URL)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
    {
        Err(ureq::Error::Status(status, response)) => {
            let mut text = String::new();
            let _ = response.into_reader().take(8192).read_to_string(&mut text);
            classify(status, &text)
        }
        _ => None,
    };
    let Some(refusal) = refused else {
        return ProbeVerdict::Proceed;
    };
    let credit = agent
        .get(CREDITS_PROBE_URL)
        .set("Authorization", &format!("Bearer {key}"))
        .call()
        .ok()
        .and_then(|response| {
            let mut text = String::new();
            response
                .into_reader()
                .take(8192)
                .read_to_string(&mut text)
                .ok()?;
            parse_credits(&text)
        });
    ProbeVerdict::Blocked(explain(wire_id, &refusal, catalog, credit))
}

const CREDITS_PROBE_URL: &str = "https://openrouter.ai/api/v1/credits";

#[cfg(test)]
mod tests_support {
    use super::*;

    pub(super) fn catalog() -> Catalog {
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
                row("cohere/north-mini-code:free", 0.0, 0.0),
                row("nvidia/nemotron-3.5-lightning:free", 0.0, 0.0),
            ],
        });
        Catalog::parse(json.to_string().as_bytes()).expect("fixture catalog parses")
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::catalog;
    use super::*;

    const FREE: &str = "nvidia/nemotron-3-ultra-550b-a55b:free";

    #[test]
    fn only_free_suffixed_ids_are_checked() {
        let catalog = catalog();
        assert_eq!(
            check("nvidia/nemotron-3-ultra-550b-a55b", &catalog),
            FreeCheck::NotRequested
        );
        assert_eq!(check("not/in-catalog", &catalog), FreeCheck::NotRequested);
        assert_eq!(
            refusal("not/in-catalog", &FreeCheck::NotRequested, &catalog),
            None
        );
    }

    #[test]
    fn a_zero_priced_free_row_is_allowed() {
        let catalog = catalog();
        let verdict = check(FREE, &catalog);
        assert_eq!(verdict, FreeCheck::Free);
        assert_eq!(refusal(FREE, &verdict, &catalog), None);
    }

    #[test]
    fn a_priced_or_unknown_free_id_is_refused() {
        let catalog = catalog();
        let priced = check("acme/priced:free", &catalog);
        assert_eq!(
            priced,
            FreeCheck::NotFree {
                input: 1.0,
                output: 2.0
            }
        );
        let message = refusal("acme/priced:free", &priced, &catalog).expect("priced :free refused");
        assert!(message.contains("$1.00/$2.00"), "{message}");
        let unknown = check("acme/missing:free", &catalog);
        assert_eq!(unknown, FreeCheck::Unknown);
        let message =
            refusal("acme/missing:free", &unknown, &catalog).expect("unknown :free refused");
        assert!(
            message.contains("`acme/missing` is a paid model"),
            "{message}"
        );
    }

    /// Captured verbatim from OpenRouter on 2026-10-06 (#1838): chat
    /// completions, with the reason metadata the old message ignored.
    const GUARDRAIL_404: &str = r#"{"error":{"message":"0 endpoints out of 1 requested are available matching your guardrail restrictions and data policy. We removed them for the following reasons (an endpoint may have matched multiple reasons):\nFree model training violation (guardrail): 1 endpoint excluded; configurable at https://openrouter.ai/workspaces/default/guardrails","code":404,"metadata":{"input_endpoint_count":1,"ineligibility_reasons":[{"reason":"free-model-training-violation-by-guardrail","endpoint_count":1,"configure_url":"https://openrouter.ai/workspaces/default/guardrails"}],"routing_funnel":[{"step":"Initial Endpoints","endpoint_count":1}],"failed_routing_step":"Filter by Guardrails"}}}"#;

    /// The same refusal on the Anthropic `/messages` surface Claude Code uses.
    const GUARDRAIL_MESSAGES_404: &str = r#"{"type":"error","error":{"type":"not_found_error","message":"0 endpoints out of 1 requested are available matching your guardrail restrictions and data policy.","metadata":{"ineligibility_reasons":[{"reason":"free-model-training-violation-by-guardrail","endpoint_count":1,"configure_url":"https://openrouter.ai/workspaces/default/guardrails"}],"failed_routing_step":"Filter by Guardrails"}}}"#;

    const CREDITS_EMPTY: &str = r#"{"data":{"total_credits":50,"total_usage":50.201787564}}"#;

    fn blocked(status: u16, body: &str) -> Refusal {
        classify(status, body).expect("an endpoint-exclusion refusal")
    }

    #[test]
    fn the_guardrail_refusal_relays_openrouters_reason_and_url() {
        let refusal = blocked(404, GUARDRAIL_404);
        assert_eq!(
            refusal.reasons,
            [ExclusionReason {
                reason: "free-model-training-violation-by-guardrail".into(),
                configure_url: Some(GUARDRAILS_URL.into()),
            }]
        );
        let message = explain(FREE, &refusal, &catalog(), None);
        assert!(
            message.contains("free-model-training-violation-by-guardrail"),
            "{message}"
        );
        assert!(
            message.contains(&format!("configure at {GUARDRAILS_URL}")),
            "{message}"
        );
        assert!(
            message.contains("0 endpoints out of 1 requested"),
            "{message}"
        );
        assert!(message.contains("edit view"), "{message}");
        assert!(message.contains("\"No policies\""), "{message}");
        assert!(message.contains("$0.50/$2.20"), "{message}");
    }

    #[test]
    fn the_messages_surface_refusal_classifies_the_same() {
        assert_eq!(
            blocked(404, GUARDRAIL_MESSAGES_404).reasons,
            blocked(404, GUARDRAIL_404).reasons
        );
    }

    #[test]
    fn an_empty_balance_replaces_the_paid_price_with_an_add_credit_hint() {
        let credit = parse_credits(CREDITS_EMPTY).expect("credits parse");
        assert!(credit.remaining() < 0.0);
        let message = explain(FREE, &blocked(404, GUARDRAIL_404), &catalog(), Some(credit));
        assert!(message.contains(CREDITS_URL), "{message}");
        assert!(message.contains("$-0.20"), "{message}");
        assert!(message.contains("not credit"), "{message}");
        assert!(!message.contains("bills $0.50"), "{message}");
    }

    #[test]
    fn a_funded_account_keeps_the_paid_price() {
        let credit = parse_credits(r#"{"data":{"total_credits":50,"total_usage":10}}"#).unwrap();
        let message = explain(FREE, &blocked(404, GUARDRAIL_404), &catalog(), Some(credit));
        assert!(message.contains("bills $0.50/$2.20"), "{message}");
    }

    #[test]
    fn an_unknown_reason_is_relayed_verbatim_without_the_guardrail_advice() {
        let body = r#"{"error":{"message":"0 endpoints match your data policy","metadata":{"ineligibility_reasons":[{"reason":"brand-new-reason"}]}}}"#;
        let message = explain(FREE, &blocked(400, body), &catalog(), None);
        assert!(message.contains("reason: brand-new-reason"), "{message}");
        assert!(!message.contains("edit view"), "{message}");
    }

    #[test]
    fn alternatives_come_from_the_offline_catalog_and_skip_the_blocked_vendor() {
        let alternatives = free_alternatives(FREE, &catalog(), 4);
        assert_eq!(alternatives, ["cohere/north-mini-code:free"]);
        let message = explain(FREE, &blocked(404, GUARDRAIL_404), &catalog(), None);
        assert!(message.contains("cohere/north-mini-code:free"), "{message}");
    }

    #[test]
    fn no_key_shaped_text_reaches_the_message() {
        let message = explain(FREE, &blocked(404, GUARDRAIL_404), &catalog(), None);
        assert!(!message.contains("sk-or-"), "{message}");
    }

    #[test]
    fn rate_limits_and_server_errors_never_block() {
        for (status, body) in [
            (
                429,
                r#"{"error":{"message":"Rate limit exceeded: free-models-per-min"}}"#,
            ),
            (502, "bad gateway"),
            (400, r#"{"error":{"message":"max_tokens too small"}}"#),
        ] {
            assert_eq!(classify(status, body), None, "{status}");
        }
        let no_endpoint = r#"{"error":{"message":"No endpoints found for x:free."}}"#;
        let refusal = blocked(404, no_endpoint);
        assert!(refusal.no_endpoint);
        let message = explain(FREE, &refusal, &catalog(), None);
        assert!(message.contains("no endpoint serving"), "{message}");
    }
}

#[cfg(test)]
mod notice_tests {
    use super::tests_support::catalog;
    use super::*;

    #[test]
    fn every_catalogued_launch_names_its_price_and_origin() {
        let catalog = catalog();
        let paid = price_notice(
            "nvidia/nemotron-3-ultra-550b-a55b",
            &catalog,
            "provider_setting",
        )
        .expect("catalogued");
        assert!(paid.contains("$0.50/$2.20"), "{paid}");
        // #2753: setup, the menu, a Hermes import or --model may have saved
        // it, so the text names no single cause.
        assert!(
            paid.contains("(saved default; change it with --model or clud setup)"),
            "{paid}"
        );
        assert!(!paid.contains("earlier --model"), "{paid}");
        let free = price_notice("nvidia/nemotron-3-ultra-550b-a55b:free", &catalog, "cli")
            .expect("catalogued");
        assert!(
            free.contains(": free (named on the command line)"),
            "{free}"
        );
        assert_eq!(price_notice("not/in-catalog", &catalog, "cli"), None);
    }

    #[test]
    fn a_free_launch_warns_that_mid_session_limits_look_like_a_malformed_response() {
        let notice = free_limits_notice("nvidia/nemotron-3-ultra-550b-a55b:free").expect("free id");
        assert!(
            notice.contains("empty or malformed response (HTTP 200)"),
            "{notice}"
        );
        assert!(
            notice.contains("`nvidia/nemotron-3-ultra-550b-a55b`"),
            "{notice}"
        );
        assert!(notice.contains("/loop"), "{notice}");
        assert_eq!(
            free_limits_notice("nvidia/nemotron-3-ultra-550b-a55b"),
            None
        );
    }
}

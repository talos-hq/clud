# Unified gateway

`clud --unified` starts one authenticated, launch-scoped loopback gateway for a
Claude Code child. It is foreground-owned and is shut down with that child; it
is neither a sidecar nor a daemon.

The configuration this gateway is built from — which routes exist, which
slots bind where, and the upstream routing object it injects — is the launch's
`ResolvedRoute`: see [route-plan.md](route-plan.md).

Unified is an explicit routing mode, not a provider. It always uses the Claude
harness, rejects an explicit Codex harness before bootstrap, and requires
Claude Code 2.1.223 or newer. Older clients are rejected with their installed
version and the `claude update` remedy before the gateway or a paid request is
started.

## Discovery and authentication

The child receives a loopback `ANTHROPIC_BASE_URL`,
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`, and an ephemeral
`X-Clud-Gateway-Token` merged into `ANTHROPIC_CUSTOM_HEADERS`. All loopback
routes require that header. The gateway token is never serialized into a
`LaunchPlan`, daemon payload, dry-run output, logs, or debug output.

Claude credentials remain owned by Claude Code: clud preserves its incoming
`Authorization`/`x-api-key` headers only on the native Claude route. The
Codex route constructs its own OpenAI request through the existing translator;
each Anthropic-compatible route (DeepSeek, Kimi, OpenRouter) receives only its
own key from clud's native credential vault. Missing
optional credentials omit only their discovery rows and produce one sanitized,
actionable startup notice; native Claude remains usable.

## Routing

`GET /v1/models` returns catalog rows from `provider_catalog::MODELS` for
available Codex, DeepSeek, Kimi, and OpenRouter routes. Synthetic IDs are in the reserved
`clud-claude-*` namespace. A selected synthetic ID is resolved before legacy
Codex compatibility parsing: Codex IDs are rewritten to their reviewed wire
model and translated to Responses; DeepSeek, Kimi, and OpenRouter IDs are
rewritten and proxied to that provider's Anthropic-compatible endpoint. A persisted or continued session can also
name a known provider by wire ID or CLI alias (`gpt-5.6-terra`,
`deepseek-v4-pro[1m]`, `kimi-k3[1m]`); those resolve through the shared catalog to their own
provider instead of leaking to Anthropic. Unknown reserved IDs fail locally
rather than falling through to a paid provider. Ordinary Claude IDs are
proxied unchanged to Anthropic.

The Anthropic-compatible routes are one list, not per-provider fields:
`UnifiedGatewayConfig` holds one `AnthropicCompatRoute` per
`provider_registry` descriptor whose key the launch holds, and a single
`provider_available` predicate decides discovery, the refusal ID list, and
dispatch. A new Anthropic-compatible provider therefore needs a descriptor
row and a catalog row, not new gateway fields (#937 Phase 4).

Each Claude session/subagent identity also owns an active route epoch. Crossing
a provider boundary clears Codex's provider-private canonical Responses items.
If that conversation later returns to Codex, the translator reseeds from the
complete Anthropic-visible transcript in the request. Opaque reasoning,
signatures, cache identifiers, and tool identifiers are never reused across
providers. Switching among Codex models can retain the current Codex epoch.
`/clear`, eviction, and gateway shutdown remove both transcript and route state.

`POST /v1/messages/count_tokens` is proxied for ordinary native Claude model
IDs. Synthetic Codex, DeepSeek, Kimi, and OpenRouter routes return an explicit
local 404 because
their upstream token-count contracts are not Anthropic-compatible; Claude Code
falls back to its documented local estimation. Streaming message responses
remain progressive; the proxy never buffers a complete upstream stream before
returning it.

## Terminal usage accounting

On bridged routes, every successful Messages response contributes exact
terminal input, cached input, and output counts to the conversation's ledger
when its provider supplies them. Native Claude, DeepSeek, and OpenRouter usage
shapes are adapted independently from copied SSE frames; ordinary non-streaming
JSON responses use a 64 KiB bounded observation buffer. Forwarded response
bytes are never rewritten or retained by the bridge ledger. On direct
Claude-harness routes, the status-line callback instead incrementally reads
provider-reported counters from the harness transcript and its subagents,
deduplicated globally by response id. Bridge totals take precedence. Both
surfaces show a single cumulative triple and the last completed model for the
current conversation; `/clear` resets both ledgers
([DD-186](../DESIGN_DECISIONS.md#dd-186-status-line-usage-is-scoped-to-one-conversation)). Cache
health remains in expanded details. Missing or malformed counters do not
become per-call estimates. This display accounting is separate from the
Codex-only cache-health fuse.

## Session-wide effort

Claude Code resolves `/effort`, `--effort`, settings, environment, model-picker
controls, and request-specific skill/subagent overrides before sending a
Messages request. The gateway sees the final `output_config.effort` string but
no source marker. Unified mode therefore has one honest contract: the harness's
effective effort is session-wide, survives `/model` switches, and is consumed
independently on every request.

Unified child setup neither injects a direct-mode effort pin nor deletes an
ambient user value, and direct mode now follows the same rule
([DD-059](../DESIGN_DECISIONS.md#dd-059-direct-provider-launches-carry-effort-on-the-session-flag-and-the-reviewed-default-is-low)): the
catalog default (currently `low` for every Anthropic-compat row) rides the
harness's `--effort` session flag and `/effort` stays live. `/effort auto` and
a session with no explicit setting are harness-resolved; the gateway cannot
recover `auto` or secretly restore Sol's `low`, Terra/Luna's `medium`, or a
provider's catalog default after the final request value exists.

| Route | Gateway behavior |
|---|---|
| Native Claude | Preserve the request body byte-for-byte, including all of `thinking` and `output_config`, and forward the required caller-owned Anthropic headers. |
| Codex Sol/Terra/Luna | Resolve the synthetic ID first, then use `codex_translate::effort_for`: `<model>@effort` > `output_config.effort` > stated thinking budget > catalog default. Unsupported stated values fail locally with zero upstream calls. |
| DeepSeek Pro/Flash | Rewrite only the model ID and preserve `thinking` plus the complete `output_config`; do not apply Codex validation. DeepSeek maps `low`/`medium` to effective `high`, `high` to `high`, and `xhigh`/`max` to `max`. |
| Kimi K3 | Same as DeepSeek: rewrite only the model ID to `kimi-k3[1m]` and pass `output_config` through; Moonshot owns how it calibrates the value. The catalog lists `low`, `high`, and `max`. |

The same level name is calibrated differently by each model. Diagnostics may
name the public provider/effort, but never credentials, prompts, reasoning
content, response bodies, or provider-private state.

## Payload rules

`CLUD_PAYLOAD_RULES=<path>` names a JSON file of operator rules that rewrite a
request body before a route is chosen ([`payload_rules.rs`](../../crates/clud-bin/src/payload_rules.rs)).
This is the port of CLIProxyAPI's `requests.payload` seam; it is the one place a
launch may change what the harness asked for, so it ships off unless the
variable is set and the file parses.

```json
{
  "default":  [{"models": [{"name": "claude-*"}], "params": {"temperature": 0.3}}],
  "override": [{"models": [{"name": "claude-*", "protocol": "claude"}],
                "params": {"max_tokens": 32}}],
  "filter":   [{"params": ["metadata.user_id"]}]
}
```

* Kinds run in `default` → `override` → `filter` order. `default` fills a path
  only when the caller left it absent or `null`; `override` always sets it;
  `filter` removes it.
* `models[].name` is a case-insensitive `*` glob and an empty `models` array
  selects every model. `models[].protocol` narrows the rule to the protocol the
  gateway is serving (`claude` for `POST /v1/messages`); absent matches any.
* `params` keys are dotted JSON paths; a numeric segment indexes an array
  (`messages.0.role`). A missing object on the way is created; an array is never
  grown, because inventing elements would change the conversation.
* A rule that cannot be applied is reported on stderr and skipped — the client's
  request still proceeds with every change that landed. The file is re-read when
  its mtime or size changes, so an edit applies to the next turn without a
  restart.
* Unknown keys are a parse error, so a typo (`overide`) is refused instead of
  silently doing nothing — a rule file that cannot be understood means the
  gateway forwards the harness bytes untouched.

Not yet ported: gjson query syntax (`tools.#(name=="x")`), the
`headers`/`match`/`not-match`/`exist` selectors, the `*-raw` variants, and the
OpenAI-shaped ingress body (rules apply to the Anthropic-shaped body this
gateway receives).

## Hung upstreams

The proxy hop's budget is *idle*, not total: it reads through an agent with
`timeout_read` set to `stream_idle_timeout`, so a model that thinks for minutes
while emitting deltas is never cut off, and a socket that stops producing bytes
is noticed after that many seconds of silence. Idleness is measured on received
bytes, never on turn duration — a long think and a hang are identical by
wall-clock alone. A failure before the first frame is a real status (a timeout
answers 504 `timeout_error`, which the harness retries, rather than 502, which
it does not); after the first frame the status is spent (DD-029), so the
failure is reported in-band as a sanitized SSE `error` event instead of a
clean-looking end of stream. See DD-028's amendment and DD-079.

## Acceptance matrix

| Contract | Guardrail |
|---|---|
| Native body/header fidelity and credential isolation | `unified_native_claude_preserves_effort_payload_and_required_headers_byte_for_byte` |
| Every Codex discovery model and accepted effort; suffix/budget/default precedence; local rejection | `unified_codex_models_and_efforts_reach_the_exact_responses_fields` |
| Both DeepSeek models, documented effective mapping, future provider value passthrough | `unified_deepseek_preserves_effort_for_both_models_without_codex_validation` |
| Claude -> Codex -> DeepSeek -> Claude switching, Codex reseed, child override isolation | `unified_provider_switch_reseeds_codex_and_keeps_main_and_agent_efforts_independent` |
| Every discovery ID routes to exactly its upstream with per-provider credential isolation | `unified_routes_all_five_ids_with_provider_credential_isolation` |
| Persisted wire IDs (`gpt-*`, `deepseek-*`) route to their own provider and never reach Anthropic | `unified_wire_ids_route_to_their_own_provider_not_anthropic` |
| Native token counting proxied with Claude auth; synthetic and wire-ID routes 404; unknown reserved IDs fail locally | `unified_native_count_tokens_is_proxied_with_claude_auth` |
| Native Claude, DeepSeek, and OpenRouter terminal usage is aggregated without changing streamed bytes | `unified_anthropic_routes_publish_exact_launch_wide_usage_to_statusline` |
| A hung upstream answers 504 `timeout_error`, not a dead-route 502 | `a_hung_upstream_answers_504_not_502` |
| A mid-stream stall is reported in-band, never closed like a clean end of stream | `a_stalled_upstream_stream_reports_an_in_band_error` |
| The upstream budget is byte-idle: a long turn that keeps streaming is not cut off | `a_long_but_continuously_streaming_turn_is_not_cut_off` |
| Ambient effort preservation and no global default injection | `unified_overlay_preserves_claude_credentials_and_enables_discovery`, `unified_overlay_does_not_inject_a_global_effort_default` |
| Kimi is advertised only with its key, routes `clud-claude-kimi-k3`/`kimi-k3`/`kimi-k3[1m]` with only its key, and token counts 404 locally | `unified_kimi_route_is_key_gated_and_credential_isolated` |
| Claude -> Codex -> DeepSeek -> Kimi -> Claude -> Codex keeps Codex private items out of other epochs | `unified_route_epoch_cycle_includes_kimi` |
| Routes come from the registry; a missing key drops only that route | `unified_routes_come_from_the_registry_and_drop_on_a_missing_key` |
| Through the binary: a vault-held Kimi key advertises and serves the Kimi row, and logout removes it | `tests/integration/test_901_acceptance_matrix.py::test_kimi_joins_the_unified_gateway_from_its_own_vault_record` |
| Missing optional credentials emit one sanitized, actionable notice | `unified_missing_provider_notices_are_sanitized_and_actionable` |
| Installed-client `--effort low|high|xhigh|max` request shape | `tests/test_real_claude_unified_effort.py` (opt in with `CLUD_REAL_CLAUDE_TESTS=1`) |

Gateway discovery requires Claude Code 2.1.223 or newer. For a release smoke,
run the opt-in fixture above, then launch `clud --unified` interactively and
verify `/model` shows the honestly labeled configured routes. Select a
synthetic row, open `/effort`, and verify the effort control (including the
`/model` slider where that client version exposes it) remains available after a
model switch. Discovery metadata does not guarantee slider presentation on
every client build; `/effort` and `--effort` remain the protocol-level controls.

The advertised DeepSeek Pro row maps to the reviewed
`deepseek-v4-pro[1m]` wire ID. Unified mode does not install direct DeepSeek's
global 1m overlay — and neither mode installs an effort pin (DD-059) — and does
not suppress Claude plan mode for Codex; Claude Code remains the per-turn
policy owner.

## Credential commands

Credential management uses `clud auth login <provider>`, `clud auth status
[provider]`, and `clud auth logout <provider>`. `codex-auth` and
`deepseek-auth` remain hidden aliases for this major version and print their
exact replacement. Claude status is reported as externally managed; clud never
copies, refreshes, or deletes Claude credentials.

## Validation boundary

Focused protocol tests select every advertised synthetic ID against separate
Claude, Codex, DeepSeek, and Kimi canary upstreams, assert the exact wire model and
credential boundary, exercise native token counting, reject unknown reserved
IDs before any upstream request, and switch Claude -> Codex -> DeepSeek ->
Claude in one conversation before verifying Codex is freshly seeded.

Surviving provider exhaustion mid-session — route health, a configured
failover ladder, pre-commit request replay, and the `/_clud/route/*` control
surface — is described in [provider-failover.md](provider-failover.md) (#968).

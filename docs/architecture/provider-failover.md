# Provider failover (#968)

**Status: implemented (#968).** Routing OpenRouter through the gateway
(section 1), the failure taxonomy and route ledger (`route_health.rs`,
section 2), the cost-labeled ladder (`failover.rs`, section 3), pre-commit
replay (section 4), and section 5's cooldown recovery, stickiness, sanitized
notices, and `/_clud/route/*` control surface. Reachable with
`--failover <routes>` and `--failover-allow-metered`; `--dry-run` reports the
resolved ladder and rejects an unroutable rung.

The upstreams a ladder may descend onto, and the boundary it must respect,
come from the launch's `ResolvedRoute`: see [route-plan.md](route-plan.md).

One thing is deliberately **not** built: a standalone `clud route
status` command. The gateway is launch-scoped and its port and token are never
serialized -- that is the property that keeps a launch's credentials off disk --
so a separate process has nothing to connect to. Health is exposed instead on
the gateway itself at `GET /_clud/route/status`, reachable by anything already
holding the gateway token, and `POST /_clud/route/clear` forgets one route's
health after a top-up or a key swap (a drained route has no clock, so nothing
else can bring it back). Building the CLI would require publishing a discovery
file that the current design intentionally avoids. This document builds on the
unified gateway ([unified-gateway.md](unified-gateway.md)) and the provider
catalog ([provider-selection.md](provider-selection.md)).

Current limits, stated plainly. Failover triggers on the
Anthropic-compatible proxy routes -- Claude, DeepSeek, and OpenRouter. Codex is
a valid *destination* rung but never a probe source, because its pipeline
commits through a different path, so a descent that lands on Codex stops there.
A quota failure delivered in-band inside a `200` stream is likewise not
recorded against the route on the proxy path: that response is already
committed, so post-commit degradation applies and the next request carries the
recovery.

## The problem

The upstream is welded on at process launch. A direct provider launch exports
`ANTHROPIC_BASE_URL` into the child and nothing reachable from inside the
session can move it afterward, so an account that runs dry ends the
conversation rather than the turn.

The reported failure is the whole diagnosis. A long-running session on
OpenRouter's free daily tier exhausted its quota mid-`/loop`; four scheduled
wakeups then died on `429 free-models-per-day`, and a topped-up retry died on
`402 insufficient credits`. Three `/model` switches — including one naming a
Claude tier directly — failed identically, because **the model picker moves the
model ID, not the upstream.** The only exit was killing the session and paying a
full uncached context re-read on `--resume`.

The context itself was never at risk: Claude Code appends every turn to its
on-disk JSONL transcript and sends the complete Anthropic-visible history on
every request. What was lost was continuity, not state. That distinction is
what makes this fixable at the gateway.

## Why the current design cannot recover

`--unified` already runs a launch-scoped loopback gateway that proxies
`POST /v1/messages` and selects an upstream per request from
`provider_catalog::MODELS`. Three things are missing.

| Gap | Evidence |
|---|---|
| OpenRouter is not routed | #939 shipped it as a **direct** provider: its catalog row carries `discovery_id: None`, `codex_bridge.rs` returns `ModelProvider::OpenRouter => false` in both `serve_unified_catalog` and `unified_catalog_ids`, and `non_claude_model_by_any_id` excludes it. Its traffic never enters the gateway, so the gateway cannot fail it over. |
| Route health is not remembered | `codex_upstream` classifies retryable-versus-terminal per request (the 408/429 transient rule; the `insufficient_quota` / `usage_limit_reached` terminal markers checked before it) and then discards the judgment. Nothing records that a route is drained until 00:00 UTC. |
| There is no second rung | A route-terminal failure has exactly one destination, the client. No configured next-best upstream exists to replay onto. |

## 1. Give OpenRouter a unified route

This is a prerequisite, not new invention: it is the same direct-to-routed
promotion #937's phase 4 later gave Kimi, which now routes (and can be a
failover rung) through the gateway's registry-driven route list. OpenRouter needs a `discovery_id` in the reserved
`clud-claude-*` namespace, both `=> false` catalog filters flipped, and
inclusion in the wire-ID resolver so a persisted `~anthropic/*` ID routes to
OpenRouter instead of leaking to Anthropic.

The payoff arrives before any automatic failover. Once both accounts are routed
catalog rows, the model picker *is* the route picker: selecting a Claude row
after an OpenRouter row changes the upstream mid-session, with no restart. The
manual escape the reported session was reaching for starts working on its own.

Nothing here is OpenRouter-specific. It is the first instance of a general
shape — an Anthropic-compatible endpoint that is not Anthropic — and Kimi
follows the same path.

## 2. Classify the failure, then remember it

Failover is safe only when the failure is the *route's* fault. A malformed
request fails identically everywhere, so replaying it spends a second account
to reproduce the same error. The classifier is therefore load-bearing, and it
feeds a per-route health record rather than a single response.

| Upstream signal | Class | Route action |
|---|---|---|
| transport error, `408`, `5xx` with a transient body | transient | Existing backoff on the same route. Never fails over. |
| `429` with `Retry-After` and no quota marker | throttled | Honor the delay in place. Escalate to exhausted after N consecutive. |
| `429` carrying a period marker (`free-models-per-day`, `insufficient_quota`, `usage_limit_reached`) | exhausted | Fail over. Cooldown until the stated reset, else a default window. |
| `402` insufficient credits | drained | Fail over. No auto-recovery; clears on a credential or config change. |
| `401`, `403` | unauthenticated | Fail over, plus one notice naming `clud auth login <provider>`. |
| `400`, `413`, `422` | request-fatal | **Never** fail over. Surface unchanged; the next rung fails the same way. |
| `400` whose body names a *policy* exclusion (a gateway guardrail — ZDR, data policy — excluded every endpoint behind this route) | policy-rejected | Fail over. The bytes are well-formed and only *this route's configuration* refuses them, which is the one case that makes the "malformed requests fail everywhere" rule not apply. |
| `200` JSON without a Messages envelope, or an SSE response with no event | provider-fault | Fail over before committing the response. Cool the route briefly; if no rung is available, answer `502` with the route name. |

The proxy checks successful JSON bodies before forwarding them. It requires a
Messages `message` object with an assistant role, ID, model, usage, and content array. This
check buffers at most 16 MiB; a larger JSON body is a provider fault. For SSE,
it waits for the first complete event with a data line before forwarding bytes, so a zero-event stream
has the same classification as a malformed non-streaming retry. The existing
`text/plain` handling remains untouched.

The `policy-rejected` row is a carve-out to the request-fatal rule, so it is worth stating why it
is safe. The `request-fatal` reasoning — *a malformed request fails identically
everywhere* — depends on the request being the thing that is wrong. A guardrail
rejection inverts it: the route's configuration is the thing that is wrong, so
another route is not merely allowed to try but is likely to succeed. Replaying
the identical bytes is exactly what the ladder already does for the rows above
it. `400` is therefore probed pre-commit alongside `401`/`402`/`403`/`429`, so
its body reaches the classifier; a non-declining 400 is re-emitted verbatim and
pays only a bounded buffer.

It is benched with a short cooldown rather than marked down, because the
remedy is a setting the operator can change at any moment. Editing the guardrail
brings the route back on its own, with no `clud route clear` and — the point of
the change — no relaunch.


One deliberate non-behavior. The observed `402` reads "requested up to 32000
tokens, but can only afford 1600." Shrinking `max_tokens` to fit is the
obvious-looking fix and the wrong one: it converts a billing failure into a
silently truncated answer. Drained is drained.

## 3. The ladder is configured, never guessed

Each launch resolves an ordered list of routes. The default ladder holds
exactly one rung — the selected route — so nothing changes for anyone who has
not opted in. Additional rungs come from `--failover` or repo/user settings,
and every rung declares who pays, because the difference between a subscription
and a metered key is the difference between a free recovery and a surprise
invoice.

| Rung | Cost owner | Role |
|---|---|---|
| `openrouter/claude-sonnet` | metered, free tier | Primary. Cheapest per token, first to run dry. |
| `claude/opus` | subscription | Same model family, same Anthropic-visible transcript, no translation layer. The cheapest possible switch. |
| `codex-terra` | metered key | Cross-provider. Costs a Responses reseed and a full cache miss. |

Order same-family rungs first. Descent past a metered rung requires consent
recorded once — an interactive confirm, or `failover.allow_metered = true` in
settings. Automatic spending nobody authorized is a worse failure than the one
being fixed.

## 4. Replay before commit, degrade after

The seam this design hangs on already exists, and `serve_messages` in
`codex_bridge.rs` documents it: *"The status is chosen only while nothing has
been written."* Everything before the first frame is still negotiable.

**Pre-commit — transparent replay.** A route-terminal status arrives before any
byte has reached the client. Mark the route, take the next rung, call
`history.enter_route()`, and re-issue the byte-identical request body upstream.
The client sees one ordinary `200` stream and never learns a provider died.
Context survives because it was never at risk: the gateway is forwarding the
transcript the client sent, not reconstructing one.

**Post-commit — end the turn, not the session.** Once a frame is out the status
is committed and no honest retry exists. Terminate the stream with the
sanitized in-band error already emitted today, mark the route dead, and route
the *next* request to the next rung. Worst case costs one turn; it never costs
the conversation.

Automatic descent and recovery retain the caller-selected route epoch. Codex's
canonical Responses items remain private to Codex while a proxy serves the
conversation; a later Codex continuation keeps those opaque items and appends
intervening visible proxy turns from the client transcript. Explicit user
provider switches still clear the epoch and reseed on return, as described in
[the unified gateway contract](unified-gateway.md).

## 5. Recover, and say so out loud

**Drift back down.** Route health carries a reset clock — `Retry-After` and
rate-limit reset headers when the provider sends them, a default window when it
does not. On expiry the rung rejoins the ladder at its original priority, so a
session that fell back at noon is on the cheap route again after the daily
reset, without a restart.

**Stay sticky in between.** Once failed over, stay on the fallback for the rest
of the turn and for subsequent turns until the primary genuinely heals.
Flapping is expensive: every switch invalidates the per-provider prompt cache
and, across a provider family, forces a reseed.

**Never swap silently.** Failover reuses the sanitized notice channel
(`foreground_runtime::unified_startup_notices`) for exactly one line per
transition, carrying no credentials, prompts, reasoning content, or response
bodies:

```
route  openrouter exhausted (429 free-models-per-day, resets 00:00Z)
       -> continuing on claude/opus (subscription)  · cache cold for one request
```

The state itself is exposed on the gateway, beside the existing
`/_clud/context/compact` and `/_clud/context/clear`: `GET /_clud/route/status`
returns the ladder with each rung's cost owner, whether it is withheld for
consent, and every known route's health and reset clock; `POST
/_clud/route/clear` forgets one route so a topped-up balance or replaced key
takes effect without a restart. Both require the gateway token, and neither
reports a credential, base URL, prompt, or response body.

## What it costs

- **One cold request per switch.** Prompt caches are per-provider, so the first
  request on a new rung re-reads the whole context at uncached rates. This is
  the argument for stickiness and for same-family ordering — and it is still
  far cheaper than the restart it replaces.
- **A reseed on cross-provider descent.** Existing, understood, already tested.
- **A real spend boundary.** The consent gate is the feature that keeps
  automatic recovery from becoming an automatic invoice.
- **Unified mode only.** A direct launch has no gateway to fail over inside.

## Acceptance matrix

| Contract | Guardrail |
|---|---|
| The OpenRouter row is advertised and routes to its own upstream with isolated credentials; `~anthropic/*` never leaks to Anthropic | `unified_routes_openrouter_with_provider_credential_isolation` |
| Selecting a Claude row after an OpenRouter row moves the upstream mid-session | `unified_model_switch_moves_the_openrouter_upstream` |
| Each taxonomy row maps to exactly its class from real captured bodies | `route_health_classifies_429_402_401_and_400_distinctly` |
| A pre-commit route-terminal status replays byte-identically onto rung 2; the client sees one `200` | `failover_replays_the_same_body_before_the_first_frame` |
| A post-commit failure ends the turn and moves only the next request | `failover_after_commit_degrades_the_turn_not_the_session` |
| A `400` never descends the ladder and issues zero downstream requests | `request_fatal_failures_never_fail_over` |
| A metered rung is skipped without recorded consent | `metered_rungs_require_recorded_consent` |
| Cooldown expiry restores original priority; no rung changes twice in one turn | `exhausted_routes_rejoin_the_ladder_after_reset` |
| Notices name provider and reason only | `failover_notices_are_sanitized_and_actionable` |
| Status reports cost owner, consent, and a clock-less drain; clear restores it | `route_status_reports_health_and_clear_restores_a_drained_route` |
| The control surface is gateway-token gated | `route_status_requires_the_gateway_token` |

## Out of scope

- **Mid-stream failover.** Once frames are committed the status is spent.
  Post-commit degradation is the honest answer, not a hidden re-stream.
- **Transcript rewriting.** The gateway forwards what the client sends; it does
  not summarize, trim, or reconstruct history to fit a cheaper rung.
- **Shrinking requests to fit a balance.** See the `402` note in section 2.
- **Rescuing a session that never ran through the gateway.** That stays a
  `--resume` restart, which is exactly why section 1 comes first.

# Codex via Claude bridge

`clud --codex --harness claude` is a foreground-only compatibility bridge: it
binds an authenticated loopback Messages endpoint for the launched Claude
harness, translates to OpenAI Responses, and forwards to the selected Codex
credential route. Native Claude launches and every non-bridge route remain
unchanged; stopping the foreground runtime stops the listener and is the
rollback boundary.

## Admission and retries

When the direct bridge has no clud-owned subscription record or API key, an
interactive foreground `clud --codex --harness claude` launch may offer to copy
a usable Codex CLI login from `~/.codex/auth.json`. The user must choose the
import; `Not now` preserves the ordinary missing-credential error and `No,
don't ask again` persists `codex.import_cli_login: "never"` in clud settings.
`"always"` is an explicit opt-in that skips the prompt. Detached, prompted,
daemon-managed, and other non-interactive launches never prompt. The copied
record lives only at `~/.clud/codex-auth.json`, so logout never edits Codex
CLI state. Since token refresh can rotate the clud copy without updating the
Codex CLI file, users should retain their Codex login or re-authenticate it if
the CLI later asks.

The bridge refreshes clud's selected subscription credential on use, within
60 seconds of expiry, including between turns of one long-lived Claude session.
Safe pre-send failures and rate limits get a bounded retry; an ambiguous
read timeout, 408, or 5xx may have already rotated the grant, so it returns a
temporary error without replaying the same refresh token. A rejected refresh
grant does not retry. If the model API rejects an access token before visible
output, the bridge re-reads the record under its lock (another process may have rotated
it), otherwise forces one refresh and replays the request once. A second 401
stops. After visible output, it never replays: an in-band auth failure is
sanitized and the next turn forces a credential recheck. A healthy stream is
not interrupted merely because its token expires while it runs.

For an interactive foreground launch with a rejected clud refresh token, a
usable native Codex login may be offered as explicit repair. Matching account
identity is described as repair; a different identity is an account switch,
and ambiguous identity is not silently substituted. Even an `"always"` import
preference does not auto-replace a rejected record. Native Codex auth is never
modified. Clud owns one selected subscription record, not a multi-account
pool, and never falls back to a different account or API key automatically.

The bridge admits two request workers at a time (`DEFAULT_MAX_CONCURRENCY`),
the smallest bound under which two connections can be in flight at once: one
slot for the foreground turn and one for anything else, such as a background
side-model call or a subagent. A single worker meant every later request
waited for the foreground turn's connection to release the sole worker, so a
subagent put nothing on the wire until the turn finished and was
indistinguishable from a hang (#989). Two is not sufficient in every case: a
subagent can still queue behind a background side-model call, and if that
happens the question is which request it queued behind, where the answer may
be a separate admission lane rather than a larger pool. The bound only makes
a second in-flight connection possible; it is not a throughput change and
does not make subagents or parallel work faster.

The per-bridge advertised worker count stays flat and small however many
bridges a process stands up, preserving #778's host-footprint cap: forensics
captured 15 bridges constructed inside one millisecond in a single pid, each
advertising a 16-worker ceiling, for 15 x 16 = 240 advertised workers. At two
workers per bridge that arithmetic is 15 x 2 = 30 advertised workers instead,
so the same host stays legible to an operator reasoning about it while
already saturated.

When both workers are occupied, later TCP connections remain in the
operating system listener backlog until a slot opens or the foreground
bridge shuts down. clud does not accept those sockets early, buffer their
request bodies, create waiter threads, or return a local `503 bridge busy`
for ordinary contention. The original socket is admitted once into the
normal pipeline, so local contention cannot duplicate a model request or
canonical-history commit. The upstream client may still make its existing
classified retries after that admission.

An occupied worker retains the existing five-minute first-frame and
stream-idle protections; a healthy stream may run longer and its backlog age is
not itself a failure. Shutdown closes active connections and the listener, so
both an active worker and queued clients are released promptly. Bridge forensic
logs record secret-free `admission_queued` and `admission_acquired` events with
aggregate upper-bound `wait_ms`; these are local scheduling observability and
are distinct from `upstream_attempt` retry records. Claude Code's own retry loop
remains a defence for transport or upstream failures that reach the harness.
The bridge does not set Claude retry environment variables to compensate for
local admission contention.

## Model discovery and context

Claude Code 2.1.223 or newer discovers a Codex-only catalog from the bridge's
authenticated `GET /v1/models`. The three rows use reserved harness-facing
IDs (`clud-claude-codex-sol`, `clud-claude-codex-terra`, and
`clud-claude-codex-luna`); the bridge rewrites the selected row to its real
current account-checked `gpt-*` wire ID before calling OpenAI. Unknown reserved
IDs fail locally. The Sol and Luna IDs come from the separate model Pages
manifest; Terra remains a reviewed catalog row. See [model-pages.md](model-pages.md).
Provider wire IDs and the legacy `<model>@<effort>` spelling remain accepted
for continued sessions, but clud no longer emits a compound wire ID to Claude
Code.

Forward compatibility is narrower than "any explicit ID" (#1005, #1022). An ID
clud has no catalog row for is served only when it is the one pinned at launch
(`--model`), and is otherwise refused with a 400. The two cases are told apart
by provenance, not by string shape: Claude Code merges this gateway's rows with
its own built-in catalog, so IDs the *harness* invented arrive looking exactly
like IDs the *user* typed, and forwarding those would return an upstream error
about a model the user never knowingly chose (#997). Matching on the launch
selection honors the ID the user actually asked for while leaving that refusal
intact. The known limit: `/model <newer-id>` mid-session is not the launch
selection, so it still refuses.

Discovery **adds** these three rows to Claude Code's `/model` picker; it does
not replace the picker's built-in Anthropic rows, and no gateway response can.
Selecting a built-in Anthropic row therefore stays possible, and it does not
fail loudly: `resolve_selection` (`codex_translate.rs:748`) maps any `claude*`
ID onto this route's configured default — the launch-time `--model` selection
when one was given, otherwise the catalog default — so the turn silently runs
on a Codex model. clud cannot constrain the picker from the gateway side — see
[provider-selection.md](provider-selection.md#gateway-discovery-adds-picker-rows-it-does-not-constrain-them)
and [DD-054](../DESIGN_DECISIONS.md#dd-054-the-model-picker-belongs-to-the-harness-and-discovery-only-adds-rows).

For this direct route only, the child overlay also owns Claude Code's workflow
role aliases: `opus` resolves to `clud-claude-codex-sol` and `sonnet` resolves
to `clud-claude-codex-terra`. This keeps upstream workflows and subagents on
their intended Codex tiers even though their requests name Claude aliases. The
overlay also gives those aliases honest Codex display names and removes any
ambient alias configuration before applying its mapping. Native Claude and the
unified gateway do not receive this override; Haiku remains harness-owned.
Every child also receives `CLUD_ROUTE_CONTEXT`, a clud-owned JSON document with
the provider, effective harness, routing mode, and cost-aware delegation policy.
Bundled `/do` skills use it rather than guessing from Claude Code's visible
model labels or probing the host environment.

Ordinary effort travels through Claude Code's session effort field and reaches
the translator as `output_config.effort`. The provider-native `none` value,
which Claude Code's CLI does not accept, remains a suffix on the synthetic ID.
The child overlay enables gateway discovery, removes the retired scalar custom
picker row, and derives `CLAUDE_CODE_MAX_CONTEXT_TOKENS=1050000` from the
common context metadata on every advertised Codex catalog row so Claude Code
does not apply its unknown-model 200K compaction fallback. A future Codex row
with missing or different context metadata fails the catalog invariant rather
than silently inheriting this value. Setting
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` is incompatible with this route
and fails before child launch.

The route intentionally has no `CLAUDE_CODE_AUTO_COMPACT_WINDOW` today. The
1.05M value is the advertised provider capability, whereas the approximately
258K window observed in a native Codex client is that client's operating
policy, not evidence that the bridge must impose the same cap; successful
bridge requests have exceeded it. An arbitrary lower cap would hide cache
identity regressions while reducing usable context. The stable identity and
bounded context-error recovery are therefore the current protection. Any
future proactive threshold needs a bounded live cache canary with recorded
token counts before catalog metadata changes; it must not change the
Anthropic-shaped DeepSeek route.

The canary is intentionally ignored and requires explicit opt-in:
`CLUD_LIVE_CODEX_CACHE_TESTS=1 soldr cargo test -p clud --lib
codex_upstream::live_probe::cache_credit_is_reused_for_a_stable_conversation_prefix -- --ignored
--nocapture`. After the explicit operator opt-in, it permits at most two serial,
tool-free requests with a 32 KiB input ceiling and no retries. The ChatGPT
subscription endpoint rejects a hard output-cap field, so the probe has no
provider-side output limit and requires explicit owner authorization before use.
Any failed request stops the probe without a cache conclusion. Ordinary tests
and CI never invoke it.

## Conversation state and compaction

The bridge owns an in-memory canonical Responses transcript for its lifetime.
When Claude supplies `X-Claude-Code-Session-Id`, the main foreground turn is
keyed by that session and each `x-claude-code-agent-id` is isolated beneath the
same session. The identifiers are hashed before entering the map; the parent
agent header is provenance only and never selects history. Clients without
these headers use the bridge-session fallback identity. State is evicted when
the bridge stops and is bounded to 32 conversations, 16,384 items, and 64 MiB
per conversation; no transcript is persisted to disk.

The same hashed `ConversationKey` is the lifetime owner for Codex upstream
identity: `session-id`, `thread-id`, `x-client-request-id`, and the derived
`prompt_cache_key` stay stable for ordinary turns, retries, and compaction in
one main or agent conversation. `build_pipeline` is request-scoped, so it must
receive this key rather than use its default fresh client UUID; rotating that
identity defeats prompt-cache reuse on long harness transcripts. The raw
Claude session and agent headers never reach upstream or logs. A credential
route's explicit `prompt_cache_key`, when supplied, remains authoritative.
DeepSeek and other Anthropic-shaped proxy routes do not construct this Codex
client and therefore do not receive these headers or this identity policy.

## Shell tool-result budget

The Claude-to-Codex translator applies a shell text budget before constructing
Responses function-call outputs (`codex_translate::capped_tool_result`). This is
the clud-owned outbound boundary for the Bash output observed by #2593's
PostToolUse hook; that hook only observes `tool_response.stdout`/`stderr` and
cannot replace the harness-owned result. Native Codex tool execution and direct
Claude/other-provider routes do not pass through this boundary.

`CLUD_TOOL_RESULT_MAX_BYTES` defaults to **4096**; **0 disables** the budget.
Set it in the environment that launches clud/the bridge. Invalid or negative
values use the default. Exactly-at-budget and smaller results pass unchanged.
Known shell call IDs (`Bash`, `bash`, `Shell`, `shell`, `shell_command`,
`exec_command`) are matched against the request's tool-use blocks. Unknown IDs
and other tools pass unchanged. Both ordinary translation and pending-input
translation after compaction use the budget.

Oversized text retains complete lines within each half of the configured byte
budget (2048 bytes per edge by default). A line longer than an edge is omitted
whole; UTF-8 characters and lines are never split. The marker reports omitted
lines and UTF-8 bytes and gives an executable `clud tool run code/slice.py`
command with the full-output path and omitted line range. As in the Round 6
prototype, the marker is **additional** to the payload budget. Even for a tiny
budget, neither the marker nor retained edge lines are clipped to fit; the
marker explicitly says it may exceed the cap.

Oversized outputs are written to private temporary files before clipping.
Spills retain the complete translated text, not an independent capture of the
shell's OS byte streams. Structured text blocks share one budget, separated by
newlines; image parts survive. Spill errors reject translation rather than send
unrecoverable clipped text. Content hashes reuse spill paths across replayed
turns in the bridge process, preserving identical history. Deleted/changed
artifacts are recreated. Files remain available after the request and process
exit, until temporary-directory cleanup; there is no added retention policy.
Only oversized results spill. Persisted text can contain sensitive command
output, just like the original result.

The opt-in `codex_translate::tool_result_cap_translate_tests::replay_shell_results`
test feeds captured UTF-8 outputs through the production `translate_bytes`
entrypoint. Supply a JSON array of strings in `CLUD_CAP_REPLAY_INPUT` and an
output filename in `CLUD_CAP_REPLAY_OUTPUT`, then run:

```bash
SOLDR_PORCELAIN=1 soldr cargo test -p clud --lib replay_shell_results -- --ignored --nocapture
```

This exercises real request translation without making a model/provider call.
It does not establish autonomous task completion, cache-hit rates, net savings
through recovery reads, native harness coverage, or corpus-wide recall.

## Cache-health safety fuse

For direct and unified Codex traffic, the bridge retains a bounded, in-memory
health window per hashed `ConversationKey`. It records only terminal provider
usage totals: input, cache-read input, uncached input, and output. Prompts,
responses, credentials, raw harness identifiers, and cache keys are neither
retained nor written to the health log.

The first large turn after startup, compaction, clear, or a route boundary is
cold. Thereafter three consecutive 50K-or-larger requests with less than 10%
cache credit arm a session-local fuse. The bridge warns as the window becomes
degraded, then rejects the following Codex request locally before it can incur
another large replay. The refusal is an HTTP 400 `invalid_request_error`, never
a 429: Claude Code retries a 429 by itself, which is exactly the loop the fuse
exists to stop. Its message says it is not retryable and names the likely cause
(a wrong or oversized review diff range, such as a stale `@{upstream}`) and how
to resume (check the range, then `/clear` or restart). Thresholds are
unchanged, and a genuine upstream 429 keeps its bounded retry. A lifecycle boundary or bridge restart starts a new cold
window. Missing or malformed usage is inert: it cannot panic, trip, clear, or
poison later valid accounting. Anthropic-shaped DeepSeek and OpenRouter proxies
are intentionally outside this Codex-specific fuse.

A Messages request is a display/replay view, not the canonical transcript.
After a successful turn, the bridge appends only that logical turn's newly
pending input, followed by verbatim `response.output_item.done` output. It
does not re-append the historical `messages` array that the harness resends on
every call. Server output is retained as opaque JSON, preserving generated
item IDs, encrypted reasoning content, and forward-compatible unknown fields.
The pending input is the complete Messages suffix after the final assistant
turn, not merely `messages.last()`: Claude Code may represent one parallel tool
batch as consecutive user messages with one `tool_result` apiece. A terminal
assistant prefill remains pending for compatibility. Before an
inference request reaches upstream, the bridge verifies that the assembled
canonical input has an output for every function call. A mismatch returns a
local 400 and records only conversation scope plus fixed item kinds/counts in
the failure log; call IDs, tool payloads, Messages content, and raw Claude
identity headers remain absent.
Failed or partial turns are never committed. If a completed turn's input and
output cannot fit the per-conversation item or byte limit, the client still
receives that completed reply; only after its downstream response is committed,
the bridge atomically clears that conversation's canonical transcript. The next
replay therefore seeds fresh canonical history rather than continuing stale
items.

A failed streamed turn can leave its triggering tool result uncommitted even
though Claude retained that result in its display transcript. If Claude then
records a synthetic error or partial assistant block, the result precedes the
ordinary final-assistant pending suffix. Continuation assembly reconciles
unresolved canonical calls against real outputs in the complete Messages replay
and commits each recovered output exactly once with the next successful turn.
It never invents a result when the replay does not contain one, and it never
replays model output or tool effects from the failed turn. Recovery diagnostics
contain only conversation scope and a count, not call IDs or payloads.

When a first inference attempt fails with the exact
`context_length_exceeded` code **before any Anthropic-visible text, reasoning,
or tool-call frame**, the bridge performs one bounded recovery cycle. The code
is accepted from either a non-2xx JSON error envelope or an HTTP 200
`response.failed` SSE event; status codes and free-form messages are not
signals. The bridge sends only prior canonical history to `/responses/compact`,
validates the opaque compaction output, atomically replaces canonical history,
appends the pending current input exactly once, and retries inference once.
`response.created` is only a protocol envelope and does not count as visible
output. No other error code recovers; a compact error, malformed compact
output, cancellation, a second context-full response, or any output before
failure stays terminal. Recovery suppresses every frame from the failed
attempt, so the client receives only the retry's single valid response
sequence.

The authenticated loopback listener also exposes three launch-private lifecycle
controls. Every bridged Claude launch registers session-local HTTP hooks through
a protected temporary `--settings` file: `PreCompact` (manual or automatic)
calls compact before Claude mutates its transcript, and `SessionStart(clear)`
clears the bridge after Claude starts the fresh session:

- `POST /_clud/context/compact` compacts the bridge's canonical transcript and
  replaces it only after a valid opaque response. Empty history makes no
  provider request but arms the same post-compaction reset. If the transcript
  has an outstanding function call or the selected
  credential route cannot compact it, the bridge acknowledges the hook and
  enters a harness-compaction fallback instead of blocking Claude.
- `POST /_clud/context/compact-finished` handles `SessionStart(compact)`. It
  completes a pending fallback by discarding the pre-compaction transcript that
  Claude's summary inference temporarily replayed. The next ordinary turn then
  seeds canonical history from Claude's compacted transcript. When provider-side
  compaction succeeded, this control is a no-op and preserves its opaque output.
- `POST /_clud/context/clear` clears the addressed Claude session and all of its
  agent descendants, then performs no upstream inference or compaction request.

All three routes use the same loopback URL and launch-scoped
`ANTHROPIC_AUTH_TOKEN` already supplied to the child. The generated settings
interpolate the bearer from the environment, so it never appears in argv.
Routes accept either an empty direct-control body or the exact matching Claude
lifecycle JSON, serialize with normal turns, and return `204` on success. They
are not part of the public Anthropic Messages surface.

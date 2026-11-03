# Launch Targets and Sticky Preferences

clud resolves two independent launch dimensions before harness bootstrap,
setup, or launch-plan construction:

- `ModelProvider` is the API/model family selected by provider flags.
- `HarnessSelection` is `default`, `claude`, `codex`, `deepseek`, or `opencode`, selected by
  `--harness`.

`default` maps the provider to its native harness: Claude to Claude and Codex
to Codex. The resolved `Backend` remains the concrete executable compatibility
type used by bootstrap, setup, argv construction, and runners. New code must
not infer the provider from that legacy field; use `ResolvedLaunchTarget` or
`LaunchPlan::model_provider()`.

DeepSeek has two deliberately separate meanings. `--deepseek` keeps its
backward-compatible meaning: use the DeepSeek API through Claude Code.
`--harness deepseek` selects DeepSeek AI's `dsh` executable. A bare `dsh`
launch maps to `dsh web`; a prompt-bearing launch maps to
`dsh --profile headless <prompt>`.

OpenCode selection, provider configuration and acceptance tests are specified
in [opencode.md](opencode.md).

## Resolution

`backend::resolve_launch_target` is the single precedence and validation
function. Each dimension independently uses:

```text
explicit CLI > ~/.clud/settings.json > built-in default
```

The built-in provider is Claude and the built-in requested harness is
`default`. Each resolved value carries a `PreferenceSource`: `cli`,
`global_setting`, or `built_in_default`.

The supported cross-route in phase 1 is Codex provider through Claude:

```text
clud --codex --harness claude
```

Claude provider through Codex is rejected before bootstrap with:

```text
unsupported launch target: Claude provider cannot use the Codex harness
```

There is no fallback. HTTP translation and credentials are later phases under
issue #622.

## Bare interactive harness launcher

A command-less launch on a real input and error terminal discovers `claude`,
`codex`, and `dsh` in that stable order. One installed harness launches
immediately. Multiple installed harnesses open the crossterm selector, whose
highlighted row auto-launches after three seconds. Up/Down or j/k moves the
highlight and disables the timeout; Enter confirms and Esc/Ctrl-C cancels.
Like every clud selector, it renders through `selector.rs` (#1195, DD-073).

The confirmed choice is stored as `launcher.last_harness` in
`~/.clud/settings.json`. This is launcher history only and does not alter
`harness.default`, provider profiles, or routing policy. An absent saved
harness falls back to the first installed row. Explicit provider/harness
intent, prompts, non-TTY launches, dry runs, subcommands, and daemon-style
launches bypass discovery and remain deterministic.

Before this picker, a bare live direct launch with no saved route checks for
the one unambiguous credential-backed route: a validly shaped stored DeepSeek
key, Claude harness route, and known unauthenticated Claude/Codex status.
That route launches DeepSeek *through Claude* without selecting native `dsh`.
Unknown auth status keeps the existing picker/default behavior. `clud run`
normalizes to the same path; explicit flags, saved routing, and dry runs win.

## DeepSeek Harness install and providers

Issue #1829 makes `dsh` a first-class install target ([DD-160](../DESIGN_DECISIONS.md#dd-160-deepseek-harness-is-a-clud-managed-npm-prefix-driven-by-a-launch-overlay)).
`--harness dsh` is an alias of `--harness deepseek`. `src/dsh_harness.rs` owns
the contract:

- **Install.** Discovery checks PATH first, then
  `~/.clud/harnesses/dsh/<version>/node_modules/.bin/dsh` (`dsh.cmd` on
  Windows). A user-managed `dsh` on PATH always wins and is never touched. A
  missing `dsh` on a terminal gets the same `[y/N]` prompt as Claude and Codex
  (default No); non-interactive launches name `clud dsh-update`, which installs
  or confirms the reviewed release explicitly. The install runs
  `npm install --prefix <staging>` for the pinned `@deepseek-ai/dsh` and a
  pinned private `node` package, verifies `dsh --version`, and renames the
  staging directory into place, so a failure leaves no partial prefix. Only npm
  is required from the system; global npm, the system Node, shell startup files
  and `$DSH_HOME` are never written.
- **Runtime.** A managed `dsh` runs with its prefix's `.bin` first on PATH, so
  it boots on the private Node 24 even where the system has Node 20 (below
  dsh's floor) or Node 26 (which fails dsh's native addon).
- **Keys.** `ForegroundRuntime::start` hands the child `DEEPSEEK_API_KEY`, or
  `OPENROUTER_API_KEY` for an OpenRouter launch, from clud's vault. An ambient
  value wins. Keys go only into the child-local environment, never into dsh's
  `.credentials.yaml`. The credential preflight runs for OpenRouter and for an
  explicit `--deepseek`; a bare `--harness dsh` still runs on a key dsh stores
  itself.
- **OpenRouter.** `clud --openrouter --harness dsh` appends
  `--patch ~/.clud/harnesses/dsh/patches/openrouter-<model>.yml`, which the
  runtime writes before spawn. The overlay makes dsh's built-in `openrouter`
  route active with `api: anthropic-messages`, the descriptor's
  `https://openrouter.ai/api` base URL, `apiKeyEnv: OPENROUTER_API_KEY`, and
  the resolved model declared explicitly (so an ID outside dsh's catalog still
  resolves). `--model` and the saved OpenRouter model reach dsh through the
  ordinary OpenRouter selection. The user's own dsh profiles are never edited.

## Foreground Codex-through-Claude bridge

Issue #626 gives the supported cross-route a foreground-only runtime shell.
`ForegroundRuntime` starts one authenticated bridge before the first harness
spawn, supplies its child-local environment to both `ManagedSubprocess` and
`NativePtyProcess`, and owns the bridge until the whole foreground runner
returns. Native provider/harness pairs retain their original environment
unchanged. The same bridge spans every in-process foreground iteration; only
daemon, detached, and repeat-worker ownership remain later #622 phases.

The cross-route overlay replaces `ANTHROPIC_BASE_URL` and
`ANTHROPIC_AUTH_TOKEN`, removes ambient `ANTHROPIC_API_KEY`, and adds Claude's
long request timeout/nonessential-traffic tuning only when the user has not set
those values. No parent-process environment is mutated. The bearer and complete
base URL are deliberately absent from Debug/error/report surfaces.

For the same launch only, `ForegroundRuntime` adds a protected temporary Claude
settings file that merges two authenticated HTTP hooks with the user's existing hooks. `PreCompact`
(`manual|auto`) compacts bridge-owned canonical history before Claude compacts
its transcript, while `SessionStart(clear)` clears the bridge after `/clear`.
The hook URL is launch-local and the authorization header interpolates
`ANTHROPIC_AUTH_TOKEN` from the child environment, so neither the bearer nor the
complete bridge URL enters argv. The settings file is deleted with the runtime.

`codex_bridge.rs` binds only `127.0.0.1:0` and routes authenticated
`POST`/`HEAD /v1/messages`, with an explicit unsupported 404 for token counting
and 404 elsewhere. Since #627 step 5 the `POST` handler runs the real pipeline
rather than a fixture. Header/body sizes and worker concurrency
are bounded; handle Drop signals shutdown and joins the listener plus admitted
workers. See DD-027 for why this parser uses `std::net` instead of the
repository's existing `tiny_http` dependency.

Each I/O phase carries its own budget rather than sharing one connection
deadline: `header_timeout` (5 s) and `body_timeout` (30 s) are absolute, while
`stream_idle_timeout` (300 s) is re-armed per frame so a long model turn is
bounded by silence, not by elapsed time. Streamed replies go through
`write_event_stream`, which uses chunked transfer encoding and flushes one SSE
event at a time; `write_response` remains the `Content-Length` writer for
errors, `HEAD`, and non-streaming bodies. Accepted sockets are explicitly put
back into blocking mode, and a read timeout is fatal only once the phase
deadline has passed. See DD-028 — the previous single-deadline arrangement
answered `408` to any request whose body arrived in a later TCP segment.

The *upstream* hop of the Anthropic passthrough proxy reads through an agent
with `timeout_connect` / `timeout_read` / `timeout_write` set to
`stream_idle_timeout`, so its budget is byte-idle in the same sense. It used
`ureq`'s `Request::timeout()` until #1263, which is a whole-request deadline
that overrides `timeout_read()` — a long stream was cut off mid-turn and a
quiet socket was only noticed after the same absolute wall-clock. A mid-stream
failure there is reported in-band (DD-029) rather than closed like a clean end
of stream, and a timeout before the first frame answers 504 `timeout_error`
instead of 502. See DD-028's amendment and DD-079.

The *direct* anthropic-compat route is the one launch shape with no clud
process in the request path, so no byte-idle watchdog is possible: the overlay
sets `API_TIMEOUT_MS` and the launch notice names the effective value and says
clud cannot detect a hang there. See DD-079.

## Request translation

`codex_translate.rs` maps an Anthropic Messages request onto an OpenAI
Responses request (#627 step 2). It is a pure function over typed structs with
no HTTP, socket, or credential dependency, so the mapping table can be driven
directly by fixtures; step 5 wires it into the bridge.

The shape difference that drives the design: one Messages `content` array can
become several Responses input items. An assistant turn holding text plus two
`tool_use` blocks becomes one message item and two `function_call` items, and
`tool_result` blocks become top-level `function_call_output` items rather than
message content. Order is preserved because the model reads the result as a
transcript.

Notable mappings: `system` → `instructions`; `input_schema` → `parameters`;
`tool_choice: any` → `required`; `disable_parallel_tool_use` (an opt-out) →
`parallel_tool_calls: false` (an opt-in), sent only when asked; `max_tokens` →
`max_output_tokens`; `thinking.budget_tokens` → the coarse `reasoning.effort`
ladder. A `claude*` model id resolves to the Codex default, while any other id
is honoured verbatim as an explicit override.

Unknown *top-level* request fields are tolerated so an additive Anthropic API
change cannot break the route, but anything that changes meaning and has no
faithful equivalent — `top_k`, `stop_sequences`, thinking blocks replayed in
history, images inside a `tool_result`, unknown block or `tool_choice` types —
is an explicit error rather than a silent drop.

## Response streaming

`codex_sse.rs` translates the upstream Responses SSE stream into Anthropic SSE
(#627 step 3), in two separable layers so fragmentation can be tested apart
from event semantics:

- `FrameDecoder` is byte-level. Line terminators are normalised on ingest (the
  SSE spec treats CRLF, LF, and bare CR alike), and a `\r` at the end of a read
  is held back so a CRLF split across two segments is not misread as CR plus a
  blank line. Comments/heartbeats are skipped, multiple `data:` lines join with
  a newline, and `finish()` flushes a final frame that arrived without a
  trailing blank line.
- `StreamTranslator` is semantic. It allocates Anthropic content-block indices
  monotonically in the order blocks open, keyed off the upstream
  `(output_index, content_index)`, so interleaved text and parallel tool calls
  each address their own block.

The invariant that shapes the design: **never emit a malformed Anthropic
block.** A `content_block_start` for a tool call cannot be sent until the
call's id and name are both known, so argument deltas that arrive first are
buffered and flushed once the block is legally open. A half-formed tool block
is worse than a late one — it makes the client fail to parse a turn it could
otherwise have used.

Termination is guaranteed: a truncated or disconnected stream still closes
every open block and emits `message_delta`/`message_stop`. Upstream failures
close open blocks and then emit a **sanitized** error — the upstream body is
never echoed, since it can carry account identifiers or key fragments and this
frame is written straight to the harness.

Reasoning summaries are deliberately not forwarded: an Anthropic `thinking`
block carries a signature the Responses API does not supply, and an unsigned
one is rejected by clients.

## Upstream client

`codex_upstream.rs` (#627 step 4) splits two concerns. `CredentialSource`
yields the base URL, auth material, account headers, and model policy;
`UpstreamClient` performs one streaming `POST /v1/responses` and hands each
byte chunk to a sink as it arrives. The trait is the seam that lets #629's
subscription auth reuse every line of translation unchanged. `ApiKeyCredentials`
is the only implementation today, and it has **no fallback chain** — a missing
`OPENAI_API_KEY` is an error, never a silent downgrade, because #629 requires
that subscription and platform credentials never substitute for one another.

> **Update (#629):** The preceding API-key-only description records the
> phase-3 baseline. The current implementation has both sources and follows
> the explicit selection rules below.

### Credential source selection (current)

`CludSubscriptionCredentials` and `ApiKeyCredentials` are the two production
sources. An existing `~/.clud/codex-auth.json` subscription record is an
explicit persisted selection and is used alone. Only when that record is absent
may `OPENAI_API_KEY` be selected; an expired or invalid record tells the user
to re-login rather than silently falling back to the API key.

## ChatGPT subscription authentication

Issue #629 adds `clud codex-auth login|status|logout`. It owns exactly one
credential record, `~/.clud/codex-auth.json`; it never reads, writes, or
deletes Codex CLI's `~/.codex/auth.json`. Login requires explicit experimental
acknowledgement and uses ChatGPT authorization-code OAuth with PKCE S256, a
random state value, and `127.0.0.1:1455/auth/callback` (1457 only as a port
fallback). The compatibility shape follows the public `openai/codex` client;
clud neither spawns nor depends on that client.

The record is locked with `fs4`. Refresh re-reads after acquiring the
cross-process lock, refreshes at most once, and atomically replaces the file.
Refresh happens on use near expiry. A pre-output upstream 401 triggers one
locked recheck/forced refresh and at most one request retry; an in-band auth
failure after output marks the next turn for recheck instead of replay.
Safe pre-send and 429 token-endpoint failures are retried within a bounded
budget; ambiguous read-timeout, 408, and 5xx outcomes return a temporary
error without replaying a potentially rotated grant. `invalid_grant` and
other rejected-refresh codes require a new login or an
explicit, identity-aware native-login import in an interactive foreground
launch. The native file is never changed.
Unix writes use mode 0600. Windows applies a protected owner-only DACL before
the temporary file receives credentials and verifies both that ACE and the
protected-DACL control flag after replacement; inherited profile ACLs alone
are not treated as sufficient. Administrators with backup/restore privileges
remain an operating-system trust boundary.
Tokens are excluded from `Debug`, errors, JSON status, bridge logs, and launch
plans. `status` reports source, safe identity, expiry, and refresh state;
`logout` removes only this clud-owned record.

## Bridge security and resource boundary (#630)

The trust boundary is explicit: CLI/settings choose a `LaunchPlan`; foreground
runtime turns that plan into a child-only environment; the Claude harness can
then reach exactly one ephemeral `127.0.0.1` listener using its per-launch
bearer. The listener translates only the supported Messages routes and creates
the upstream Responses request itself. It never forwards arbitrary downstream
headers, the local bearer, ambient Anthropic credentials, or an upstream error
body. The daemon IPC carries additive plan fields for compatibility, but it
does not own a bridge for the normal foreground route.

All input and lifetime boundaries are bounded: headers (32 KiB), body (32
MiB), header/body/frame deadlines, one active worker with a finite admission
wait, and a kernel backlog rather than unbounded user-space queues. Disconnect,
cancellation, timeout, bridge drop, and repeated shutdown close sockets and
join workers idempotently. Fragmentation and malformed SSE sequences are
deterministically tested. Once a downstream SSE frame is visible, the upstream
client will not retry, preventing duplicate text or tool calls.

Failure diagnostics are synthesized from allowlisted facts and scrubbed before
they reach stderr or the bounded bridge log. A healthy turn records nothing;
since #999 a catalog fetch writes an entry -- on the unified route as well as
the direct Codex one since #1021 -- but only a failure, refusal, or truncation
makes the launch print the log's path on shutdown. The opt-in local benchmark is documented at
[`bench/codex_bridge`](../../bench/codex_bridge/README.md); it measures the
full local request path and reports RSS growth without a flaky normal-CI
timing threshold.

### The retry boundary

The rule is *never replay after downstream-visible output has begun*, and the
step-1 streaming writer is what makes it absolute: once a single SSE frame has
been flushed, the `200` and its headers are already on the wire, so there is no
status left to change and a replay would duplicate content the user has already
seen. The client tracks whether the sink has accepted anything and refuses to
retry once it has, however retryable the failure looks. That rule is unchanged
by everything below: classification only ever widens the *pre-commit* window.

### Failure classification (#764)

A status alone is not enough to decide whether retrying can help. Upstream
returns permanent rejections wearing a 5xx costume — a model that needs a newer
client, an unsupported parameter — and retrying those can never succeed, while
burning quota and, with enough concurrency, tipping a healthy account into
cooldown.

So the error response is **read rather than discarded**. `capture_failure`
takes a bounded 8 KiB prefix of the body plus `cf-ray`, `x-request-id` and
`Retry-After`, reduces them to an `UpstreamFailure`, and drops the raw bytes.
Nothing downstream ever sees the body; the scrubbed one-line `detail` exists
for the operator log only, and `scrub` redacts token-shaped runs before it is
even retained.

`FailureClass` then drives the budget:

| Class | Recognised by | Attempts |
| --- | --- | --- |
| `Permanent` | a permanent body signature, or a passed-through 4xx | 1 |
| `Transient` | transport, `408`/`429`, or a 5xx body that reads like an outage | `max_attempts` |
| `Unknown` | any other 5xx | `unknown_max_attempts` |

`Unknown` is deliberately not folded into `Transient`. Treating every
unrecognised 5xx as fully retryable is what produces the cascade documented in
CLIProxyAPI#4327, and treating it as permanent would break on a legitimately
new transient code; a reduced budget is the safe middle.

Backoff is exponential with **jitter** over the lower half of each window, so
clients that fail together do not retry in lockstep, capped per-sleep by
`max_retry_delay` and in total by `max_retry_elapsed`. A `Retry-After` hint
wins over the computed delay but is clamped by the same ceiling, so a generous
server hint cannot pin a turn open. A `usage_limit_reached` body's
`resets_in_seconds` is parsed and surfaced in the client message.

Timeouts follow the same split as the bridge's own: connect, an *idle* read
timeout (a model may think for minutes before its first token), and an overall
deadline. Cancellation is polled between reads, so its latency is bounded by
the read timeout rather than immediate — the cost of not putting an async
runtime behind a synchronous bridge.

No downstream header is forwarded upstream; the module constructs every
outbound header itself, so the harness's own Anthropic bearer cannot leak into
an upstream request. Transport failures are classified into fixed strings
rather than carrying the library's message, which embeds the URL, and upstream
error bodies are never propagated because they can contain account identifiers
and key fragments.

## Request pipeline

`codex_pipeline.rs` (#627 step 5) chains the pieces above into one call:

```text
Anthropic request -> codex_translate -> codex_upstream -> codex_sse -> Anthropic SSE
```

Upstream is **always** streamed, even for a non-streaming Messages request:
`MessageAggregator` folds the translated Anthropic events back into a single
`Message`, so the non-streaming shape reuses the state machine step 3 fuzzed
instead of introducing a second, separately-wrong mapping.

Status selection follows the same committed/uncommitted boundary as the retry
policy. Before any frame is written, a failure picks a status:

| Failure | Status |
| --- | --- |
| malformed request | `400` |
| invalid local bridge bearer | `401` |
| missing or rejected upstream credentials, or upstream `401` after recovery | `502` |
| temporary credential refresh failure | `503` |
| upstream `400`/`403`/`404`/`413`/`422`/`429` | passed through |
| any other upstream status, or a transport failure | `502` |
| overall deadline elapsed | `504` |
| response over the byte budget | `413` |
| cancelled, or the downstream client hung up | `499` |

`502` is reserved for failures that really are gateway failures (#764). It used
to be the catch-all for five unrelated cases, which made an edge blip, a TLS
reset, an oversized response and a genuine outage indistinguishable in a log.

`client_message` carries the status, an opaque `x-request-id` when upstream
supplied one, and a rate-limit reset hint when the body had one — never the
body itself. Issue #772 adds an always-on forensic floor at
`~/.clud/state/sessions/<pid>__<epoch>/bridge.jsonl`: failures and retry
attempts, buffered across workers, capped at 1 MiB with a visible
`truncated` marker. Records contain only fixed bridge reasons and fields already
exposed by `UpstreamFailure` (status, class, correlation IDs, retry hints, and
scrubbed detail); request/response bodies, credentials, bearers, authorization
headers, and upstream URLs are never inputs to the logger. Issue #999 adds the
model-discovery handshake to that floor **on the direct Codex route only** — a
`catalog_advertised` entry per `GET /v1/models` served by `serve_codex_catalog`,
and the requested model ID on a refusal from `serve_codex_discovery_messages` —
so a session wedged by a model selection is diagnosable; a direct-Codex launch
that fetches the catalog therefore does create a file. Unified mode is not
covered: `serve_unified_catalog` is not given the log, and
`serve_unified_messages`' own unknown-model refusal is not recorded, so a
unified launch still logs failures and retries only. The advertisement is
recorded as ambient context, so the shutdown print of the log's path still fires
only for a launch that recorded a failure, a refusal, or a truncation.
`CLUD_CODEX_BRIDGE_DEBUG=1` remains the richer interactive stderr tier.
Unit-test builds and processes marked `CLUD_INTEGRATION_TESTS=1` use the sibling
`~/.clud/state/test-sessions/` tree so synthetic failures never enter the
production log inventory.

Once `EventStreamWriter` has flushed a frame the response is committed, so a
later failure is reported in-band as a sanitized SSE `error` event and the
chunked body is simply terminated. That is why the writer defers its headers
until the first frame.

The debug seam (`CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL`, still gated on a debug
build *and* `CLUD_INTEGRATION_TESTS=1`) now points at a **Responses-shaped**
fake. Phase 2 pointed it at a passthrough that echoed the Anthropic body, which
meant the end-to-end tests proved transport and auth but nothing about
translation; the integration tests now assert on the request the fake actually
receives, so a translation regression fails there.

## Resource bounds

The fixture-era defaults were replaced in #627 step 6, each with a stated
reason rather than a guess:

- **Body cap: 32 MiB** (was 1 MiB). The governing principle is that *the bridge
  must not be stricter than the endpoint it impersonates* — Claude Code sizes
  its requests against the real Anthropic API, so a lower cap turns a
  legitimate request into a bridge-only `413` that reads as a client bug. A
  single base64 screenshot already exceeds 1 MiB before any text is counted;
  `a_representative_request_fits_the_body_cap` builds a request from the parts
  a real turn always carries and asserts it clears the old cap and fits the new
  one.
- **Concurrency: 2** (#989), preserving #778's host-footprint cap. Exceeding it
  queues rather than failing. Claude Code issues several requests at once — the
  foreground turn plus background side-model calls and any subagents. While
  every slot is busy the accept loop simply declines to accept, so pending
  connections wait in the kernel's listen backlog until a slot opens or the
  bridge shuts down. clud never accepts them to buffer a body or synthesize
  `503 bridge busy`; each active worker's five-minute first-frame and
  stream-idle timeouts still bound a silent upstream wedge. Queue wait is recorded separately from upstream retry
  events, with an aggregate upper-bound duration and without request content or
  secrets.

The representative request is *constructed*, not captured production traffic.
That distinction is deliberate and is stated in the test: it is evidence about
the shape and scale of a real turn, not a measurement of one.

## Persistence

Global launch preferences use the existing settings document and `fs4` lock:

```json
{
  "backend": { "default": "codex" },
  "harness": { "default": "claude" }
}
```

`GlobalSettingsPatch` applies model, harness, and settings-TUI changes inside
one locked read/modify/write. The settings TUI uses one typed choice state
machine for the provider and harness rows. The same generic `ChoiceSelector`
drives the inline session/global launch-scope selector.

An interactive explicit provider or harness selection offers:

- **Session only**: use it for this invocation without writing settings.
- **Globally**: atomically persist the explicitly selected dimensions and the
  effective harness's global setup scope.

When a saved non-`default` harness affects a real TTY launch, clud prints one
green stderr line before spawning:

```text
[clud] Harness override: Claude (global setting)
```

It is suppressed for non-TTY stderr and structured output. `--dry-run` reports
the effective values and their sources instead.

## LaunchPlan and compatibility

Every newly built `LaunchPlan` carries `model_provider`, `requested_harness`,
`effective_harness`, `provider_source`, and `harness_source`. These are
`serde(default)` options because old daemon/worker payloads contain only
`backend`. Accessors fall back to that legacy field, preserving old native
behavior.

The daemon worker receives the same plan. Repeat jobs also pin the resolved
provider and requested harness in their one-shot argv so a settings edit cannot
silently change an existing job.

`backend` remains in dry-run JSON for compatibility and names the effective
executable. The additive fields make both dimensions explicit:

```json
{
  "backend": "claude",
  "model_provider": "codex",
  "requested_harness": "claude",
  "effective_harness": "claude",
  "provider_source": "cli",
  "harness_source": "cli"
}
```

## DeepSeek: direct Claude-child provider, not a bridge (#874-#881)

`clud --deepseek` selects `ModelProvider::DeepSeek`. Its only supported
harness is Claude — `--deepseek --harness codex` is rejected before
bootstrap, the same way `--claude --harness codex` is — and it does not
accept `--model`: the initial integration ships one fixed, documented model
profile rather than passthrough configuration. Both rejections happen in
`backend::resolve_launch_target` / `backend::validate_provider_options`
before any launch work is accepted.

An API key may be passed inline: `clud --deepseek <API_KEY>` (likewise
`--kimi`, `--openrouter`, or `--provider` naming one). `Args::parse_from_raw`
lifts the first `sk-…`-shaped token out of the backend argv
(`Args::extract_inline_api_key`), and `main.rs` stores it in the provider's
native-vault record before the credential preflight runs, so the preflight
finds it and never prompts. The key is never forwarded to Claude Code — before
this, 2.8.1 passed it through as the session's opening prompt, which left fresh
installs (typically Windows) unable to use the key at all and sent it to the
model where a key was already stored. `--dry-run` strips the key but never
touches the vault.

This is deliberately **not** a Codex-style bridge. There is no local HTTP
listener, no request translation, no `BridgeHandle`. DeepSeek publishes an
Anthropic-compatible endpoint directly, so `ForegroundRuntime::start` routes
it straight to a child-only environment overlay — `is_deepseek_via_claude`
and `is_codex_via_claude` are separate, mutually exclusive branches of the
same dispatch, and only one of Claude, native Codex, the Codex-to-Claude
bridge, or the DeepSeek direct route applies to any given launch (see the
routing-matrix test in `foreground_runtime.rs`).

### Credential trust boundary

Exactly one API key is stored, in the OS-native encrypted credential vault:
Windows Credential Manager (direct `Advapi32`/`CredWriteW` calls — lowercase
in the `#[link]` attribute, since xwin's vendored SDK used for the
cross-compiled CI build normalizes lib filenames to lowercase and a
case-sensitive host linker cannot find `Advapi32.lib`), macOS Keychain, and
Linux Secret Service, both via the `keyring` crate pinned at `=3.6.3`. Linux
uses the `async-secret-service` feature (zbus, pure Rust) rather than
`sync-secret-service`, which would pull in `dbus-secret-service` and require
the system `libdbus` C library at build time — a real dependency this
project does not want.

`clud deepseek-auth login|status|logout` manage that one record.
`deepseek_auth::SecretStore` is the injectable boundary (`get`/`set`/
`delete`); production uses `NativeSecretStore`, and every test — including
`ForegroundRuntime`'s routing tests — injects an in-memory fake, so normal
automated test runs never touch a real vault. The raw key is never written
to clud JSON settings, argv, a `LaunchPlan`, daemon/worker IPC, dry-run
output, status output, logs, or `Debug` output; `deepseek-auth status`
reports only configured/not-configured state.

### Launch-time preflight

Before any foreground, detached, detachable, or repeat-loop DeepSeek launch
is accepted, `main.rs` calls `deepseek_auth::preflight_native`. Two pure,
unit-tested predicates decide the shape of that check:

- `launch_needs_preflight(provider, dry_run)` — false for every non-DeepSeek
  provider and for `--dry-run`, which makes zero vault calls.
- `launch_is_interactive(args, backend, stdin_is_terminal, stderr_is_terminal)` —
  true only for a genuine interactive foreground launch: both streams are a
  real tty, the effective harness classifies the prompt as interactive, not
  `--detach`/`--detachable`, and not a `--repeat` loop. Prompt classification is
  backend-aware: Codex built-ins are interactive, while `-p` and `loop` remain
  headless and DeepSeek retains its headless prompt profile.
  `grind` uses one normal Claude-harness PTY prompt seeded with `/loop`, with
  repetition owned by the harness; see [grind.md](grind.md). Other harnesses
  reject `grind` before launch.

A missing key may be entered via hidden terminal input only when
`launch_is_interactive` is true. Every other case — noninteractive,
detached, detachable, repeat — fails immediately with the exact
`clud deepseek-auth login` instruction rather than hanging on a prompt
nobody can answer.

### Child environment overlay

At the `ForegroundRuntime` spawn boundary, the initiating process's own
preflight has already guaranteed the key exists, so `apply_anthropic_compat_overlay`
reads it from the vault a second time — directly in the process that is
about to spawn the child, foreground or worker — and builds a DeepSeek-only
overlay for the Claude child: the documented Anthropic-compatible endpoint,
auth token, and the resolved model and context profile. Effort is not
overlaid: the catalog's `low` default rides the harness's `--effort` session
flag so `/effort` stays live (DD-059). The default model and the
haiku/subagent model come from the
[served DeepSeek model names](provider-selection.md#served-deepseek-model-names);
today that is `deepseek-flash[1m]` (DeepSeek-V4.1-Flash) in every slot,
including haiku/subagent. Explicit Pro/Flash and
auto/1m selections are applied as documented in
[provider selection](provider-selection.md). The 1m compaction threshold is
emitted only for a 1m wire profile. Every conflicting inherited Anthropic/profile
variable is removed **unconditionally** case-insensitively first — not
gated on OS the way the pre-existing Codex cross-route overlay's comparison
is (`env_key_eq`, which mirrors real per-platform env-var uniqueness
semantics and is intentionally case-sensitive on Unix). DeepSeek's removal
is a secret-hygiene guarantee, not an OS-semantics match, so it must hold
the same way on every platform — a distinction caught only by running the
overlay test through the `clud-docker-linux-build` container, where the
platform-gated comparison silently no-ops.

The overlay is child-local: the parent process's environment is never
mutated, and `ForegroundRuntime`'s `Debug` impl exposes only
`bridge_active`/`environment_entries` — never environment contents — so the
token cannot leak through a crash report or log line that `Debug`-prints
the runtime.

### What crosses the daemon/worker boundary

Only typed provider/harness metadata and the normalized model selection
(model and wire IDs, effort, context, and sources) cross in `LaunchPlan`.
`LaunchPlan` has no field that could carry a credential, so there is nothing
for daemon IPC to leak structurally; the wire round-trip test in
`daemon/wire_prost/tests.rs` proves `ModelProvider::DeepSeek` and its selection
survive encode/decode and that the encoded payload contains no secret-shaped
value. A daemon-spawned worker retrieves the key itself, the same way the
foreground path does, by calling
`ForegroundRuntime::start` locally in the worker process — never through
the daemon.

### Out of scope

Uncatalogued DeepSeek capabilities and Codex OAuth credential migration remain
out of scope; see issue #874's decisions and the provider-selection catalog for
the supported DeepSeek profiles.

## OpenRouter: vault-backed direct Claude gateway (#939)

`clud --openrouter` is another descriptor-backed Anthropic-compatible direct
route. It reuses `provider_auth::NativeSecretStore`, preflight, settings,
daemon/repeat metadata, and `ForegroundRuntime` dispatch, but stores a distinct
credential at `clud.openrouter/api-key-v1`; it never reads or copies the
DeepSeek/Kimi records. Dry runs resolve the provider without touching a vault.

The child-only overlay targets `https://openrouter.ai/api` (no `/v1`), places
the vault secret only in `ANTHROPIC_AUTH_TOKEN`, explicitly sets
`ANTHROPIC_API_KEY` to the empty string, and enables
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`. OpenRouter's documented Claude
role aliases are pinned independently: Opus, Sonnet, Haiku, and Opus for
subagents, plus OpenRouter's documented Fable alias. The parent environment is
unchanged and the route creates no local bridge or listener.

This direct connector guarantees only the Anthropic-first-party Claude path
that OpenRouter documents as compatible with Claude Code. Non-Claude models are
best-effort. OpenRouter through the Codex harness and OpenRouter in clud's
unified gateway are separate scopes because they use different protocol and
routing contracts. Clud does not delete Claude Code-owned cached login state;
users encountering an auth conflict must run `/logout` once in Claude Code.

## `clud video`: video-use as a session-scoped plugin (#1851)

`clud video [DIR] [--update]` (`video.rs`) runs the third-party
[browser-use/video-use](https://github.com/browser-use/video-use) skill in one
Claude session and nowhere else
([DD-163](../DESIGN_DECISIONS.md#dd-163-video-use-is-a-session-scoped-plugin-pinned-to-a-reviewed-sha)).

- **Managed checkout.** `~/.clud/extern/video-use/<VIDEO_USE_SHA>/`: a
  `git fetch --depth 1` of the pinned commit into a sibling staging dir,
  verified (`HEAD` equals the pin, `SKILL.md` present, `uv sync` succeeded,
  then a `.clud-verified` marker), and renamed into place. Python deps live in
  that checkout's private `.venv`. `--update` forces a reinstall and exits.
- **Plugin wrapper.** `~/.clud/extern/video-use-plugin/` holds
  `.claude-plugin/plugin.json` (`name: clud-video-use`) and
  `skills/video-use`, a symlink to the checkout (a copy without `.git` where
  links fail). Claude Code discovers plugin skills at
  `<plugin>/skills/<name>/SKILL.md`; `claude plugin validate` accepts the
  layout, and a session follows the symlink.
- **Launch.** `command/builder.rs` adds `--plugin-dir <wrapper>` and a
  `/video-use ...` seed prompt (inventory, propose, wait for the user's OK)
  only for `Command::Video`; a plain launch has neither. `video_launch_error`
  refuses non-Claude harnesses, because `--plugin-dir` is a Claude Code flag.
  `DIR` becomes the session's working directory.
- **Preconditions** (real launches only, after the `--dry-run` exit, so a dry
  run installs nothing and reads no vault): `ffmpeg`/`ffprobe` on PATH (one
  message with a platform install hint; clud never runs a package manager),
  the managed install, and the ElevenLabs key.
- **Key.** An ambient `ELEVENLABS_API_KEY` wins; then the native vault at
  `clud.elevenlabs/api-key-v1` (the debug-only test vault in tests); then, on
  a TTY, one masked prompt whose answer is stored; otherwise an error. clud
  sets the key in its own environment immediately before spawning the
  harness, which inherits it; it never appears in the plan or `--dry-run`.
- **Never automatic.** The upstream skill is never written to
  `~/.claude/skills` or `~/.codex/skills`.
- **On demand: `/video-use` (#1866).** A second entry point for an ordinary
  session: the bundled bridge skill `assets/skills/video-use/` is
  explicit-only (`disable-model-invocation: true` for Claude, whose docs say
  the description then stays out of context; `allow_implicit_invocation:
  false` in `agents/openai.yaml` for Codex), so it costs no context until the
  user types `/video-use`. It runs `clud video --path`, which installs the
  pinned checkout if needed and prints it, then follows that checkout's
  `SKILL.md`. The key comes from an ambient `ELEVENLABS_API_KEY`; otherwise
  the user runs `clud video` once. See
  [DD-164](../DESIGN_DECISIONS.md#dd-164-video-use-is-an-explicit-only-bundled-bridge-skill).

## Ownership

- `backend.rs`: typed dimensions, precedence, validation, notice policy.
- `args.rs`: `--harness` parsing and passthrough ownership.
- `clud_settings.rs`: locked reads and atomic typed patches.
- `preference.rs`: shared choice state machine.
- `launch_setup.rs` / `settings_tui.rs`: session/global and settings UI.
- `command/types.rs` / `command/builder.rs`: plan metadata and harness argv.
- `main.rs`: bootstrap/setup the effective harness and emit dry-run metadata.
- `codex_bridge.rs` / `foreground_runtime.rs`: phase-2 foreground transport,
  child overlay, spawn seam, and lifetime ownership.
- `codex_auth.rs`: clud-owned OAuth callback, PKCE, separate credential store,
  atomic refresh, and `codex-auth` command implementation.
- `codex_upstream.rs`: explicit subscription/API credential source selection.
- `daemon/entry.rs`: pin repeat-job provider/harness choices.
- `deepseek_auth.rs`: injectable `SecretStore`, native-vault adapter,
  `deepseek-auth` command implementation, and launch-time preflight
  (`preflight_native`, `launch_needs_preflight`, `launch_is_interactive`).
- `foreground_runtime.rs`: DeepSeek routing (`is_deepseek_via_claude`,
  `apply_deepseek_overlay`) alongside the Codex bridge routing above.

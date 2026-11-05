# Standalone API gateway

`clud serve --claude` runs a foreground, loopback-only Anthropic Messages API
for independent local clients. `--openrouter` adds the OpenRouter route using
`clud auth login openrouter`; `--deepseek` and `--kimi` add the other two
Anthropic-compatible registry providers through the credentials `clud auth
login deepseek` / `clud auth login kimi` stored (their `DEEPSEEK_API_KEY` /
`KIMI_API_KEY` environment variables remain the last source). Every selected
provider must hold a credential or startup fails with that provider's login
command as the remedy. `--port` chooses a port (default `8317`, or `0` to let
the OS choose). Startup prints the base URL. Ctrl+C closes client and
connected upstream sockets; DNS resolution or connection setup may delay exit.

The current Claude credential path takes an Anthropic Console API key from
`ANTHROPIC_API_KEY`. The caller authenticates separately with a random token
of at least 32 printable non-space ASCII characters in `CLUD_SERVE_CLIENT_TOKEN`.
The server does not print either credential. Use a different, unpredictable
client token for each operator or service. For example, configure the two
variables in your shell or secret manager, then run:

```sh
clud serve --claude --openrouter --deepseek --kimi
```

In another shell with `CLUD_SERVE_CLIENT_TOKEN` set to the same value:

```sh
curl -H "x-api-key: $CLUD_SERVE_CLIENT_TOKEN" http://127.0.0.1:8317/v1/models
curl http://127.0.0.1:8317/v1/messages \
  -H "x-api-key: $CLUD_SERVE_CLIENT_TOKEN" \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"CLAUDE_MODEL_ID","max_tokens":128,"messages":[{"role":"user","content":"Hello"}]}'
```

Choose a Claude model ID returned by `/v1/models`; the same list includes the
configured OpenRouter discovery IDs plus the DeepSeek and Kimi catalog rows
(`deepseek-flash`, `deepseek-v4-pro`, `kimi-k3`). Each row is routed to its own
provider with that provider's own key; a model ID from one route never reaches
another route's upstream. Messages requests accept streaming and
tools through the Anthropic protocol. Client `x-api-key` or bearer credentials
are removed before forwarding; the gateway separately resolves the selected
provider's upstream credential. Missing OpenRouter credentials make startup
fail with a remedy. The server does not provide OpenAI-compatible endpoints.

Claude's model inventory is fetched using its upstream credential; all pages
are collected before OpenRouter rows are appended. If discovery fails, the
server returns a sanitized gateway error instead of advertising an incomplete
list. The existing launch-scoped gateway remains documented in
[unified-gateway.md](unified-gateway.md).

## Codex

`clud serve --codex` serves Codex through the same Anthropic Messages API,
including streaming and tool calls. It requires no Claude credential and
launches no interactive harness. Authenticate the upstream first with
`clud auth login codex --acknowledge-experimental` (clud-managed ChatGPT
subscription), or set `OPENAI_API_KEY` for metered OpenAI API access. An
existing clud subscription record takes precedence; its refresh and failure
handling are the same as the interactive Codex bridge. A native `codex login`
is not automatically imported by this unattended server.

Set a separate random `CLUD_SERVE_CLIENT_TOKEN` as above, then run:

```sh
clud serve --codex
# Optional additional routes:
clud serve --codex --openrouter
clud serve --codex --claude
```

Use `/v1/models` to discover the advertised Codex IDs (for example,
`clud-claude-codex-sol`) and pass one as `model` to `/v1/messages`.
Codex-only discovery does not contact Anthropic. This is an Anthropic-compatible
frontend. Several client dialects are translated at the door and answered in the
shape they were asked in. All of them run over the same ladder, credentials and
allowlist as `/v1/messages`; nothing is duplicated and no route learns a second
protocol:

| Route | Client dialect |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions (`openai_ingress`) |
| `POST /v1/completions` | OpenAI legacy Completions (`completions_ingress`) |
| `POST /v1/responses` | OpenAI Responses (`responses_ingress`) |
| `POST /v1/responses/compact` | Responses compaction passthrough (`responses_compact`, Codex lane) |
| `POST /v1beta/models/<model>:generateContent` | Gemini `GenerateContentResponse` (`gemini_client_ingress`) |
| `POST /v1beta/models/<model>:streamGenerateContent` | Gemini SSE chunks |

The translated doors are exposed only when the launch has a unified gateway,
because the relay they share lives there.

## OpenAI Responses client bodies

`crates/clud-bin/src/responses_ingress.rs` translates one OpenAI Responses
(`POST /v1/responses`) request body into the Anthropic Messages body the routes
above already serve, ported from CLIProxyAPI
`internal/translator/claude/openai/responses/claude_openai-responses_request.go`.
It is reachable on the loopback gateway and on `clud serve` as
`POST /v1/responses`, behind the same `X-Clud-Gateway-Token` / client-token
contract as every other door, when the launch has a unified gateway; without one
the turn is refused `501` in the Responses error envelope. The hidden seam
`clud __openai-responses-translate` reads a request from stdin and writes the
Messages body. `instructions` plus every role=system/developer input item
become top-level `system` blocks in source order; `message` items become
user/assistant turns; `function_call`/`custom_tool_call` become `tool_use` and
their `*_output` partners `tool_result`; a `reasoning` item with
`encrypted_content` becomes a `thinking` block, and one without a signature keeps
its text as an assistant text block because a Claude `thinking` block without a
signature is invalid.

Deliberate differences from upstream: a builtin server tool (`web_search`,
`file_search`, `code_interpreter`, `image_generation`, `computer_use*`,
`local_shell`, `mcp`, `tool_search`) is refused by name (`UnsupportedPart`)
instead of silently dropped, because clud has no counterpart and removing a
capability quietly makes the model answer worse; an unknown input item type is
skipped with one stderr line (upstream logs and drops it); a `function_call_output`
that never paired with a call is carried as plain user text (Claude rejects an
orphan `tool_result`), and a `tool_use` with no result is closed with an
`is_error` result so an interrupted turn can be replayed; `text.format` /
`response_format` structured-output synthesis, tool-search discovery replay and
the namespace/custom-tool name map are not ported yet; `cache_control` markers
are not replayed.

## OpenAI Responses answers

The same module renders the other direction, ported from CLIProxyAPI
`internal/translator/claude/openai/responses/claude_openai-responses_response.go`:
`translate_responses_response` turns one buffered Messages body into a Responses
`response` object, and `ResponsesStreamTranslator` turns the Messages SSE frames
into `response.created` / `response.in_progress` / `response.output_item.*` /
`response.content_part.*` / `response.output_text.delta` /
`response.function_call_arguments.*` / `response.reasoning_summary_*` /
`response.completed` events, each carrying a monotonic `sequence_number`.
`thinking` becomes a `reasoning` item whose `encrypted_content` is the Claude
signature — the request half reads it back — and `redacted_thinking` rides in
`encrypted_content` behind upstream's `claude-redacted-thinking:` marker.
`max_tokens` / `model_context_window_exceeded` end the response `incomplete`
with `incomplete_details.reason = max_output_tokens`; a `refusal` stop reason is
`incomplete` with `content_filter`, because Responses clients treat it as a
retryable refusal rather than a finished answer; `pause_turn` stays `completed`,
since the client continues it by replaying the assistant turn.

Deliberate differences from upstream: a stream whose upstream body ends without
`message_stop` is sealed as `response.failed` with code `server_error` rather
than `response.completed` — a truncated answer is not a finished one; the
`web_search_call` item is not synthesised, because clud refuses server tools at
the door instead of injecting them; and the websocket verb upstream also serves
at `/v1/responses` is not ported, since this bridge is plain HTTP/1.1.

## Legacy text completions

`crates/clud-bin/src/completions_ingress.rs` serves OpenAI's older
`POST /v1/completions` contract, ported from CLIProxyAPI
`sdk/api/handlers/openai/openai_handlers.go` (`Completions`,
`convertCompletionsRequestToChatCompletions`,
`convertChatCompletionsResponseToCompletions`), which upstream exposes at
`internal/api/server_routes.go:74`. Upstream implements it as a thin adapter
over its chat-completions handler; clud does the same: the one-`prompt` body is
rewritten into the single-user-turn chat request and handed to
`openai_ingress::translate_chat_request`, and the Messages answer is rendered
back as `text_completion` (buffered) or `text_completion` chunks plus
`data: [DONE]` (streamed). Model resolution, routing, failover and effort
suffixes are unchanged. The hidden seam is
`clud __openai-completions-translate [--response [--stream]] [--for-model M]`.

Deliberate differences from upstream, all refusals by name rather than a
silently different answer: an empty prompt is refused (`MissingPrompt`) instead
of being served with upstream's invented `"Complete this:"` prefix; a token-id
prompt array is refused (`prompt:token_ids`) instead of collapsing to the empty
string gjson returns for an array; `prompt` as an array of strings is joined with
newlines; and `n > 1`, `echo: true`, `suffix`, `best_of`, `logprobs: true` and
`top_logprobs` are refused (`UnsupportedField`) because Anthropic Messages
returns one choice, cannot echo a prompt and reports no token
log-probabilities.

## Images

`clud serve --codex` also answers `POST /v1/images/generations` on the same
listener, behind the same client token. The body is OpenAI's images request;
`model` and `prompt` are required and the model must be one of `gpt-image-1`,
`gpt-image-1.5`, `gpt-image-2`, `gpt-image-2.5`, `gpt-image-2.5-flare`,
`gpt-image-2.5-sunburst` (a provider prefix such as `openai/` is stripped).
clud forwards the body to the Codex `/images/generations` endpoint and returns
the upstream's JSON envelope unchanged, so `b64_json`/`url` and the upstream's
own error body reach the client as-is.

`POST /v1/images/edits` sits beside it and takes OpenAI's multipart form:
`model`, `prompt` and at least one `image`/`image[]` file are required; a
`mask` file becomes `mask.image_url`, text fields are copied (with `n`,
`output_compression` and `partial_images` coerced to integers) and every upload
is rewritten to a `data:` URL under `images[]`, which is the JSON contract the
Codex endpoint accepts. A JSON edit body is forwarded like a generation.

`stream: true` on either door is forwarded as `true` and the upstream's SSE body
is relayed event by event (`text/event-stream`, chunked). Nothing is written
until the upstream's first byte lands, so a failure before that point is still a
clean OpenAI error envelope; after the head is out the response is committed and
an upstream failure only ends the stream. A non-2xx upstream answer is relayed
whole, never turned into a local error.

Deliberate limits: a launch without `--codex` answers 501 naming `--codex`,
because no other clud provider sells an image model; an edit without an uploaded
image is a local 400 rather than a paid upstream round-trip; and an image request
is never retried, because a retry is a second billed image the caller did not ask
for.

## MCP bridge inside `clud --claude`

Every Claude launch (except the Codex->Claude bridge) registers this same
server with Claude Code through one `--mcp-config=<json>` token, using the
venv `python` beside the `clud` binary (`command/mcp_config.rs`). The agent
can then delegate to Codex sub-agents with `mcp__clud__session_start`
(`backend="codex"`). A build with no sibling `python`, such as a dev binary
under `target/`, registers nothing.

## MCP discovery for Hermes

Install clud (the MCP SDK is a base dependency; no extra needed) and register
`python -m clud.mcp_server` as a stdio MCP server in Hermes. Its tool list now
includes `serve_start`, `serve_status`, and `serve_stop` alongside agent-session
tools. `serve_start` defaults to Codex; pass `backend="claude"` for Claude,
`openrouter=true` for the additional OpenRouter route, or `port` to choose a
loopback port. The default port is ephemeral for MCP-managed gateways.

`serve_start` and `serve_status` return the base URL, model-list and Messages
URLs, protocol (`anthropic_messages`), authentication header (`x-api-key`),
and a freshly generated **client** token. Keep that token private. Provider
credentials stay in the server environment or clud's credential store and
are never returned through MCP. Hermes must use an Anthropic-compatible
client to consume the endpoint; this does not expose OpenAI endpoints.

Identical repeated starts reuse the same gateway. To change configuration,
call `serve_stop` first. Startup has a 30-second deadline and returns an
error with credential setup guidance if it fails. The MCP server owns the
child process: disconnecting stops it; `serve_stop` also stops it explicitly.
`serve_status` reports only the gateway owned by this MCP server, not servers
started separately in a terminal. These controls do not require the clud daemon.

Anonymous HTTP requests use request-local history, so a long-running gateway
does not retain one conversation per independent request. Clients that send
explicit session identifiers retain the existing bounded conversation history.

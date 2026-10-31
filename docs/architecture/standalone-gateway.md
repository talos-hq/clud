# Standalone API gateway

`clud serve --claude` runs a foreground, loopback-only Anthropic Messages API
for independent local clients. `--openrouter` adds the OpenRouter route using
`clud auth login openrouter`; `--port` chooses a port (default `8317`, or `0`
to let the OS choose). Startup prints the base URL. Ctrl+C closes client and
connected upstream sockets; DNS resolution or connection setup may delay exit.

The current Claude credential path takes an Anthropic Console API key from
`ANTHROPIC_API_KEY`. The caller authenticates separately with a random token
of at least 32 printable non-space ASCII characters in `CLUD_SERVE_CLIENT_TOKEN`.
The server does not print either credential. Use a different, unpredictable
client token for each operator or service. For example, configure the two
variables in your shell or secret manager, then run:

```sh
clud serve --claude --openrouter
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
configured OpenRouter discovery IDs. Messages requests accept streaming and
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
frontend, `/v1/responses` is not exposed.

Two other client dialects are translated at the door and answered in the shape
they were asked in. Both run over the same ladder, credentials and allowlist as
`/v1/messages`; nothing is duplicated and no route learns a second protocol:

| Route | Client dialect |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions (`openai_ingress`) |
| `POST /v1beta/models/<model>:generateContent` | Gemini `GenerateContentResponse` (`gemini_client_ingress`) |
| `POST /v1beta/models/<model>:streamGenerateContent` | Gemini SSE chunks |

The Gemini verb lives in the path, not the body, so `<model>` is the clud model
to route to (`clud-claude-*`, `@effort` and `(budget)` suffixes included) — clud
has no Gemini provider, so a Google model id is only routable when a catalog row
maps it. Refusals on these doors keep the HTTP status and use that client's own
error envelope.

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

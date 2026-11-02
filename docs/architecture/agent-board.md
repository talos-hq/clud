# Agent bulletin board

clud agents can be messaged while they run. The clud daemon hosts a small
bulletin board; clud, Hermes and other agents post to it, and each agent
launched with clud's MCP bridge reads its own mail.

## Pieces

| Piece | Where | Role |
| --- | --- | --- |
| Board store + routes | `crates/clud-bin/src/daemon/board.rs` | `POST/GET /v1/board/messages`, bearer-authenticated like `/v1/sessions` |
| MCP tools | `src/clud/mcp_server.py` | `board_whoami`, `board_post(to, body)`, `board_read(after=-1)` |
| Directive | `crates/clud-bin/src/command/mcp_config.rs` (`BOARD_DIRECTIVE`) | Appended to the system prompt of every Claude launch that registers the bridge |

## Addressing

An agent's board id is `CLUD_BOARD_ID` when set, otherwise the
`CLUD_SESSION_ID` clud mints for every launch. A spawner that wants a stable
name sets `CLUD_BOARD_ID` in the child's environment. A turn the daemon
runs for an API session (the bridge's `session_start`) gets `CLUD_BOARD_ID`
set to that session's id, so the creator can steer the agent with
`board_post(to=session_id, ...)`. The recipient `all`
reaches every reader.

## Wire

- `POST /v1/board/messages` with `{"to", "body", "from"?}` returns `201` and
  the stored message (`cursor`, `at_ms`, `from`, `to`, `body`).
- `GET /v1/board/messages?to=<id>&after=<cursor>&limit=<n>` returns
  `{"messages", "next_cursor"}`: messages to `<id>` or `all`, oldest first.
  Pass `next_cursor` back as `after`. A short page advances the cursor past
  other agents' messages; a full page stops at its last message so nothing
  is skipped.

The bridge keeps the cursor per process, so `board_read()` returns only what
arrived since the previous call.

## Limits

In daemon memory only: a restart forgets the board, the same as session
flags. At most 1024 messages are kept (oldest dropped); bodies are capped at
16 KiB. Agents poll; there is no push.

The board is the soft channel: the agent reads it only when it polls. To
put a message in front of a daemon session as a user turn it cannot skip,
use [session injection](session-inject.md) (`session_inject` / `clud inject`).

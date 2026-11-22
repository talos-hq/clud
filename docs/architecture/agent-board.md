# Agent bulletin board

clud agents can be messaged while they run. The clud daemon hosts a small
bulletin board; clud, Hermes and other agents post to it, and each agent
launched with clud's MCP bridge reads its own mail.

## Pieces

| Piece | Where | Role |
| --- | --- | --- |
| Board store + routes | `crates/clud-bin/src/daemon/board.rs` | `POST/GET /v1/board/messages`, bearer-authenticated like `/v1/sessions` |
| Agent directory | `crates/clud-bin/src/daemon/board.rs` (`Agents`) | `GET/POST /v1/board/agents`, `GET /v1/board/agents/<id>`: one card per agent |
| MCP tools | `src/clud/mcp_server.py` | `board_whoami`, `board_post(to, body)`, `board_read(after=-1)`, `board_status(working_on)`, `board_agents(parent?)`; each takes an optional `agent_id` |
| Subagent identity | `crates/clud-bin/src/board_subagent.rs` | `SubagentStart` hook: mints a Claude Code subagent's board id and hands it over as context |
| Directive | `crates/clud-bin/src/command/mcp_config.rs` (`BOARD_DIRECTIVE`) | Appended to the system prompt of every Claude launch that registers the bridge, and to `developer_instructions` of every Codex launch |
| Codex registration | `mcp_config::codex_bridge_overrides` | The same server entry as `-c mcp_servers.clud.{command,args,env}` overrides, so codex agents (including `session_start` sub-agents) get the board tools |
| Registration | `crates/clud-bin/src/command/mcp_config.rs` | A wheel install uses the `python` beside `clud`. A native install (`install.sh`) unpacks the bridge sources embedded in the binary to `~/.clud/state/mcp-bridge/<version>/` and runs them through `uv run --with mcp`; with no `uv` the launch prints how to get one and registers nothing |

## Addressing

An agent's board id is `CLUD_BOARD_ID` when set, otherwise the
`CLUD_SESSION_ID` clud mints for every launch. A spawner that wants a stable
name sets `CLUD_BOARD_ID` in the child's environment. A turn the daemon
runs for an API session (the bridge's `session_start`) gets `CLUD_BOARD_ID`
set to that session's id, so the creator can steer the agent with
`board_post(to=session_id, ...)`. The recipient `all`
reaches every reader.

## Sub-agents and the agent directory

Every agent can file a **card**: `id`, `parent` (the board id of the agent
that spawned it), `cwd`, `kind`, `repo` (GitHub `owner/repo`), `branch` and
`working_on`, the "what I'm working on now" slot. `board_agents(parent=<id>)`
lists one agent's children, so a main agent sees which sub-agent works in which
repo, branch and directory, and on what.

Two kinds of sub-agent get a pre-populated identity:

- **Daemon sessions** (`session_start`, e.g. a codex sub-agent). The bridge
  sends its own board id as `board_parent` when it creates the session; the
  daemon files the card at once (id = session id, parent, cwd, backend, repo
  slug and branch from the cwd's git checkout) and runs each turn with
  `CLUD_BOARD_ID=<session id>` and `CLUD_BOARD_PARENT=<parent>`. The sub-agent's
  own bridge then speaks as itself, and `board_status` keeps the parent.
- **Claude Code subagents** (the Agent tool). They share the parent's MCP
  connection, so the bridge cannot tell them apart. clud's `SubagentStart`
  hook mints `<parent>/<agent_type>-<agent id prefix>` and gives it to the
  subagent as context with its cwd and parent; the subagent passes it as
  `agent_id` to every board tool. The bridge keeps a separate read cursor per
  `agent_id`, and `board_status(agent_id=...)` files the card under the
  bridge's own id. The hook line rides the same client-version gate as the
  worktree hooks.

The bridge reads `repo`/`branch` from `.git` files directly (no git process).
Cards live in daemon memory like messages; at most 512 are kept, the least
recently updated dropped first.

`tests/integration/test_board_subagent_a2a.py` demonstrates the round trip:
the real bridge as the main agent, `session_start(backend="codex")`, and the
mock agent (`MOCK_AGENT_BOARD=1`) as a board-aware codex sub-agent.

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

A fresh process starts at cursor zero, which re-delivered the whole backlog:
measured 2026-11-02, 38-39 unrelated progress broadcasts (~26 KB) in every new
session. So the bridge's **first** read keeps every message addressed to that
agent — the steering channel is never capped — plus the newest
`CLUD_BOARD_FIRST_READ_BROADCASTS` broadcasts (default 8). Withheld broadcasts
are reported as `skipped_broadcasts` with a `note`; they stay on the board and
`board_read(after=0)` replays them. Set the variable to a negative value to
replay the full backlog on the first read (the pre-cap behaviour); `0` means
"no broadcasts on the first read". Board pages are serialized without
indentation — they are parsed, not read (10.2% measured on a 39-message page).

## Limits

In daemon memory only: a restart forgets the board, the same as session
flags. At most 1024 messages are kept (oldest dropped); bodies are capped at
16 KiB. There is no push, but `board_read(wait_seconds=N)` long-polls: the
bridge re-reads the page once a second and returns the moment a message
arrives, so an idle supervisor spends one turn per N seconds instead of one
turn per poll. Measured 2026-11-08: 173 of 352 `board_read` calls in 24 h
returned `messages: []`, each one a full model turn. The wait defaults to 0
(return immediately, the old behaviour) and is capped at 540 s, below the
client's tool-call deadline; an empty result carries `next_wait_seconds` so
the caller re-arms with a wait instead of spinning.

The board is the soft channel: the agent reads it only when it polls. To
put a message in front of a daemon session as a user turn it cannot skip,
use [session injection](session-inject.md) (`session_inject` / `clud inject`).

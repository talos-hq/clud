# Session injection: steering a running daemon session

`session_inject` (MCP) and `clud inject` (CLI) deliver an operator's message
into an already-running, daemon-owned logical session (`sess-...`, created
by `session_start` or `POST /v1/sessions`). The agent sees the message as a
new **user turn**. Before this existed, a headless run could not be steered:
text appended to a brief after launch never reached the agent, and the
operator could not tell whether it had been seen.

The storage model this builds on is in
[api-session-storage.md](api-session-storage.md); the MCP bridge is
`src/clud/mcp_server.py`; the bulletin board
([agent-board.md](agent-board.md)) stays the soft, poll-based channel.
Injection is the hard channel: a delivered injection is a turn the agent
cannot skip.

## 1. Delivery mechanism per harness

A daemon session runs one provider process per turn: the first turn starts
the harness headless, and every later turn resumes the same provider
conversation (`claude -p --resume <id>`, `codex exec resume <thread>`). The
daemon never keeps a long-lived stdin to the harness. Claude Code's
`--input-format stream-json` stdin is therefore **not** used. Turn-boundary
resume gives the same agent-visible result: the injected text arrives as
the next user message in the same conversation. It works the same way for
both harnesses, and it survives a daemon restart, because the queue is part
of the durable session record rather than a pipe.

| Harness | Queued (next boundary) | `interrupt: true` | Mechanism |
|---|---|---|---|
| Claude Code | supported | supported | `--resume <session_id>` turn; interrupt = graceful SIGINT, grace, kill, then the resume turn |
| Codex | supported | supported | `codex exec resume <thread_id>` turn; same interrupt path |
| DeepSeek Harness (`harness: deepseek`) | **not supported** | **not supported** | daemon headless turns do not support it; inject fails closed with `harness_not_supported` naming `deepseek` |
| Interactive PTY sessions (`clud` / `clud do` in a terminal) | **not supported** | **not supported** | not daemon API sessions, so the id is unknown (`404 not_found`). Use the bulletin board, which those agents poll |
| Foreground `clud -p` (no daemon) | **not supported** | **not supported** | no daemon owns the process. Launch through `session_start` to make a run injectable |

Codex's app-server turn API could deliver mid-turn without a restart. We
don't use it: resume-based delivery covers both harnesses with one code
path, and the app-server is not the transport clud runs Codex under.

## 2. Semantics

* An injection is always a **user turn**. It is never a system prompt,
  never an override, and never spliced into the in-flight turn.
* The default is **queued at the next boundary**. If a turn is running, the
  message waits until that turn seals, then becomes the next turn. If the
  session is idle (or `starting`, or its last turn failed), the message is
  delivered at once.
* `interrupt: true` is opt-in. The running turn is interrupted (same path
  as `POST /v1/sessions/{id}/interrupt` with `interrupt_running`). The
  injected message starts right away as the resume turn. Older queued
  injections stay queued and follow it in `seq` order.
* Ordering: queued injections are delivered one per turn, lowest `seq`
  first.

## 3. Durability: nothing is silently dropped

Every injection is recorded on the session record **before** delivery is
attempted. The result is always `{accepted, state, seq}`:

| state | meaning |
|---|---|
| `queued` | recorded; waits for the running turn to seal |
| `delivered` | a turn carrying the message was started; `turn_id` names it |
| `failed` | delivery was attempted and could not start a turn; `error` says why (it is never left queued forever without a reason) |

* Session `terminated` (sealed): rejected, `409 session_sealed`, not recorded.
* Unsupported harness: rejected, `422 harness_not_supported`, with the harness named.
* Idempotent on `client_msg_id`. A repeat with the same id and message
  returns the original `seq`/`state` with `duplicate: true`, and is never
  delivered twice. The same id with a different message is `409
  injection_conflict`. Delivery itself submits the turn under the
  idempotency key `inject-<seq>`, so two racing deliverers (the inject
  call and the end-of-turn drain) start at most one turn.
* Race between "queue" and "turn ends": the message is persisted before the
  session state is read. The end-of-turn drain reads the queue after the
  turn seals. Whichever runs second sees the other's write, so a message
  cannot fall between them.

## 4. Observability

The session record (`GET /v1/sessions/{id}`) carries `injections[]`. Each
entry has `seq`, `client_msg_id`, `message`, `from` (who injected:
`CLUD_BOARD_ID`/session id for agents, `--from` for humans), `ticket`
(optional issue/ticket reference), `interrupt`, `state`,
`submitted_at_ms`, `delivered_at_ms`, `turn_id` and `error`. The event log
also gets `injection_queued` / `injection_delivered` / `injection_failed`
events, so a later reader can see exactly when and how the run was steered.

## 5. Surfaces

* HTTP: `POST /v1/sessions/{id}/inject` with
  `{message, client_msg_id?, interrupt?, from?, ticket?}`. Returns `202`
  with `{accepted, state, seq, session_id, turn_id?, duplicate}`.
* MCP: `session_inject {session_id, message, interrupt?, client_msg_id?, ticket?}`.
  It returns the same object. When `client_msg_id` is omitted, the bridge
  derives a stable one from the session id and message, so a client retry
  of the same call stays idempotent.
* CLI: `clud inject <session> "<message>" [--interrupt] [--client-msg-id ID]
  [--from WHO] [--ticket REF] [--detach] [--timeout SECS] [--json]`. It
  never prompts. By default it blocks until the injection is `delivered`
  (or `failed`), up to `--timeout`. With `--detach` it returns as soon as
  the injection is accepted.

## Why injection targets a session id, not a checkout

clud refuses a second agent on a claimed checkout, and one checkout can
hold several historical sessions. The session id is the only unambiguous
handle for "the run I launched", and it is already the board address
`session_start` returns.
